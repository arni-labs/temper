//! Initial ID equality classes, including distinguished concrete strings.

use super::*;

impl JointModel {
    /// The starting states: one per way the records' ids can be equal or
    /// differ from one another and from the relevant concrete literals.
    pub(super) fn inits(&self) -> Box<dyn Iterator<Item = JointState> + '_> {
        let blank = self.blank();
        if self.records == Records::One {
            return Box::new(std::iter::once(JointState {
                records: blank,
                inflight: Vec::new(),
                calls: Vec::new(),
                armed: BTreeMap::new(),
            }));
        }
        let mut literals = self.id_literals.clone();
        for model in self.models.values() {
            literals.extend(model.string_literals.values().flatten().cloned());
        }
        for record in blank.values() {
            for value in record.strings.values() {
                if let ModelString::Literal(s) = value {
                    literals.insert(s.clone());
                }
            }
        }
        literals.remove("");
        let literals: Vec<_> = literals.into_iter().map(ModelString::from).collect();
        Box::new(partitions(blank.len(), literals.len()).map(move |blocks| {
            let mut records = blank.clone();
            for (record, block) in records.values_mut().zip(blocks) {
                record.strings.insert(
                    ID.to_string(),
                    literals
                        .get(block)
                        .cloned()
                        .unwrap_or_else(|| ModelString::Symbol(block - literals.len() + 1)),
                );
            }
            let mut state = JointState {
                records,
                inflight: Vec::new(),
                calls: Vec::new(),
                armed: BTreeMap::new(),
            };
            self.arm_at_start(&mut state);
            self.order.normalize(&mut state);
            canonical(&mut state);
            state
        }))
    }
}

/// Every way to split `n` items into blocks, as each item's block number,
/// with `fixed` distinguished literal blocks followed by fresh blocks
/// numbered in order of first use.
fn partitions(n: usize, fixed: usize) -> impl Iterator<Item = Vec<usize>> {
    // Restricted-growth strings enumerate partitions with O(n) storage.
    let mut blocks = Some(vec![0; n]);
    std::iter::from_fn(move || {
        let current = blocks.take()?;
        let mut next = current.clone();
        for i in (0..n).rev() {
            let largest = next[..i]
                .iter()
                .copied()
                .map(|b| b + 1)
                .max()
                .unwrap_or(0)
                .max(fixed);
            if next[i] < largest {
                next[i] += 1;
                next[i + 1..].fill(0);
                blocks = Some(next);
                break;
            }
        }
        Some(current)
    })
}

#[cfg(test)]
mod tests {
    use super::partitions;
    use std::collections::BTreeSet;

    #[test]
    fn literal_blocks_and_fresh_equalities_are_all_explored() {
        assert_eq!(
            partitions(1, 2).collect::<Vec<_>>(),
            vec![vec![0], vec![1], vec![2]]
        );
        assert_eq!(
            partitions(2, 1).collect::<Vec<_>>(),
            vec![vec![0, 0], vec![0, 1], vec![1, 0], vec![1, 1], vec![1, 2],]
        );
    }

    #[test]
    fn incremental_partitions_are_exhaustive_and_unique() {
        for (n, expected) in [1, 1, 2, 5, 15, 52].into_iter().enumerate() {
            let all: Vec<_> = partitions(n, 0).collect();
            assert_eq!(all.len(), expected);
            assert_eq!(all.into_iter().collect::<BTreeSet<_>>().len(), expected);
        }
    }
}
