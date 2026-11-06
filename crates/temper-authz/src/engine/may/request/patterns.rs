//! The `like` patterns of a set of policies, printed once.

use std::borrow::Cow;
use std::collections::HashMap;

use serde_json::Value as Json;

use super::super::json::{at, single};

/// Each `like` pattern of some policies as JSON text, by where the pattern
/// is. The policies must outlive it and stay where they are: a part of them
/// is found by its address, and no other value can have that address while
/// they live.
#[derive(Debug, Default)]
pub(in crate::engine::may) struct Patterns(HashMap<usize, String>);

impl Patterns {
    pub(in crate::engine::may) fn read(policies: &[(cedar_policy::Effect, Json)]) -> Self {
        let mut patterns = Self::default();
        for (_, policy) in policies {
            patterns.walk(policy);
        }
        patterns
    }

    fn walk(&mut self, expr: &Json) {
        if let Some(("like", arg)) = single(expr) {
            let pattern = at(arg, "pattern");
            self.0
                .insert(std::ptr::from_ref(pattern) as usize, pattern.to_string());
        }
        match expr {
            Json::Object(fields) => fields.values().for_each(|value| self.walk(value)),
            Json::Array(items) => items.iter().for_each(|item| self.walk(item)),
            _ => {}
        }
    }

    /// `pattern`'s JSON text: printed now when it is not one of the
    /// policies'.
    pub(in crate::engine::may) fn text<'p>(&'p self, pattern: &Json) -> Cow<'p, str> {
        match self.0.get(&(std::ptr::from_ref(pattern) as usize)) {
            Some(text) => Cow::Borrowed(text),
            None => Cow::Owned(pattern.to_string()),
        }
    }
}
