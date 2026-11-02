//! What a timer's fire names: the row or arming it fires, so a fire sent
//! twice commits once.

/// A scheduled action's row, by its record and arm id.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ScheduledFire {
    /// The record's tenant.
    pub tenant: String,
    /// The record's entity type.
    pub entity_type: String,
    /// The record's id.
    pub entity_id: String,
    /// The row's arm id.
    pub arm_id: String,
}

/// A state timer's fire: its record, its action, and the record's journal
/// sequence when the timer was armed.
///
/// An arming is read from the journal at `armed_seq`, so every writer that
/// holds it (an old and a new process during a deploy, or a fire sent again
/// after a lost answer) fires at that sequence. The first to commit moves the
/// journal past it; the store's sequence check picks that writer. Every other
/// fire of the arming then finds the record moved and is dropped, so an
/// arming commits its action at most once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateTimerFire {
    /// The record's tenant.
    pub tenant: String,
    /// The record's entity type.
    pub entity_type: String,
    /// The record's id.
    pub entity_id: String,
    /// The timed state the timer was declared for.
    pub state: String,
    /// The timer's `on_timeout` action.
    pub action: String,
    /// The record's journal sequence when the timer was armed.
    pub armed_seq: u64,
}
