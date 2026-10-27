//! Whether a stack's event store writes the stack's own entity rows in its
//! commit.
//!
//! A store that can write a record's query-plane and timer rows with its
//! events (`EventStore::commits_entity_rows`) writes them into itself. Those
//! are the rows the server reads only when the stack reads them from that same
//! store. A stack that pairs the journal with another query plane, or with
//! none, still has the server write the rows after the commit. The stack that
//! composes the stores is what knows which case it is.

use std::sync::Arc;

use super::{QueryPlaneStore, StorageStack};

/// Which of a stack's rows its event store's commit writes.
#[derive(Clone)]
pub(super) enum CommitWrites {
    /// None the stack reads.
    Nothing,
    /// The rows of this query plane, which is the event store itself. The
    /// stack's timer rows are kept in the same store.
    QueryPlane(Arc<dyn QueryPlaneStore>),
    /// The stack has no query plane, and the event store's own rows are the
    /// record's rows (a simulated store whose timer rows the server reads
    /// through the same store).
    OwnRows,
}

impl StorageStack {
    /// Mark the stack's query plane as the event store itself: its commits
    /// write that query plane's rows and the timer rows.
    pub(super) fn commits_its_query_plane(mut self) -> Self {
        self.commit_writes = match &self.query_plane {
            Some(plane) => CommitWrites::QueryPlane(plane.clone()),
            None => CommitWrites::Nothing,
        };
        self
    }

    /// Declare that the event store's own rows are this stack's entity rows.
    /// For a stack with no query plane, whose event store keeps each record's
    /// rows itself (the simulated store).
    pub fn with_entity_rows_in_journal(mut self) -> Self {
        self.commit_writes = CommitWrites::OwnRows;
        self
    }

    /// Whether each event's query-plane and timer rows are written in its
    /// commit, so nothing writes them after it. True only while the store can
    /// do it and the rows it writes are the ones this stack reads: replacing
    /// the query plane (a wrapper, or none) makes the server write them again.
    pub fn entity_rows_in_commit(&self) -> bool {
        if !self.events.commits_entity_rows() {
            return false;
        }
        match (&self.commit_writes, &self.query_plane) {
            (CommitWrites::QueryPlane(written), Some(plane)) => Arc::ptr_eq(written, plane),
            (CommitWrites::OwnRows, None) => true,
            _ => false,
        }
    }
}
