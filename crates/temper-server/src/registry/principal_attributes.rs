//! The Cedar attributes a tenant's principal-record types put on the
//! principal, and the refusal of a tenant where two of them share one.
//!
//! A `principal_record` type `T` sets `principal.T` (its status) and, with a
//! `principal_record_set` field `f`, `principal.T_f` (the set); an agent's user
//! gets the same names behind `actingFor`. Type `A` with field `b_c` and type
//! `A_b` with field `c` would both set `principal.A_b_c`, and a type named
//! `Role_roles` would set the attribute that type `Role`'s `roles` set sets.
//! Either value could then be the one a policy reads, so the registry refuses
//! such a tenant at install, naming both types, and a request never meets it.
//!
//! The `actingFor` forms clash exactly when the in-person forms do: they add
//! one prefix to both, and an in-person name starts uppercase while an
//! `actingFor` name starts lowercase, so the in-person names are checked.

use std::collections::BTreeMap;

use temper_spec::automaton::AutomatonMeta;

use super::types::RegistryError;

/// Refuse `types`, every type the tenant would hold after an install, when
/// two principal-record types reach one principal attribute.
pub(super) fn refuse_shared_principal_attributes<'a>(
    tenant: &str,
    types: impl IntoIterator<Item = (&'a str, &'a AutomatonMeta)>,
) -> Result<(), RegistryError> {
    let mut owners: BTreeMap<String, &str> = BTreeMap::new();
    for (entity_type, meta) in types {
        if !meta.principal_record {
            continue;
        }
        let set = meta
            .principal_record_set
            .as_ref()
            .map(|field| format!("{entity_type}_{field}"));
        for attribute in std::iter::once(entity_type.to_string()).chain(set) {
            if let Some(first) = owners.insert(attribute.clone(), entity_type) {
                return Err(RegistryError::PrincipalAttributeShared {
                    tenant: tenant.to_string(),
                    first: first.to_string(),
                    second: entity_type.to_string(),
                    attribute,
                });
            }
        }
    }
    Ok(())
}
