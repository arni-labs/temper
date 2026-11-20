//! The page a walk keeps (ADR-0188). A walk judges the rows a batch at a
//! time; what it holds between batches is the requested page and no more:
//!
//! - in id order (no `$orderby`), the first `$skip` rows are counted and
//!   dropped, and the page is the next `$top` documents;
//! - in an order the walk sorts itself, a row can move into the page until
//!   the last one is judged, so the walk holds the first `$skip + $top` rows
//!   in that order as their sort keys and ids in a bounded heap, never as
//!   documents, and reads the page's documents again once the walk ends.
//!   Only when `$skip` is 0 is the heap the page itself, and then it holds the
//!   documents.
//!
//! Each batch costs `log($skip + $top)` a row in the heap; nothing re-sorts
//! what is held.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

use temper_odata::query::types::OrderByClause;

use super::super::super::authz::entity_id_from_body;
use crate::query_eval::{compare_sort_keys, sort_key};

/// One row the sorted walk holds: what it sorts by, then its id.
pub(super) struct Ranked<'a> {
    key: Vec<Option<serde_json::Value>>,
    id: String,
    document: Option<serde_json::Value>,
    order: &'a [OrderByClause],
}

impl Ord for Ranked<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        compare_sort_keys(&self.key, &other.key, self.order).then_with(|| self.id.cmp(&other.id))
    }
}

impl PartialOrd for Ranked<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Ranked<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Ranked<'_> {}

/// What a walk holds between batches.
pub(super) enum WalkPage<'a> {
    /// Rows in the order they are judged (id order): the first `skip` are
    /// counted, not kept.
    InIdOrder {
        skip: usize,
        top: usize,
        passed: usize,
        page: Vec<serde_json::Value>,
    },
    /// Rows the walk sorts: the first `skip + top` by [`compare_sort_keys`]
    /// and then id, the largest on top of the heap so it is the one dropped.
    Sorted {
        order: &'a [OrderByClause],
        skip: usize,
        top: usize,
        heap: BinaryHeap<Ranked<'a>>,
    },
}

/// The page once the walk ends.
pub(super) enum FinishedPage {
    /// The page's documents, in order.
    Documents(Vec<serde_json::Value>),
    /// The page's ids, in order, whose documents the walk did not keep.
    Ids(Vec<String>),
}

impl<'a> WalkPage<'a> {
    pub(super) fn new(order: Option<&'a [OrderByClause]>, skip: usize, top: usize) -> Self {
        match order {
            None => Self::InIdOrder {
                skip,
                top,
                passed: 0,
                page: Vec::new(),
            },
            // The heap grows with the rows judged, up to `skip + top`; it is
            // never sized up front from a client's `$skip`.
            Some(order) => Self::Sorted {
                order,
                skip,
                top,
                heap: BinaryHeap::new(),
            },
        }
    }

    /// Whether more rows cannot change the page: only in id order, once the
    /// page holds `top` rows.
    pub(super) fn is_full(&self) -> bool {
        match self {
            Self::InIdOrder { top, page, .. } => page.len() >= *top,
            Self::Sorted { .. } => false,
        }
    }

    /// Documents held now.
    pub(super) fn documents_held(&self) -> usize {
        match self {
            Self::InIdOrder { page, .. } => page.len(),
            Self::Sorted { skip: 0, heap, .. } => heap.len(),
            Self::Sorted { .. } => 0,
        }
    }

    /// Take one batch of judged rows, in the order they were judged.
    pub(super) fn add(&mut self, rows: Vec<serde_json::Value>) {
        match self {
            Self::InIdOrder {
                skip,
                top,
                passed,
                page,
            } => {
                for row in rows {
                    let index = *passed;
                    *passed += 1;
                    if index >= *skip && page.len() < *top {
                        page.push(row);
                    }
                }
            }
            Self::Sorted {
                order,
                skip,
                top,
                heap,
            } => {
                let capacity = skip.saturating_add(*top);
                for row in rows {
                    let entry = Ranked {
                        key: sort_key(&row, order),
                        id: entity_id_from_body(&row).unwrap_or_default().to_string(),
                        document: (*skip == 0).then_some(row),
                        order,
                    };
                    if heap.len() < capacity {
                        heap.push(entry);
                    } else if heap.peek().is_some_and(|largest| entry < *largest) {
                        heap.pop();
                        heap.push(entry);
                    }
                }
            }
        }
    }

    /// The page: rows `skip..skip + top` of everything added.
    pub(super) fn finish(self) -> FinishedPage {
        match self {
            Self::InIdOrder { page, .. } => FinishedPage::Documents(page),
            Self::Sorted { skip, heap, .. } => {
                let page = heap.into_sorted_vec().into_iter().skip(skip);
                if skip == 0 {
                    FinishedPage::Documents(page.filter_map(|entry| entry.document).collect())
                } else {
                    FinishedPage::Ids(page.map(|entry| entry.id).collect())
                }
            }
        }
    }
}

/// Put documents read again for a page back in the walk's order.
pub(super) fn sort_page(documents: &mut [serde_json::Value], order: &[OrderByClause]) {
    documents.sort_by(|a, b| {
        compare_sort_keys(&sort_key(a, order), &sort_key(b, order), order).then_with(|| {
            entity_id_from_body(a)
                .unwrap_or_default()
                .cmp(entity_id_from_body(b).unwrap_or_default())
        })
    });
}

#[cfg(test)]
#[path = "walk_page_test.rs"]
mod tests;
