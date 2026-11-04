//! Which text literals each record id and text field can meet.
//!
//! Two terms (a type's field, id or status) share a class when anything in
//! scope can tell their values apart or carry one into the other: a guard,
//! rule, trigger guard or liveness goal compares them (`==`, `!=`, `<`,
//! `in` a list field), an effect, a reaction's or a timer's arguments, or a
//! spawn copies one into the other, a constraint ties a parameter to a
//! field, a related read `T[f].x` or a reaction's target reference finds a
//! record by its id, or a same-id reaction gives two types one id. A move's
//! parameter counts as the field of its name, which the runtime writes.
//!
//! A class holds the literals its terms are compared with, start with, are
//! assigned or have appended, and are sent as a reaction's, timer's or
//! spawn's literal arguments or a static target's id; a status class holds
//! its type's places. The walk offers each id and tracked field only its own
//! class's literals. A literal no term of a class is compared with or
//! receives is, to everything that reads that class, a value like any made
//! up one, and the walk already offers made-up values (`#n`, blank or not)
//! everywhere: a state that holds such a literal is a renaming of one the
//! walk still reaches, and every guard, rule and liveness verdict is the
//! same on both.

use std::collections::{BTreeMap, BTreeSet};

use temper_authz::Read;
use temper_spec::automaton::{ActionConstraint, Automaton, Initial, TargetResolver, TriggerKind};
use temper_spec::predicate::{Arg, Effect, Expr, Literal, Operand, Set};

use super::super::JointState;
use super::super::env::ID;
use super::JointModel;
use crate::model::{ModelString, ParamValue, TemperModel};

impl JointModel {
    /// A made-up value of `state` outside the class of the field or parameter
    /// holding it, which would mean something copied or compared it that
    /// [`Classes`] did not read. Checked steps look for one.
    pub(in crate::composite::joint) fn value_out_of_class(
        &self,
        state: &JointState,
    ) -> Option<String> {
        let stray = |entity: &str, field: &str, value: &ModelString| {
            let class = self.models.get(entity)?.string_classes.get(field)?;
            value
                .class()
                .is_some_and(|of| of != *class)
                .then(|| format!("{entity}.{field} holds {value} of another class than {class}"))
        };
        let in_records = state.records.iter().flat_map(|(entity, record)| {
            let strings = record.strings.iter().map(|(f, v)| (*f, v));
            let lists = record
                .lists
                .iter()
                .flat_map(|(f, list)| list.iter().map(|v| (*f, v)));
            strings
                .chain(lists)
                .filter_map(move |(f, v)| stray(entity.as_str(), f.as_str(), v))
        });
        let on_their_way = state.inflight.iter().flat_map(|dispatch| {
            dispatch
                .params
                .iter()
                .filter_map(|(param, value)| match value {
                    ParamValue::Str(value) => stray(dispatch.to.as_str(), param.as_str(), value),
                    _ => None,
                })
        });
        in_records.chain(on_their_way).next()
    }
}

/// What in a record a value sits in.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Slot {
    Id,
    Status,
    Field(String),
}

impl Slot {
    fn named(name: &str) -> Self {
        if matches!(name, "Id" | "id") {
            Slot::Id
        } else {
            Slot::Field(name.to_string())
        }
    }
}

/// A slot of one entity type.
type Term = (String, Slot);

/// The equality classes of the terms of the types in a scope, each with the
/// literals it holds.
#[derive(Debug, Default)]
pub(in crate::composite::joint) struct Classes {
    parent: BTreeMap<Term, Term>,
    held: BTreeMap<Term, BTreeSet<String>>,
    /// The parameters a `param_nonempty` constraint refuses a blank value in.
    reads_blank: BTreeSet<Term>,
}

impl Classes {
    /// The classes of everything the types in `scope` declare.
    pub(in crate::composite::joint) fn read(
        by_name: &BTreeMap<&str, &Automaton>,
        scope: &BTreeSet<String>,
    ) -> Self {
        let mut classes = Classes::default();
        for entity in scope {
            classes.declare(entity, by_name[entity.as_str()]);
        }
        classes
    }

    /// The literals `entity`'s field `field` (or its id) can meet, `''` left
    /// out: the walk always offers it.
    pub(in crate::composite::joint) fn literals(
        &self,
        entity: &str,
        field: &str,
    ) -> BTreeSet<String> {
        let root = self.root(&(entity.to_string(), Slot::named(field)));
        let mut literals = self.held.get(&root).cloned().unwrap_or_default();
        literals.remove("");
        literals
    }

    /// Link what a policy compares (`resource.a == resource.b`, say), for
    /// each type in `scope`: `compared` is each comparison's reads that can
    /// hold a made-up value. A context key that can holds some type's id.
    pub(in crate::composite::joint) fn read_policies(
        &mut self,
        compared: &[Vec<Read>],
        scope: &BTreeSet<String>,
    ) {
        for reads in compared {
            for entity in scope {
                let terms: Vec<Term> = reads
                    .iter()
                    .flat_map(|read| match read {
                        Read::Resource(field) => vec![(entity.clone(), Slot::named(field))],
                        Read::Context(_) => scope.iter().map(|t| (t.clone(), Slot::Id)).collect(),
                    })
                    .collect();
                for pair in terms.windows(2) {
                    self.link(&pair[0], &pair[1]);
                }
            }
        }
    }

    /// Tell each of `models` the class of its string fields and parameters.
    ///
    /// Nothing can tell whether values of two classes are equal: everything
    /// that compares or copies one value into another links their terms, so
    /// a walk that keeps the values of each class apart and numbers them from
    /// one in each class merges only states nothing can tell apart.
    pub(in crate::composite::joint) fn number(&self, models: &mut BTreeMap<String, TemperModel>) {
        let mut numbers: BTreeMap<Term, usize> = BTreeMap::new();
        let reads_blank: BTreeSet<Term> = self
            .reads_blank
            .iter()
            .map(|term| self.root(term))
            .collect();
        for (entity, model) in models.iter_mut() {
            let params = model.transitions.iter().flat_map(|t| &t.string_params);
            let names: BTreeSet<String> = model
                .tracked_strings
                .iter()
                .cloned()
                .chain(params.map(|param| param.name.to_string()))
                .chain([ID.to_string()])
                .collect();
            for name in names {
                let root = self.root(&(entity.clone(), Slot::named(&name)));
                let next = numbers.len() + 1;
                if !reads_blank.contains(&root) {
                    let class = numbers.get(&root).copied().unwrap_or(next);
                    model.never_blank.insert(class);
                }
                let class = *numbers.entry(root).or_insert(next);
                model.string_classes.insert(name, class);
            }
        }
    }

    fn declare(&mut self, entity: &str, a: &Automaton) {
        let term = |name: &str| (entity.to_string(), Slot::named(name));
        let own = (entity.to_string(), Slot::Status);
        self.hold_all(&own, a.automaton.states.iter().cloned());
        for sv in &a.state {
            match &sv.initial {
                Initial::String(value) => self.hold(&term(&sv.name), value),
                Initial::List(items) => self.hold_all(&term(&sv.name), items.iter().cloned()),
                _ => {}
            }
        }
        for inv in &a.invariants {
            self.visit(entity, &inv.assert);
        }
        for rule in &a.entry_rules {
            self.visit(entity, &rule.assert);
        }
        for goal in a
            .liveness
            .iter()
            .filter_map(|live| live.eventually.as_ref())
        {
            self.visit(entity, goal);
        }
        for timeout in &a.state_timeouts {
            for (param, arg) in &timeout.args {
                self.copy(&term(param), entity, arg);
            }
        }
        for action in &a.actions {
            self.visit(entity, &action.guard);
            for constraint in &action.constraints {
                if matches!(constraint, ActionConstraint::ParamNonempty { .. }) {
                    self.reads_blank.insert(term(constraint.param()));
                }
                if let Some(field) = constraint.field() {
                    self.link(&term(constraint.param()), &term(field));
                }
            }
            for effect in &action.effect {
                match effect {
                    Effect::Assign { var, value, .. } => self.copy(&term(var), entity, value),
                    Effect::Append { list, value } | Effect::Remove { list, value } => {
                        self.copy(&term(list), entity, value);
                    }
                    Effect::Spawn {
                        entity_type,
                        store_id_in,
                        id,
                        copy,
                        ..
                    } => {
                        let child = |name: &str| (entity_type.clone(), Slot::named(name));
                        if let Some(field) = store_id_in {
                            self.link(&term(field), &child("Id"));
                        }
                        if let Some(id) = id {
                            self.copy(&child("Id"), entity, id);
                        }
                        // The child's first action gets this action's
                        // parameters and the copied fields.
                        let passed = action
                            .params
                            .iter()
                            .map(|p| p.name())
                            .chain(copy.iter().map(String::as_str));
                        for name in passed {
                            self.link(&term(name), &child(name));
                        }
                    }
                    _ => {}
                }
            }
            for trigger in &action.triggers {
                if let Some(guard) = &trigger.guard {
                    self.visit(entity, guard);
                }
                let (TriggerKind::Entity, Some(to)) = (&trigger.kind, &trigger.target_entity)
                else {
                    continue;
                };
                let target = |name: &str| (to.clone(), Slot::named(name));
                for (param, arg) in &trigger.args {
                    self.copy(&target(param), entity, arg);
                }
                match &trigger.resolve_target {
                    Some(
                        TargetResolver::Field { id_field }
                        | TargetResolver::CreateIfMissing { id_field },
                    ) => self.link(&term(id_field), &target("Id")),
                    Some(TargetResolver::SameId) => self.link(&term("Id"), &target("Id")),
                    Some(TargetResolver::Static { entity_id }) => {
                        self.hold(&target("Id"), entity_id);
                    }
                    Some(TargetResolver::Create) | None => {}
                }
            }
        }
    }

    /// `value`, written by an `entity` move, goes into `into`.
    fn copy(&mut self, into: &Term, entity: &str, value: &Arg) {
        match value {
            Arg::Lit(Literal::Str(text)) => self.hold(into, text),
            Arg::Var(name) | Arg::Param(name) => {
                self.link(into, &(entity.to_string(), Slot::named(name)));
            }
            Arg::Lit(_) => {}
        }
    }

    /// The term `operand` of an `owner` predicate reads, linking the id
    /// field a related read finds its record by with that record's id.
    fn term(&mut self, owner: &str, operand: &Operand) -> Option<Term> {
        let mut related = |entity_type: &str, id_field: &str, slot: Slot| {
            let id = (entity_type.to_string(), Slot::Id);
            self.link(&(owner.to_string(), Slot::named(id_field)), &id);
            Some((entity_type.to_string(), slot))
        };
        match operand {
            Operand::Var(name) | Operand::Param(name) => {
                Some((owner.to_string(), Slot::named(name)))
            }
            Operand::Status => Some((owner.to_string(), Slot::Status)),
            Operand::CrossStatus {
                entity_type,
                id_field,
            } => related(entity_type, id_field, Slot::Status),
            Operand::CrossField {
                entity_type,
                id_field,
                field,
            } => related(entity_type, id_field, Slot::named(field)),
            Operand::Len(_) | Operand::Lit(_) => None,
        }
    }

    fn visit(&mut self, owner: &str, expr: &Expr) {
        match expr {
            Expr::Not(inner) => self.visit(owner, inner),
            Expr::And(parts) | Expr::Or(parts) => {
                parts.iter().for_each(|part| self.visit(owner, part));
            }
            Expr::Implies(a, b) => {
                self.visit(owner, a);
                self.visit(owner, b);
            }
            Expr::Compare { lhs, rhs, .. } => {
                match (self.term(owner, lhs), self.term(owner, rhs), lhs, rhs) {
                    (Some(a), Some(b), _, _) => self.link(&a, &b),
                    (Some(t), None, _, Operand::Lit(Literal::Str(text)))
                    | (None, Some(t), Operand::Lit(Literal::Str(text)), _) => {
                        self.hold(&t, text);
                    }
                    _ => {}
                }
            }
            Expr::In { value, set, .. } => {
                let value_term = self.term(owner, value);
                match (value_term, set, value) {
                    (Some(t), Set::List(items), _) => {
                        for item in items {
                            if let Literal::Str(text) = item {
                                self.hold(&t, text);
                            }
                        }
                    }
                    (Some(t), Set::Var(list), _) => {
                        self.link(&t, &(owner.to_string(), Slot::named(list)));
                    }
                    (None, Set::Var(list), Operand::Lit(Literal::Str(text))) => {
                        self.hold(&(owner.to_string(), Slot::named(list)), text);
                    }
                    _ => {}
                }
            }
            Expr::Const(_) | Expr::Empty(_) | Expr::Var(_) => {}
        }
    }

    fn root(&self, term: &Term) -> Term {
        let mut at = term;
        while let Some(up) = self.parent.get(at) {
            at = up;
        }
        at.clone()
    }

    fn hold(&mut self, term: &Term, literal: &str) {
        let root = self.root(term);
        self.held
            .entry(root)
            .or_default()
            .insert(literal.to_string());
    }

    fn hold_all(&mut self, term: &Term, literals: impl IntoIterator<Item = String>) {
        let root = self.root(term);
        self.held.entry(root).or_default().extend(literals);
    }

    fn link(&mut self, a: &Term, b: &Term) {
        let (a, b) = (self.root(a), self.root(b));
        if a == b {
            return;
        }
        let moved = self.held.remove(&a).unwrap_or_default();
        self.held.entry(b.clone()).or_default().extend(moved);
        self.parent.insert(a, b);
    }
}
