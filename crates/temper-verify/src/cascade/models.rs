//! The models a cascade's levels walk, built from its source.

use super::{CascadeLevel, LevelResult, VerificationCascade};
use crate::budget::Spending;
use crate::model::{self, TemperModel};

impl VerificationCascade {
    /// The model L0-L2 walk and the smaller one L3 walks, each limited by the
    /// budget being spent. The source is anyone's, so one that does not parse
    /// is the first level failed, naming why, never a panic, and so is one an
    /// install would refuse.
    pub(super) fn models(
        &self,
        spending: Option<&Spending>,
    ) -> Result<(TemperModel, TemperModel), Box<LevelResult>> {
        if let Some(refused) = install_refusal(&self.ioa_source) {
            return Err(Box::new(refused));
        }
        let build = |max_counter: usize| -> Result<TemperModel, String> {
            let model = model::build_model_from_ioa(&self.ioa_source, max_counter)?;
            Ok(match spending {
                Some(spending) => spending.limit(model),
                None => model,
            })
        };
        let both = || -> Result<_, String> { Ok((build(self.max_counter)?, build(2)?)) };
        both().map_err(|error| Box::new(does_not_parse(error)))
    }
}

/// The first level, failed, naming each finding an install or upgrade
/// refuses ([`temper_spec::automaton::lint_install_refusals`]), so a check
/// says before an install what the install would refuse, with the fix in
/// its words. `None` for a spec that does not parse: building its model
/// says why.
pub(super) fn install_refusal(source: &str) -> Option<LevelResult> {
    let automaton = temper_spec::automaton::parse_automaton(source).ok()?;
    let findings =
        temper_spec::automaton::lint_install_refusals(&automaton, &automaton.automaton.name);
    if findings.is_empty() {
        return None;
    }
    let messages: Vec<String> = findings.into_iter().map(|f| f.message).collect();
    Some(LevelResult {
        level: CascadeLevel::SymbolicVerification,
        passed: false,
        summary: format!(
            "L0 Symbolic FAILED: an install refuses this spec: {}",
            messages.join("; ")
        ),
        diagnostics: messages,
        verification: None,
        simulation: None,
        prop_test: None,
        smt: None,
    })
}

/// The first level, failed, naming why the spec does not parse. The levels
/// after it have no model to walk, so they do not run.
fn does_not_parse(error: String) -> LevelResult {
    LevelResult {
        level: CascadeLevel::SymbolicVerification,
        passed: false,
        summary: format!("L0 Symbolic FAILED: the spec does not parse: {error}"),
        diagnostics: vec![error],
        verification: None,
        simulation: None,
        prop_test: None,
        smt: None,
    }
}

#[cfg(test)]
mod tests {
    use crate::cascade::VerificationCascade;

    /// A bool written as a string is the old syntax this kernel refuses.
    const OLD_SYNTAX: &str = r#"
[automaton]
name = "Stall"
states = ["Open", "Closed"]
initial = "Open"

[[state]]
name = "stalls"
type = "bool"
initial = "false"

[[action]]
name = "Close"
from = ["Open"]
to = "Closed"
"#;

    /// `Open` takes `note`, but the record's own `note` replaces it after
    /// the move: an install refuses it.
    const LOST_PARAM: &str = r#"
[automaton]
name = "Memo"
states = ["Draft", "Sent"]
initial = "Draft"

[[state]]
name = "note"
type = "string"
initial = ""

[[action]]
name = "Send"
from = ["Draft"]
to = "Sent"
params = [{ name = "note", type = "string" }]
"#;

    #[test]
    fn a_spec_an_install_refuses_fails_its_first_level_and_keeps_no_verdict() {
        let result = VerificationCascade::from_ioa(LOST_PARAM)
            .with_sim_seeds(1)
            .with_prop_test_cases(1)
            .run();
        assert!(!result.all_passed);
        assert_eq!(result.levels.len(), 1, "{result:?}");
        let level = &result.levels[0];
        assert!(
            level.summary.contains("an install refuses this spec")
                && level.summary.contains("'note'")
                && level.summary.contains("note = params.note"),
            "{}",
            level.summary
        );
        assert!(
            VerificationCascade::from_ioa(LOST_PARAM)
                .kept_key()
                .is_none(),
            "a verdict kept before the refusal must not answer for it"
        );

        let stored = LOST_PARAM.replace(
            "params = [{ name = \"note\", type = \"string\" }]",
            "params = [{ name = \"note\", type = \"string\" }]\neffect = [\"note = params.note\"]",
        );
        let result = VerificationCascade::from_ioa(&stored)
            .with_sim_seeds(1)
            .with_prop_test_cases(1)
            .run();
        assert!(
            result
                .levels
                .iter()
                .all(|level| !level.summary.contains("an install refuses")),
            "{result:?}"
        );
        assert!(VerificationCascade::from_ioa(&stored).kept_key().is_some());
    }

    #[test]
    fn a_spec_that_does_not_parse_fails_its_first_level_without_panicking() {
        let result = VerificationCascade::from_ioa(OLD_SYNTAX)
            .with_sim_seeds(1)
            .with_prop_test_cases(1)
            .run();
        assert!(!result.all_passed);
        assert_eq!(result.levels.len(), 1, "{result:?}");
        let level = &result.levels[0];
        assert!(!level.passed);
        assert!(
            level.summary.contains("does not parse")
                && level.summary.contains("migrate-predicates"),
            "{}",
            level.summary
        );
        assert!(
            VerificationCascade::from_ioa(OLD_SYNTAX)
                .kept_key()
                .is_none(),
            "a spec that does not parse has no verdict to keep"
        );
    }
}
