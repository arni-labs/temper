//! Aggregate-bounded field-overflow hydration.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use serde_json::Value;
use sha2::{Digest as _, Sha256};
use temper_runtime::tenant::TenantId;
use temper_wasm::FieldBlobReader;

use super::read_source::{BlobReadSource, read_blob_ref_bytes};
use super::{field_overflow_descriptor, field_overflow_sha256};
#[cfg(test)]
use crate::blob_store::BlobStore;
use crate::blob_store::{BlobReadBounded, hex_lower};
use crate::state::ServerState;

const GENERIC_INLINE_HYDRATION_BUDGET_BYTES: usize = 1024 * 1024;
const GENERIC_MAX_INLINE_FIELD_BYTES: usize = 128 * 1024;
/// What one WASM call may read of a record's large (overflowed) fields: 16 MiB in all, and so also the most one field
/// may be. It is the one limit on the size of what a module reads and of what a record write takes: the largest
/// `/tdata` body is this too (router.rs), so no single write can carry a field larger than a module may read. The
/// 16 MiB is shared by the large fields the module reads in one call, charged as it reads each one
/// (`WasmFieldBlobReader`); a field it never reads takes nothing from it.
///
/// The reason for the number is memory. A module reads a field into a buffer of the field's size in its own linear
/// memory, which is 64 MiB by default (`temper_wasm::WasmResourceLimits::max_memory`); 16 MiB is a quarter of it,
/// leaving the rest for the module's code, its invocation context and a decoded copy of what it read. The host holds
/// the same bytes in the call's blob cache until the call ends, so this also bounds the host memory one call takes.
pub(crate) const WASM_FIELD_READ_BUDGET_BYTES: usize = 16 * 1024 * 1024;
const MAX_BLOB_REFS_PER_VALUE: usize = 1024;
const MAX_BLOB_READ_ATTEMPTS_PER_RESPONSE: usize = 64;

#[derive(Clone, Debug)]
pub(crate) struct BlobHydrationBudget {
    inner: Arc<Mutex<BlobHydrationBudgetState>>,
    max_inline_field_bytes: usize,
    max_deferred_field_bytes: usize,
}

#[derive(Debug)]
struct BlobHydrationBudgetState {
    inline_remaining: usize,
    deferred_remaining: usize,
    read_attempts_remaining: usize,
    failed_keys: BTreeSet<String>,
}

#[derive(Clone, Copy, Debug)]
enum HydrationBudgetKind {
    Inline,
    Deferred,
}

#[derive(Debug)]
struct HydrationReservation {
    budget: BlobHydrationBudget,
    kind: HydrationBudgetKind,
    reserved: usize,
    committed: bool,
}

impl BlobHydrationBudget {
    pub(crate) fn generic_response() -> Self {
        Self::new(
            GENERIC_INLINE_HYDRATION_BUDGET_BYTES,
            GENERIC_MAX_INLINE_FIELD_BYTES,
            0,
            0,
        )
    }

    pub(crate) fn wasm_dispatch() -> Self {
        Self::new(
            GENERIC_INLINE_HYDRATION_BUDGET_BYTES,
            GENERIC_MAX_INLINE_FIELD_BYTES,
            WASM_FIELD_READ_BUDGET_BYTES,
            WASM_FIELD_READ_BUDGET_BYTES,
        )
    }

    pub(crate) fn new(
        inline_bytes: usize,
        max_inline_field_bytes: usize,
        deferred_bytes: usize,
        max_deferred_field_bytes: usize,
    ) -> Self {
        Self {
            inner: Arc::new(Mutex::new(BlobHydrationBudgetState {
                inline_remaining: inline_bytes,
                deferred_remaining: deferred_bytes,
                read_attempts_remaining: MAX_BLOB_READ_ATTEMPTS_PER_RESPONSE,
                failed_keys: BTreeSet::new(),
            })),
            max_inline_field_bytes,
            max_deferred_field_bytes,
        }
    }

    fn try_reserve_inline(&self, declared_size: usize) -> Option<HydrationReservation> {
        self.try_reserve(
            HydrationBudgetKind::Inline,
            declared_size,
            self.max_inline_field_bytes,
        )
    }

    fn try_reserve_deferred(&self, declared_size: usize) -> Option<HydrationReservation> {
        self.try_reserve(
            HydrationBudgetKind::Deferred,
            declared_size,
            self.max_deferred_field_bytes,
        )
    }

    fn try_reserve(
        &self,
        kind: HydrationBudgetKind,
        requested: usize,
        max_field_bytes: usize,
    ) -> Option<HydrationReservation> {
        if requested == 0 || requested > max_field_bytes {
            return None;
        }
        let mut state = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let remaining = match kind {
            HydrationBudgetKind::Inline => &mut state.inline_remaining,
            HydrationBudgetKind::Deferred => &mut state.deferred_remaining,
        };
        if requested > *remaining {
            return None;
        }
        *remaining -= requested;
        Some(HydrationReservation {
            budget: self.clone(),
            kind,
            reserved: requested,
            committed: false,
        })
    }

    #[cfg(test)]
    pub(super) fn remaining(&self) -> (usize, usize) {
        let state = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        (state.inline_remaining, state.deferred_remaining)
    }

    fn refund(&self, kind: HydrationBudgetKind, bytes: usize) {
        let mut state = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let remaining = match kind {
            HydrationBudgetKind::Inline => &mut state.inline_remaining,
            HydrationBudgetKind::Deferred => &mut state.deferred_remaining,
        };
        *remaining = remaining.saturating_add(bytes);
    }

    fn try_begin_read(&self) -> bool {
        let mut state = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        if state.read_attempts_remaining == 0 {
            return false;
        }
        state.read_attempts_remaining -= 1;
        true
    }

    fn is_known_failed(&self, key: &str) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .failed_keys
            .contains(key)
    }

    fn mark_failed(&self, key: &str) {
        self.inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .failed_keys
            .insert(key.to_string());
    }

    #[cfg(test)]
    pub(crate) fn read_attempts_remaining(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .read_attempts_remaining
    }
}

impl HydrationReservation {
    fn max_bytes(&self) -> usize {
        self.reserved
    }

    fn commit(mut self, actual_bytes: usize) {
        debug_assert!(actual_bytes <= self.reserved);
        let refund = self.reserved.saturating_sub(actual_bytes);
        if refund > 0 {
            self.budget.refund(self.kind, refund);
        }
        self.committed = true;
    }
}

impl Drop for HydrationReservation {
    fn drop(&mut self) {
        if !self.committed {
            self.budget.refund(self.kind, self.reserved);
        }
    }
}

fn collect_blob_ref_pointers(value: &Value, pointer: &str, out: &mut Vec<String>) {
    if out.len() >= MAX_BLOB_REFS_PER_VALUE {
        return;
    }
    if field_overflow_descriptor(value).is_some() {
        out.push(pointer.to_string());
        return;
    }

    match value {
        Value::Array(values) => {
            for (index, child) in values.iter().enumerate() {
                collect_blob_ref_pointers(child, &format!("{pointer}/{index}"), out);
            }
        }
        Value::Object(map) => {
            for (key, child) in map {
                let escaped = key.replace('~', "~0").replace('/', "~1");
                collect_blob_ref_pointers(child, &format!("{pointer}/{escaped}"), out);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

/// Verify stored bytes against their content address.
pub(super) fn blob_bytes_match_key(key: &str, bytes: &[u8]) -> bool {
    let Some(expected) = field_overflow_sha256(key) else {
        return false;
    };
    hex_lower(&Sha256::digest(bytes)) == expected
}

#[cfg(test)]
pub(crate) async fn hydrate_blob_refs_in_value(store: &BlobStore, value: &mut Value) {
    let budget = BlobHydrationBudget::new(16 * 1024 * 1024, 16 * 1024 * 1024, 0, 0);
    hydrate_blob_refs_with_source(&BlobReadSource::Store(store), value, &budget).await;
}

/// Hydrate refs below `max_inline_bytes`; larger ones stay references for a module to read on request.
#[cfg(test)]
pub(crate) async fn hydrate_blob_refs_in_value_with_ceiling(
    store: &BlobStore,
    value: &mut Value,
    max_inline_bytes: usize,
) {
    let budget = BlobHydrationBudget::new(max_inline_bytes, max_inline_bytes, 0, 0);
    hydrate_blob_refs_with_source(&BlobReadSource::Store(store), value, &budget).await;
}

pub(super) async fn hydrate_blob_refs_with_source(
    source: &BlobReadSource<'_>,
    value: &mut Value,
    budget: &BlobHydrationBudget,
) {
    let mut pointers = Vec::new();
    collect_blob_ref_pointers(value, "", &mut pointers);
    pointers.sort();

    for pointer in pointers {
        let Some((key, declared_size)) = (|| {
            let slot = if pointer.is_empty() {
                Some(&*value)
            } else {
                value.pointer(&pointer)
            }?;
            let descriptor = field_overflow_descriptor(slot)?;
            let size = usize::try_from(descriptor.serialized_bytes).ok()?;
            Some((descriptor.key.to_owned(), size))
        })() else {
            continue;
        };
        if budget.is_known_failed(&key) {
            continue;
        }

        // A field too large to inline stays a reference in the context; the module reads it on request
        // (`WasmFieldBlobReader`).
        let Some(reservation) = budget.try_reserve_inline(declared_size) else {
            continue;
        };
        if !budget.try_begin_read() {
            continue;
        }
        match read_blob_ref_bytes(source, &key, reservation.max_bytes()).await {
            Ok(BlobReadBounded::Found(bytes)) => {
                if bytes.len() != declared_size {
                    tracing::warn!(%key, actual_bytes = bytes.len(), declared_size, "field-overflow blob length did not match its descriptor");
                    budget.mark_failed(&key);
                    continue;
                }
                if !blob_bytes_match_key(&key, &bytes) {
                    tracing::warn!(%key, "field-overflow blob failed SHA-256 verification");
                    budget.mark_failed(&key);
                    continue;
                }
                match serde_json::from_slice::<Value>(&bytes) {
                    Ok(restored) => {
                        reservation.commit(bytes.len());
                        if pointer.is_empty() {
                            *value = restored;
                        } else if let Some(slot) = value.pointer_mut(&pointer) {
                            *slot = restored;
                        }
                    }
                    Err(error) => {
                        tracing::warn!(%key, %error, "failed to decode hydrated field-overflow blob");
                        budget.mark_failed(&key);
                    }
                }
            }
            Ok(BlobReadBounded::Missing) => {
                tracing::warn!(%key, "field-overflow blob missing during hydration");
                budget.mark_failed(&key);
            }
            Ok(BlobReadBounded::TooLarge { actual_bytes }) => {
                tracing::warn!(%key, ?actual_bytes, "field-overflow blob exceeded inline hydration reservation");
                budget.mark_failed(&key);
            }
            Err(error) => {
                tracing::warn!(%key, %error, "failed to hydrate field-overflow blob");
                budget.mark_failed(&key);
            }
        }
    }
}

pub(crate) async fn hydrate_blob_refs_for_tenant(
    state: &ServerState,
    tenant: &TenantId,
    value: &mut Value,
) {
    let budget = BlobHydrationBudget::generic_response();
    hydrate_blob_refs_for_tenant_with_budget(state, tenant, value, &budget).await;
}

pub(crate) async fn hydrate_blob_refs_for_tenant_with_budget(
    state: &ServerState,
    tenant: &TenantId,
    value: &mut Value,
    budget: &BlobHydrationBudget,
) {
    hydrate_blob_refs_with_source(&BlobReadSource::Tenant { state, tenant }, value, budget).await;
}

/// Where a `WasmFieldBlobReader` reads from. It outlives the dispatch that built it, so it owns what it reads with.
#[cfg_attr(
    test,
    expect(
        clippy::large_enum_variant,
        reason = "the small Store variant exists only in tests"
    )
)]
enum FieldBlobSource {
    Tenant {
        state: ServerState,
        tenant: TenantId,
    },
    #[cfg(test)]
    Store(BlobStore),
}

/// A WASM call's reads of a record's overflowed fields. `host_read_field` asks for a field the first time the module
/// reads it, and each read is charged to the call's `WASM_FIELD_READ_BUDGET_BYTES`: the deferred part of the same
/// budget the call's inline hydration took from. A field the module never reads is never fetched.
pub(crate) struct WasmFieldBlobReader {
    source: FieldBlobSource,
    budget: BlobHydrationBudget,
}

impl WasmFieldBlobReader {
    /// Reads `tenant`'s overflowed fields, charging `budget`.
    pub(crate) fn for_tenant(
        state: ServerState,
        tenant: TenantId,
        budget: BlobHydrationBudget,
    ) -> Self {
        Self {
            source: FieldBlobSource::Tenant { state, tenant },
            budget,
        }
    }

    #[cfg(test)]
    pub(crate) fn for_store(store: BlobStore, budget: BlobHydrationBudget) -> Self {
        Self {
            source: FieldBlobSource::Store(store),
            budget,
        }
    }
}

#[async_trait::async_trait]
impl FieldBlobReader for WasmFieldBlobReader {
    async fn read_field_blob(&self, reference: &Value) -> Option<Vec<u8>> {
        let source = match &self.source {
            FieldBlobSource::Tenant { state, tenant } => BlobReadSource::Tenant { state, tenant },
            #[cfg(test)]
            FieldBlobSource::Store(store) => BlobReadSource::Store(store),
        };
        read_field_on_request(&source, reference, &self.budget).await
    }
}

/// Read one overflowed field a module asked for, within what is left of the call's budget.
async fn read_field_on_request(
    source: &BlobReadSource<'_>,
    reference: &Value,
    budget: &BlobHydrationBudget,
) -> Option<Vec<u8>> {
    let descriptor = field_overflow_descriptor(reference)?;
    let key = descriptor.key;
    let declared_size = usize::try_from(descriptor.serialized_bytes).ok()?;
    if budget.is_known_failed(key) {
        return None;
    }
    let Some(reservation) = budget.try_reserve_deferred(declared_size) else {
        tracing::warn!(%key, declared_size, "overflowed field exceeds what is left of the call's read budget");
        return None;
    };
    if !budget.try_begin_read() {
        return None;
    }
    match read_blob_ref_bytes(source, key, reservation.max_bytes()).await {
        Ok(BlobReadBounded::Found(bytes)) => {
            if bytes.len() != declared_size {
                tracing::warn!(%key, actual_bytes = bytes.len(), declared_size, "overflowed field length did not match its descriptor");
            } else if !blob_bytes_match_key(key, &bytes) {
                tracing::warn!(%key, "overflowed field failed SHA-256 verification");
            } else {
                reservation.commit(bytes.len());
                return Some(bytes);
            }
        }
        Ok(BlobReadBounded::Missing) => tracing::warn!(%key, "overflowed field missing"),
        Ok(BlobReadBounded::TooLarge { actual_bytes }) => {
            tracing::warn!(%key, ?actual_bytes, "overflowed field exceeded its read reservation")
        }
        Err(error) => tracing::warn!(%key, %error, "failed to read overflowed field"),
    }
    budget.mark_failed(key);
    None
}

/// Resolve only a caller-selected set of comparison fields. Unavailable or
/// invalid referenced bytes cannot turn an equality check into inequality.
pub(crate) async fn hydrate_comparison_fields(
    source: &BlobReadSource<'_>,
    fields: &mut Value,
) -> Result<(), String> {
    let mut pointers = Vec::new();
    collect_blob_ref_pointers(fields, "", &mut pointers);
    if pointers.is_empty() {
        return Ok(());
    }
    let budget = BlobHydrationBudget::new(
        WASM_FIELD_READ_BUDGET_BYTES,
        WASM_FIELD_READ_BUDGET_BYTES,
        0,
        0,
    );
    hydrate_blob_refs_with_source(source, fields, &budget).await;
    for pointer in pointers {
        if fields
            .pointer(&pointer)
            .is_some_and(|value| field_overflow_descriptor(value).is_some())
        {
            return Err("Comparison field blob could not be resolved and verified".to_string());
        }
    }
    Ok(())
}
