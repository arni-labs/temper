//! Scheduled follow-ons that have not fired yet.
//!
//! Two follow-ons for the same action at the same moment on one record are
//! one follow-on. The second would run the same action on the same record in
//! the same second, against the state the first one just left; when each copy
//! schedules again, the copies would multiply every round instead of keeping
//! the steady pace the delay sets. Follow-ons at different moments, or for
//! different actions or records, all stay.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

/// One pending scheduled follow-on: the action, the record it runs on, and
/// the second it is due.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct FollowOnKey {
    pub(crate) tenant: String,
    pub(crate) entity_type: String,
    pub(crate) entity_id: String,
    pub(crate) action: String,
    /// Unix second at which the follow-on is due.
    pub(crate) due_second: i64,
}

/// The set of scheduled follow-ons waiting for their due second.
///
/// It holds one entry per waiting follow-on task, so it is as large as the
/// work already in flight and no larger.
#[derive(Clone, Default)]
pub(crate) struct PendingFollowOns(Arc<Mutex<BTreeSet<FollowOnKey>>>);

impl PendingFollowOns {
    /// Record `key` as pending. Returns `false` when the same follow-on is
    /// already pending, so the caller does not start a second one.
    pub(crate) fn claim(&self, key: FollowOnKey) -> bool {
        self.0
            .lock()
            .expect("pending follow-ons lock poisoned")
            .insert(key)
    }

    /// Remove `key` as its follow-on fires, so the action may be scheduled
    /// for that record again.
    pub(crate) fn release(&self, key: &FollowOnKey) {
        let removed = self
            .0
            .lock()
            .expect("pending follow-ons lock poisoned")
            .remove(key);
        debug_assert!(removed, "a firing follow-on was claimed when scheduled");
    }

    /// Number of follow-ons waiting to fire.
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

    fn key(action: &str, due_second: i64) -> FollowOnKey {
        FollowOnKey {
            tenant: "t".into(),
            entity_type: "Twin".into(),
            entity_id: "w1".into(),
            action: action.into(),
            due_second,
        }
    }

    #[test]
    fn the_same_action_at_the_same_second_on_one_record_is_one_follow_on() {
        let pending = PendingFollowOns::default();
        assert!(pending.claim(key("Ring", 10)));
        assert!(!pending.claim(key("Ring", 10)));
        assert!(pending.claim(key("Ring", 11)));
        assert!(pending.claim(key("Chime", 10)));
        pending.release(&key("Ring", 10));
        assert!(pending.claim(key("Ring", 10)));
    }
}
