//! Appending to several entity journals in one atomic step.
//!
//! One Lua script checks every journal's expected sequence before it writes
//! any event. Redis runs a script without interleaving other commands, but it
//! does not undo the writes a script made before an error, so the checks all
//! come first: either every journal moves, or none does.

use std::collections::BTreeSet;

use temper_runtime::persistence::{
    PersistenceAppend, PersistenceAppendResult, PersistenceError, storage_error,
};
use temper_runtime::tenant::parse_persistence_id_parts;

use super::{EntityRef, RedisEventStore};

/// Lua script for an atomic append to several journals.
///
/// For the `a`th append (1-based): KEYS[3a-2] = seq_key, KEYS[3a-1] =
/// events_key, KEYS[3a] = entities_key. ARGV holds, for each append in
/// order: expected_seq, entity_ref_json, event_count, then that many
/// serialized events.
///
/// Returns `{1, new_seq_1, ..., new_seq_n}` on success, or `{0, a, current}`
/// when the `a`th journal is not at its expected sequence; nothing is
/// written then.
pub(super) const BATCH_APPEND_LUA: &str = r#"
local appends = #KEYS / 3
local cursor = 1
for a = 1, appends do
    local expected = tonumber(ARGV[cursor])
    local count = tonumber(ARGV[cursor + 2])
    local current = tonumber(redis.call('GET', KEYS[3 * a - 2]) or '0')
    if current ~= expected then
        return {0, a, current}
    end
    cursor = cursor + 3 + count
end
local result = {1}
cursor = 1
for a = 1, appends do
    local expected = tonumber(ARGV[cursor])
    local entity_ref = ARGV[cursor + 1]
    local count = tonumber(ARGV[cursor + 2])
    for i = 1, count do
        redis.call('RPUSH', KEYS[3 * a - 1], ARGV[cursor + 2 + i])
    end
    redis.call('SET', KEYS[3 * a - 2], tostring(expected + count))
    redis.call('SADD', KEYS[3 * a], entity_ref)
    result[#result + 1] = expected + count
    cursor = cursor + 3 + count
end
return result
"#;

impl RedisEventStore {
    /// Append each of `appends` to its journal, all or none: the script
    /// refuses the whole batch when any journal is not at its expected
    /// sequence.
    ///
    /// Each journal appears once. The script checks every expected sequence
    /// before it writes, and a second append to the same journal would be
    /// checked against the sequence the first one moves it from.
    pub(super) async fn append_journals(
        &self,
        appends: &[PersistenceAppend],
    ) -> Result<Vec<PersistenceAppendResult>, PersistenceError> {
        let mut journals = BTreeSet::new();
        let mut keys = Vec::with_capacity(appends.len() * 3);
        let mut args: Vec<String> = Vec::new();
        let mut entities = Vec::with_capacity(appends.len());
        for append in appends {
            if !journals.insert(append.persistence_id.as_str()) {
                return Err(PersistenceError::Storage(format!(
                    "a batch appends to each journal once; {} appears twice",
                    append.persistence_id
                )));
            }
            let (tenant, entity_type, entity_id) =
                parse_persistence_id_parts(&append.persistence_id)
                    .map_err(PersistenceError::Storage)?;
            keys.push(Self::seq_key(tenant, entity_type, entity_id));
            keys.push(Self::events_key(tenant, entity_type, entity_id));
            keys.push(Self::tenant_entities_key(tenant));

            let entity_ref = EntityRef {
                entity_type: entity_type.to_string(),
                entity_id: entity_id.to_string(),
            };
            args.push(append.expected_sequence.to_string());
            args.push(
                serde_json::to_string(&entity_ref)
                    .map_err(|e| PersistenceError::Serialization(e.to_string()))?,
            );
            args.push(append.events.len().to_string());
            let mut seq = append.expected_sequence;
            for event in &append.events {
                seq += 1;
                let mut envelope = event.clone();
                envelope.sequence_nr = seq;
                args.push(
                    serde_json::to_string(&envelope)
                        .map_err(|e| PersistenceError::Serialization(e.to_string()))?,
                );
            }
            entities.push((tenant, entity_type, entity_id));
        }

        let result: Vec<i64> = self
            .batch_append_script
            .evalsha_with_reload(&self.client, keys, args)
            .await
            .map_err(storage_error)?;
        match result.as_slice() {
            [1, new_seqs @ ..] if new_seqs.len() == appends.len() => {
                let mut out = Vec::with_capacity(appends.len());
                for ((append, (tenant, entity_type, entity_id)), new_seq) in
                    appends.iter().zip(entities).zip(new_seqs)
                {
                    let new_seq = *new_seq as u64;
                    self.record_segment(
                        tenant,
                        entity_type,
                        entity_id,
                        append.expected_sequence,
                        new_seq,
                    )
                    .await?;
                    out.push(PersistenceAppendResult {
                        persistence_id: append.persistence_id.clone(),
                        sequence_nr: new_seq,
                    });
                }
                Ok(out)
            }
            [0, index, actual] => {
                let append = usize::try_from(*index)
                    .ok()
                    .and_then(|index| index.checked_sub(1))
                    .and_then(|index| appends.get(index))
                    .ok_or_else(|| {
                        PersistenceError::Storage(format!(
                            "batch append script named append {index} of {}",
                            appends.len()
                        ))
                    })?;
                Err(PersistenceError::ConcurrencyViolation {
                    expected: append.expected_sequence,
                    actual: *actual as u64,
                })
            }
            other => Err(PersistenceError::Storage(format!(
                "unexpected batch append script result: {other:?}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use temper_runtime::persistence::{EventMetadata, EventStore, PersistenceEnvelope};

    use super::*;

    async fn make_store() -> Option<RedisEventStore> {
        let url = std::env::var("REDIS_URL").ok()?;
        Some(
            RedisEventStore::new(&url)
                .await
                .expect("failed to connect to Redis"),
        )
    }

    fn envelope(event_type: &str) -> PersistenceEnvelope {
        PersistenceEnvelope {
            sequence_nr: 0,
            event_type: event_type.to_string(),
            payload: serde_json::json!({}),
            metadata: EventMetadata {
                event_id: uuid::Uuid::new_v4(),
                causation_id: uuid::Uuid::new_v4(),
                correlation_id: uuid::Uuid::new_v4(),
                timestamp: chrono::Utc::now(),
                actor_id: "redis-batch-test".to_string(),
            },
        }
    }

    fn append(persistence_id: &str, expected_sequence: u64) -> PersistenceAppend {
        PersistenceAppend {
            persistence_id: persistence_id.to_string(),
            expected_sequence,
            events: vec![envelope("Deleted")],
        }
    }

    #[tokio::test]
    async fn every_journal_moves_or_none_does() {
        let Some(store) = make_store().await else {
            eprintln!("REDIS_URL not set, skipping test");
            return;
        };
        let tenant = format!("test-{}", uuid::Uuid::new_v4());
        let ids = ["t-1", "t-2", "t-3"].map(|id| format!("{tenant}:Track:{id}"));
        for id in &ids {
            store.append(id, 0, &[envelope("Created")]).await.unwrap();
        }
        // t-2 moves on after it was read at sequence 1.
        store
            .append(&ids[1], 1, &[envelope("Moved")])
            .await
            .unwrap();

        let stale = [append(&ids[0], 1), append(&ids[1], 1), append(&ids[2], 1)];
        let refused = store.append_batch(&stale).await;
        assert!(
            matches!(
                refused,
                Err(PersistenceError::ConcurrencyViolation {
                    expected: 1,
                    actual: 2
                })
            ),
            "a journal past its expected sequence refuses the batch: {refused:?}"
        );
        for (id, length) in ids.iter().zip([1, 2, 1]) {
            assert_eq!(store.read_events(id, 0).await.unwrap().len(), length);
        }

        let current = [append(&ids[0], 1), append(&ids[1], 2), append(&ids[2], 1)];
        let results = store.append_batch(&current).await.unwrap();
        assert_eq!(
            results.iter().map(|r| r.sequence_nr).collect::<Vec<_>>(),
            vec![2, 3, 2]
        );
        for (id, length) in ids.iter().zip([2, 3, 2]) {
            let events = store.read_events(id, 0).await.unwrap();
            assert_eq!(events.len(), length);
            assert_eq!(events.last().unwrap().event_type, "Deleted");
            assert_eq!(events.last().unwrap().sequence_nr, length as u64);
        }
    }
}
