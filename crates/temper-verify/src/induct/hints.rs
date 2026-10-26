//! The hints an agent gives a proof: written beside the rule in its spec,
//! or given by the rule's name.

use temper_spec::automaton::{ProofLemma, ProofRegion};

use super::live::{Measure, Region};

/// What an agent gives the proofs: lemmas, and for each `eventually` rule a
/// measure and the regions that say which system move brings it closer.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct Hints {
    #[serde(default)]
    pub lemma: Vec<Lemma>,
    #[serde(default)]
    pub measure: Vec<Measure>,
    #[serde(default)]
    pub region: Vec<Region>,
}

impl Hints {
    /// The hints for `rule`: those written beside it, then those `extra`
    /// gives by its name; a measure beside it replaces `extra`'s.
    pub(super) fn given(
        extra: &Hints,
        rule: &str,
        lemma: &[ProofLemma],
        measure: &[String],
        region: &[ProofRegion],
    ) -> Hints {
        let lemma = lemma
            .iter()
            .map(|l| Lemma {
                rule: rule.to_string(),
                name: l.name.clone(),
                assert: l.assert.clone(),
            })
            .chain(extra.lemma.iter().filter(|l| l.rule == rule).cloned())
            .collect();
        let measure = match measure {
            [] => extra
                .measure
                .iter()
                .filter(|m| m.rule == rule)
                .cloned()
                .collect(),
            terms => vec![Measure {
                rule: rule.to_string(),
                terms: terms.to_vec(),
            }],
        };
        let region = region
            .iter()
            .map(|r| Region {
                rule: rule.to_string(),
                name: r.name.clone(),
                region: r.region.clone(),
                witness: r.witness.clone(),
                step: r.step.clone(),
                params: r.params.clone(),
            })
            .chain(extra.region.iter().filter(|r| r.rule == rule).cloned())
            .collect();
        Hints {
            lemma,
            measure,
            region,
        }
    }
}

/// A lemma: a clause the proof holds alongside the rule.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Lemma {
    /// The rule it is for.
    pub rule: String,
    pub name: String,
    /// SMT-LIB over `$`-framed names.
    pub assert: String,
}
