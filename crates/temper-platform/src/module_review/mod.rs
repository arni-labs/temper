//! Jev's review of an app's own modules against its specs, made at install.
//!
//! A module may only compute a value between two moves: parse, format, score, transform, or call one outside service
//! and return its answer. What else it does (choosing the move, keeping steps of its own, waiting, doing several
//! steps, holding rules, deciding out of sight, packing several concerns) belongs to the spec's states, guards, timers
//! and Cedar, where the checker can see it and the person can follow it. When the app's manifest gives a module's
//! sentence on what it computes (`does`), Jev is also asked whether its code does that, and a sentence it does not is a
//! finding too.
//!
//! Enforced for new and rewritten code on every install a request makes (a push, a start's `--app`, a Genesis registry
//! install): in a module whose code this server runs nowhere, or whose sentence changed, a finding on a line the change
//! added or altered, or on no line, is refused (`os_apps::module_review_gate`). A finding on a line as the app last
//! installed it, a module whose code is as installed, and a boot replay or rollback are only advised on, so code that
//! runs now is not blocked until someone changes it. Each window of a module is asked in words that do not change when
//! the rest of the module does (`questions::review_state`), so the same code gets the same answer. A Jev that does
//! not answer refuses nothing and is said once in what the review keeps, never retried in a loop: a Jev outage must not
//! stop every install.

mod excerpt;
mod items;
mod jev;
mod questions;
mod review;
mod reviewer;
mod source;
mod types;
mod under_way;

pub use excerpt::{Excerpt, SPEC_BYTES, excerpt, said_left_out};
pub use items::{FOUND_AT, ITEMS, Item, LARGE_LINES, QUESTION_VERSION};
pub use jev::{JEV_KEY_ENV, JEV_MODEL, JEV_URL_ENV, JevJudge};
pub use review::review_module;
pub use reviewer::{BundleReview, ModuleReviewer};
pub use source::changed_lines;
pub use types::{
    Answer, Answers, Finding, Judge, JudgeUnavailable, ModuleReview, ModuleSource, Question,
    SpecSource,
};
pub use under_way::ReviewsUnderWay;
pub(crate) use under_way::{ReviewSaveFailure, ReviewStatus};

#[cfg(test)]
mod tests;
