//! Liveness by helpful moves.
//!
//! A rule `eventually = goal` SETTLES when, from every reachable state, the
//! system's own moves (dispatches arriving, timers, module answers, and the
//! actions the rule relies on while `goal` is false) can reach a state where
//! `goal` holds: the joint walk's reading. Its proof names regions R1..Rn
//! that cover every state where the lemmas hold and `goal` does not, each
//! with a helpful move: from any state in Ri the move is enabled and leads
//! to a state where `goal` holds or into some Rj with j < i. The order of
//! the regions is the ranking, so every state reaches `goal` in at most n
//! helpful moves. The lemmas are proved inductive alongside.
//!
//! A region may name a witness `w`, a dispatch of one kind in flight: the
//! region holds when some such dispatch satisfies it, and its move is that
//! dispatch arriving. A helpful move's parameters are the ones its region
//! pins (the system chooses them, as the walk lets a module answer anything
//! its record takes); the others may be anything.

use std::collections::BTreeMap;

/// One region of a liveness proof.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Region {
    /// The rule it is for.
    pub rule: String,
    pub name: String,
    /// SMT-LIB over `$`-framed names, and `w` when it names a witness.
    pub region: String,
    /// The kind of dispatch `w` is, when the region names one.
    #[serde(default)]
    pub witness: Option<String>,
    /// The helpful move, by its label.
    pub step: String,
    /// The helpful move's parameters, as SMT-LIB over `$`-framed names;
    /// `passed.<p>` says whether a string parameter is passed.
    #[serde(default)]
    pub params: BTreeMap<String, String>,
}

impl Region {
    /// The region read in `frame`: with a witness, that some dispatch in
    /// flight satisfies it.
    pub(super) fn read(&self, frame: &str) -> String {
        let body = self.region.replace('$', &format!("{frame}."));
        match &self.witness {
            Some(kind) => format!("(exists ((w Msg/{kind})) {body})"),
            None => body,
        }
    }

    /// The region read in `frame` about the move's own witness `w`.
    pub(super) fn read_at_w(&self, frame: &str) -> String {
        self.region.replace('$', &format!("{frame}."))
    }
}
