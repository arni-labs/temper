//! The digest a cascade's verdict is kept by.

use sha2::{Digest, Sha256};

use super::VerificationCascade;
use crate::model;

impl VerificationCascade {
    /// The digest of what the levels read: the model L0, L1 and L2 walk, the
    /// one L3 walks, and how far each level goes. Every level reads only
    /// these, so two specs with one digest get one verdict, and a change
    /// none of them reads (the text of a string no rule reads, a comment,
    /// the order of sections) keeps it. The budget is left out: a check
    /// that ran out of it has no verdict to keep.
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
        for part in [format!("{model:?}"), format!("{prop_model:?}"), settings] {
            hasher.update((part.len() as u64).to_le_bytes());
            hasher.update(part.as_bytes());
        }
        Some(format!("{:x}", hasher.finalize()))
    }
}
