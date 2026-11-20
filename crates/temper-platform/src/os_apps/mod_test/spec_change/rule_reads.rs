//! A changed rule is checked against what each stored record holds under
//! both versions: a value only the installed rule reads, and the initial
//! value a record that never stored a name reads, both decide whether the
//! record kept the rule before.
use super::*;

/// The first version with `note`, and a rule that lets a place sit at c1
/// only with a note of "ok".
fn noted(rule: &str) -> String {
    format!(
        "{PLACE}\n[[state]]\nname = \"note\"\ntype = \"string\"\ninitial = \"\"\n\n\
         [[field_invariant]]\nname = \"Allowed\"\nassert = \"{rule}\"\n"
    )
}

/// p1 sits at c1 with a note of "ok", so it kept the installed rule; the new
/// rule forbids c1 outright. The install reads p1's note to know it held.
#[tokio::test]
async fn a_rule_that_stops_reading_a_value_is_checked_against_the_value_held() {
    let places = Fixture::new("old-reads").await;
    places
        .install(&noted("commit != 'c1' || note == 'ok'"))
        .await
        .expect("first version");
    places
        .placed_with("p1", serde_json::json!({ "note": "ok" }))
        .await;

    let refused = places
        .install(&noted("commit != 'c1'"))
        .await
        .expect_err("p1 kept the rule at c1 with its note and breaks the new one");
    assert!(
        refused.contains("Allowed") && refused.contains("p1"),
        "{refused}"
    );
}

/// The rule reads `allowed`, which p1 never stored, so p1 reads its initial
/// value: c1 under the second version, and c2 under the third, which changes
/// nothing but that value.
fn allowing(initial: &str) -> String {
    format!(
        "{PLACE}\n[[state]]\nname = \"allowed\"\ntype = \"string\"\ninitial = \"{initial}\"\n\n\
         [[field_invariant]]\nname = \"OnlyAllowed\"\n\
         assert = \"commit == '' || commit == allowed\"\n"
    )
}

#[tokio::test]
async fn a_rule_whose_initial_value_changes_is_checked_again() {
    let places = Fixture::new("defaults").await;
    places.install(PLACE).await.expect("first version");
    places.placed("p1").await;
    places
        .install(&allowing("c1"))
        .await
        .expect("p1 is at c1, which the rule allows");
    let p1 = places
        .state
        .server
        .get_tenant_entity_state(&places.tenant, "Place", "p1")
        .await
        .expect("read p1")
        .state;
    assert!(
        p1.fields.get("allowed").is_none(),
        "p1 must read the initial value: {:?}",
        p1.fields
    );

    let refused = places
        .install(&allowing("c2"))
        .await
        .expect_err("p1 now reads c2 as allowed, and it is at c1");
    assert!(
        refused.contains("OnlyAllowed") && refused.contains("p1"),
        "{refused}"
    );
}

/// A commit too long to keep inline: it is stored out of line, and the
/// record holds a reference to it.
const LONG: &str = "c1-0123456789abcdef0123456789abcdef0123";

/// The place, keeping only 16 bytes of a commit inline, with `rule`.
fn long_commits(rule: &str) -> String {
    let spilling = PLACE.replacen(
        "name = \"commit\"\ntype = \"string\"\ninitial = \"\"\n",
        "name = \"commit\"\ntype = \"string\"\ninitial = \"\"\noverflow_inline_max_bytes = 16\n",
        1,
    );
    assert_ne!(spilling, PLACE, "the commit's declaration moved");
    if rule.is_empty() {
        return spilling;
    }
    format!("{spilling}\n[[field_invariant]]\nname = \"Commit\"\nassert = \"{rule}\"\n")
}

/// A rule is checked against the commit p1 holds, not the reference to it:
/// one that forbids that commit is refused, and one that asks for it is
/// installed.
#[tokio::test]
async fn a_rule_reads_a_value_stored_out_of_line_as_the_value() {
    let places = Fixture::new("spilled").await;
    places
        .install(&long_commits(""))
        .await
        .expect("first version");
    places.create("p1").await;
    let brought = places
        .act("p1", "Bring", serde_json::json!({ "commit": LONG }))
        .await;
    assert_eq!(brought, (true, "Bringing".to_string()));
    let journaled = places
        .state
        .server
        .get_tenant_entity_state(&places.tenant, "Place", "p1")
        .await
        .expect("read p1")
        .state;
    assert!(
        journaled.fields["commit"]
            .get("__temper_blob_ref")
            .is_some(),
        "the commit must be stored out of line: {:?}",
        journaled.fields
    );

    let refused = places
        .install(&long_commits(&format!("commit != '{LONG}'")))
        .await
        .expect_err("p1 holds the commit the new rule forbids");
    assert!(
        refused.contains("Commit") && refused.contains("p1"),
        "{refused}"
    );
    places
        .install(&long_commits(&format!(
            "commit == '' || commit == '{LONG}'"
        )))
        .await
        .expect("p1 holds the commit the new rule asks for");
}
