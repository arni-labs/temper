//! Classes of outside principals: what the policies can tell apart about the
//! principal who sends an action.
//!
//! A reaction with no declared principal runs as whoever sent its source's
//! action. When that is someone outside, the reaction's Cedar is decided for
//! that same principal, who has already passed the source's. The policies
//! test a principal's type, and its attributes against values and patterns.
//! A class fixes each of those: one type the policies name, or a type none of
//! them names; and for each attribute they read, one value they compare it
//! with, a value none of them names (matching some of their patterns and not
//! the others), or no such attribute. Every principal falls in some class,
//! so keeping the classes that may send the source and deciding the
//! reaction for each decides it for the source's caller. A class may hold no
//! real principal (a value matching patterns nothing can match at once);
//! that only adds a caller who cannot exist.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value as Json;

use super::json::{entity_parts, single};

/// One class of outside principals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Class {
    /// Its type, or `None` for a type none of `types` is.
    kind: Option<String>,
    /// The types the policies name.
    types: BTreeSet<String>,
    /// Each attribute the policies read.
    attrs: BTreeMap<String, Held>,
}

/// An attribute of a class of principals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Held {
    /// It has no such attribute.
    Absent,
    /// This value.
    Value(Json),
    /// Some other value.
    Other(Other),
}

/// A value none of the policies' values for its attribute equals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Other {
    /// The values the policies compare the attribute with.
    pub(super) named: Vec<Json>,
    /// The patterns they test it against, each with whether it matches.
    pub(super) patterns: Vec<(Json, bool)>,
}

impl Class {
    /// Whether a principal of this class is of type `kind`: `None` when the
    /// class does not say.
    pub(super) fn is(&self, kind: &str) -> Option<bool> {
        match &self.kind {
            Some(own) => Some(own == kind),
            None if self.types.contains(kind) => Some(false),
            None => None,
        }
    }

    /// The class's attribute `attr`, when the policies read it.
    pub(super) fn attribute(&self, attr: &str) -> Option<&Held> {
        self.attrs.get(attr)
    }
}

/// What policies test about the principal.
#[derive(Default)]
struct Facts {
    types: BTreeSet<String>,
    /// Each attribute read, with the values it is compared with and the
    /// patterns it is tested against.
    attrs: BTreeMap<String, (Vec<Json>, Vec<Json>)>,
}

/// Every class of outside principals `policies` tell apart.
pub(super) fn classes<'a>(policies: impl IntoIterator<Item = &'a Json>) -> Vec<Class> {
    let mut facts = Facts::default();
    for policy in policies {
        facts.collect(policy);
    }
    let mut classes: Vec<Class> = facts
        .types
        .iter()
        .cloned()
        .map(Some)
        .chain([None])
        .map(|kind| Class {
            kind,
            types: facts.types.clone(),
            attrs: BTreeMap::new(),
        })
        .collect();
    for (attr, (values, patterns)) in &facts.attrs {
        let mut held = vec![Held::Absent];
        held.extend(values.iter().cloned().map(Held::Value));
        let mut matches: Vec<Vec<bool>> = vec![Vec::new()];
        for _ in patterns {
            matches = matches
                .into_iter()
                .flat_map(|some| [false, true].map(|yes| [some.clone(), vec![yes]].concat()))
                .collect();
        }
        held.extend(matches.into_iter().map(|matches| {
            Held::Other(Other {
                named: values.clone(),
                patterns: patterns.iter().cloned().zip(matches).collect(),
            })
        }));
        classes = classes
            .into_iter()
            .flat_map(|class| {
                held.iter().map(move |one| {
                    let mut class = class.clone();
                    class.attrs.insert(attr.clone(), one.clone());
                    class
                })
            })
            .collect();
    }
    classes
}

impl Facts {
    fn collect(&mut self, policy: &Json) {
        let principal = &policy["principal"];
        match principal["op"].as_str() {
            Some("==") => self.entity(&principal["entity"]),
            Some("is") => self.kind(&principal["entity_type"]),
            _ => {}
        }
        for condition in policy["conditions"].as_array().into_iter().flatten() {
            self.walk(&condition["body"]);
        }
    }

    fn walk(&mut self, expr: &Json) {
        if let Some((op, arg)) = single(expr) {
            match op {
                "is" if is_principal(&arg["left"]) => self.kind(&arg["entity_type"]),
                "==" | "!=" => {
                    for (side, other) in
                        [(&arg["left"], &arg["right"]), (&arg["right"], &arg["left"])]
                    {
                        if let (true, Some(value)) = (is_principal(side), literal(other)) {
                            self.entity(&value);
                        }
                        if let (Some(attr), Some(value)) = (principal_attr(side), literal(other)) {
                            self.value(attr, value);
                        }
                    }
                }
                "like" => {
                    if let Some(attr) = principal_attr(&arg["left"]) {
                        self.pattern(attr, arg["pattern"].clone());
                    }
                }
                "in" | "contains" => {
                    let (item, set) = if op == "in" {
                        (&arg["left"], &arg["right"])
                    } else {
                        (&arg["right"], &arg["left"])
                    };
                    if let (Some(attr), Some(Json::Array(set))) =
                        (principal_attr(item), literal(set))
                    {
                        set.into_iter().for_each(|value| self.value(attr, value));
                    }
                }
                "has" | "." if is_principal(&arg["left"]) => {
                    if let Some(attr) = arg["attr"].as_str() {
                        self.attrs.entry(attr.to_string()).or_default();
                    }
                }
                _ => {}
            }
        }
        match expr {
            Json::Object(fields) => fields.values().for_each(|child| self.walk(child)),
            Json::Array(items) => items.iter().for_each(|child| self.walk(child)),
            _ => {}
        }
    }

    fn kind(&mut self, kind: &Json) {
        if let Some(kind) = kind.as_str() {
            self.named(kind);
        }
    }

    fn entity(&mut self, entity: &Json) {
        if let Some((kind, _)) = entity_parts(entity) {
            self.named(kind);
        }
    }

    /// A type a policy names. Only the kernel makes `System` principals,
    /// so no outside principal is of it.
    fn named(&mut self, kind: &str) {
        if kind != "System" {
            self.types.insert(kind.to_string());
        }
    }

    fn value(&mut self, attr: &str, value: Json) {
        let (values, _) = self.attrs.entry(attr.to_string()).or_default();
        if !values.contains(&value) {
            values.push(value);
        }
    }

    fn pattern(&mut self, attr: &str, pattern: Json) {
        let (_, patterns) = self.attrs.entry(attr.to_string()).or_default();
        if !patterns.contains(&pattern) {
            patterns.push(pattern);
        }
    }
}

fn is_principal(expr: &Json) -> bool {
    single(expr).is_some_and(|(op, arg)| op == "Var" && arg == "principal")
}

/// `principal.<attr>`.
fn principal_attr(expr: &Json) -> Option<&str> {
    let (op, arg) = single(expr)?;
    (op == "." && is_principal(&arg["left"])).then(|| arg["attr"].as_str())?
}

/// A literal: a value, or a set of values.
fn literal(expr: &Json) -> Option<Json> {
    match single(expr)? {
        ("Value", value) => Some(value.clone()),
        ("Set", Json::Array(members)) => members
            .iter()
            .map(|member| match single(member) {
                Some(("Value", value)) => Some(value.clone()),
                _ => None,
            })
            .collect::<Option<Vec<_>>>()
            .map(Json::Array),
        _ => None,
    }
}
