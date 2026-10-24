//! Scheduled follow-ons that have not finished yet.
//!
//! A follow-on is one `schedule` effect of one committed transition: the
//! record, the transition's event in that record's journal, and the effect's
//! place among the transition's schedule effects. A retried request that
//! replays the same reply (same event) starts no second copy of a follow-on
//! still in flight. Follow-ons of different transitions are different
//! operations, whatever action and time they name, and all run.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

/// One scheduled follow-on, named by the transition that scheduled it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct FollowOnKey {
    pub(crate) tenant: String,
    pub(crate) entity_type: String,
    pub(crate) entity_id: String,
    /// The scheduling transition's event: the record's event count after it
    /// committed (`EntityState::total_event_count`).
    pub(crate) parent_event: usize,
    /// Position of the `schedule` effect within that transition.
    pub(crate) effect_index: usize,
}

/// The set of scheduled follow-ons waiting to fire or still running.
///
/// It holds one entry per follow-on task in flight, so it is as large as the
/// work already in flight and no larger.
#[derive(Clone, Default)]
pub(crate) struct PendingFollowOns(Arc<Mutex<BTreeSet<FollowOnKey>>>);

impl PendingFollowOns {
    /// Record `key` as in flight. Returns `false` when the same follow-on is
    /// already in flight, so the caller does not start a second one.
    pub(crate) fn claim(&self, key: FollowOnKey) -> bool {
        self.0
            .lock()
            .expect("pending follow-ons lock poisoned")
            .insert(key)
    }

    /// Remove `key` once its follow-on has run.
    pub(crate) fn release(&self, key: &FollowOnKey) {
        let removed = self
            .0
            .lock()
            .expect("pending follow-ons lock poisoned")
            .remove(key);
        debug_assert!(removed, "a finished follow-on was claimed when scheduled");
    }

    /// Number of follow-ons in flight.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.0
            .lock()
            .expect("pending follow-ons lock poisoned")
            .len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(parent_event: usize, effect_index: usize) -> FollowOnKey {
        FollowOnKey {
            tenant: "t".into(),
            entity_type: "Bell".into(),
            entity_id: "b1".into(),
            parent_event,
            effect_index,
        }
    }

    #[test]
    fn a_follow_on_is_named_by_its_transition_and_effect() {
        let pending = PendingFollowOns::default();
        assert!(pending.claim(key(3, 0)));
        assert!(!pending.claim(key(3, 0)));
        assert!(pending.claim(key(3, 1)));
        assert!(pending.claim(key(4, 0)));
        pending.release(&key(3, 0));
        assert!(pending.claim(key(3, 0)));
    }
}
