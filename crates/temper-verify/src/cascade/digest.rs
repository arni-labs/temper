//! The digest a cascade's verdict is kept by.

use sha2::{Digest, Sha256};

use super::VerificationCascade;
use crate::model::{self, TemperModel};

impl VerificationCascade {
    /// The digest of what the levels read: the model L0, L1 and L2 walk, the
    /// one L3 walks, and how far each level goes. Every level reads only
    /// these, so two specs with one digest get one verdict, and a change
    /// none of them reads (the text of a string no rule reads, a comment,
    /// the order of sections) keeps it. The budget is left out: a check
    /// that ran out of it has no verdict to keep.
    ///
    /// A rule that reads a related record (`Type[id].status`) names its
    /// type here by first use, not by name: the levels read the related
    /// record's status without knowing its type, so the same spec in two
    /// apps, whose types differ only by the app's prefix, has one digest.
    ///
    /// `None` when the cascade reads more than its models (an actor
    /// simulation, path extraction or a composite scope), or the spec does
    /// not parse.
    pub fn digest(&self) -> Option<String> {
        if self.actor_sim_runner.is_some()
            || self.path_extraction_config.is_some()
            || self.composite_scope.is_some()
        {
            return None;
        }
        let model = model::build_model_from_ioa(&self.ioa_source, self.max_counter).ok()?;
        let prop_model = model::build_model_from_ioa(&self.ioa_source, 2).ok()?;
        let related = related_types(&model);
        let settings = format!(
            "max_counter={} sim_seeds={} sim_ticks={} prop_test_cases={} prop_test_max_steps={} fail_fast={}",
            self.max_counter,
            self.sim_seeds,
            self.sim_ticks,
            self.prop_test_cases,
            self.prop_test_max_steps,
            self.fail_fast,
        );
        // Each part is length-prefixed so no two inputs run together into
        // the same bytes.
        let mut hasher = Sha256::new();
        for part in [
            by_first_use(&model, &related),
            by_first_use(&prop_model, &related),
            settings,
        ] {
            hasher.update((part.len() as u64).to_le_bytes());
            hasher.update(part.as_bytes());
        }
        Some(format!("{:x}", hasher.finalize()))
    }
}

/// The types `model`'s guards and rules read a related record of, in
/// first-use order.
fn related_types(model: &TemperModel) -> Vec<String> {
    let guards = model.transitions.iter().map(|t| &t.guard);
    let rules = model.invariants.iter().map(|i| &i.assert);
    let mut types: Vec<String> = Vec::new();
    for (entity_type, _) in guards.chain(rules).flat_map(|expr| expr.related_types()) {
        if !types.contains(&entity_type) {
            types.push(entity_type);
        }
    }
    types
}

/// `model` as it prints, with each of `related` named by its place in it.
/// Only a related record's operand prints an `entity_type` field.
fn by_first_use(model: &TemperModel, related: &[String]) -> String {
    let mut text = format!("{model:?}");
    for (place, entity_type) in related.iter().enumerate() {
        text = text.replace(
            &format!("entity_type: {entity_type:?}"),
            &format!("entity_type: \"#{place}\""),
        );
    }
    text
}
