//! Work accounting inside state expansion.

/// Shared work budget for expansion, including intermediate products. A walk
/// must report incomplete if this budget stops even when its queue is empty.
pub(crate) struct Expansion {
    limit: Option<super::types::ExploreLimit>,
    remaining: std::cell::Cell<usize>,
    until: std::cell::Cell<Option<std::time::Instant>>,
    pub(crate) exhausted: std::cell::Cell<bool>,
}
impl Default for Expansion {
    fn default() -> Self {
        Self {
            limit: None,
            remaining: std::cell::Cell::new(usize::MAX),
            until: std::cell::Cell::new(None),
            exhausted: std::cell::Cell::new(false),
        }
    }
}
impl Expansion {
    pub(crate) fn for_model(model: &super::types::TemperModel) -> Self {
        Self {
            limit: model.limit.clone(),
            ..Self::default()
        }
    }

    pub(crate) fn reset(&self, states: usize, time: Option<std::time::Duration>) {
        self.remaining.set(states);
        self.until
            .set(time.and_then(|t| std::time::Instant::now().checked_add(t)));
        self.exhausted.set(false);
    }
    pub(crate) fn take(&self) -> bool {
        use std::sync::atomic::Ordering;
        if let Some(limit) = &self.limit
            && !limit.lifted.load(Ordering::Relaxed)
            && (limit.past_time()
                || limit
                    .work
                    .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
                    .is_err())
        {
            limit.exhausted.store(true, Ordering::Relaxed);
            self.exhausted.set(true);
            return false;
        }
        if self.remaining.get() == 0
            || self
                .until
                .get()
                .is_some_and(|t| std::time::Instant::now() >= t)
        {
            self.exhausted.set(true);
            return false;
        }
        self.remaining.set(self.remaining.get() - 1);
        true
    }
}
