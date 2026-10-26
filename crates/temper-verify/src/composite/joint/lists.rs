//! Lists of numbers, one for each walked state, kept in one table.

/// A list of numbers for each state, kept in one table.
pub(super) struct Lists {
    /// Where each state's list starts in `items`; it ends where the next
    /// one starts.
    starts: Vec<usize>,
    pub(super) items: Vec<u32>,
}

impl Lists {
    /// The lists `add` fills. It is called twice, and gives the same pairs
    /// of a state and a number for its list both times: once to count each
    /// list, then to fill it.
    pub(super) fn build(count: usize, add: impl Fn(&mut dyn FnMut(u32, u32))) -> Self {
        let mut starts = vec![0; count];
        add(&mut |state, _| starts[state as usize] += 1);
        let mut total = 0;
        for start in &mut starts {
            total += *start;
            *start = total;
        }
        // Each list is filled from its end, which leaves its start where
        // the list begins.
        let mut items = vec![0; total];
        add(&mut |state, number| {
            let start = &mut starts[state as usize];
            *start -= 1;
            items[*start] = number;
        });
        Self { starts, items }
    }

    pub(super) fn get(&self, state: usize) -> &[u32] {
        let end = self
            .starts
            .get(state + 1)
            .copied()
            .unwrap_or(self.items.len());
        &self.items[self.starts[state]..end]
    }
}
