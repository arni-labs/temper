use std::collections::BTreeMap;

use super::*;

/// A commit graph held in memory: sha -> parents.
struct Graph(BTreeMap<String, Vec<String>>);

impl Graph {
    fn new(edges: &[(&str, &[&str])]) -> Self {
        Self(
            edges
                .iter()
                .map(|(sha, parents)| {
                    (
                        sha.to_string(),
                        parents.iter().map(|p| p.to_string()).collect(),
                    )
                })
                .collect(),
        )
    }
}

impl CommitHistory for Graph {
    async fn parents(&self, sha: &str) -> Result<Option<Vec<String>>, String> {
        Ok(self.0.get(sha).cloned())
    }
}

/// root <- a <- b <- d (merge of b and c), root <- c; x is unrelated.
fn history() -> Graph {
    Graph::new(&[
        ("root", &[]),
        ("a", &["root"]),
        ("b", &["a"]),
        ("c", &["root"]),
        ("d", &["b", "c"]),
        ("x", &[]),
    ])
}

async fn compare(graph: &Graph, base: &str, head: &str) -> VersionComparison {
    compare_in_history(graph, base, head)
        .await
        .expect("both commits exist")
}

#[tokio::test]
async fn a_commit_compares_identical_to_itself() {
    assert_eq!(
        compare(&history(), "b", "b").await,
        VersionComparison::Identical
    );
}

#[tokio::test]
async fn a_descendant_is_ahead_and_its_ancestor_behind() {
    let graph = history();
    assert_eq!(compare(&graph, "a", "b").await, VersionComparison::Ahead);
    assert_eq!(compare(&graph, "b", "a").await, VersionComparison::Behind);
    assert_eq!(compare(&graph, "root", "d").await, VersionComparison::Ahead);
}

#[tokio::test]
async fn a_merge_descends_from_both_of_its_parents() {
    let graph = history();
    assert_eq!(compare(&graph, "c", "d").await, VersionComparison::Ahead);
    assert_eq!(compare(&graph, "b", "d").await, VersionComparison::Ahead);
}

#[tokio::test]
async fn siblings_and_unrelated_histories_diverge() {
    let graph = history();
    assert_eq!(compare(&graph, "b", "c").await, VersionComparison::Diverged);
    assert_eq!(compare(&graph, "x", "d").await, VersionComparison::Diverged);
}

#[tokio::test]
async fn a_missing_base_or_head_is_not_found() {
    for (base, head) in [("nope", "b"), ("b", "nope")] {
        let error = compare_in_history(&history(), base, head)
            .await
            .expect_err("a commit outside the history cannot be compared");
        assert!(error.contains("not found"), "{error}");
    }
}

#[tokio::test]
async fn a_parent_without_a_row_makes_the_answer_unknown() {
    // b's parent `gone` was never stored, so whether a is behind b cannot
    // be read; claiming "diverged" would be a guess.
    let graph = Graph::new(&[("a", &[]), ("b", &["gone"])]);
    assert_eq!(compare(&graph, "a", "b").await, VersionComparison::Unknown);
}

#[tokio::test]
async fn a_walk_past_the_bound_is_unknown_and_one_inside_it_is_answered() {
    // A straight line of commits: c0 <- c1 <- ... <- cN.
    let line = |len: usize| {
        let mut edges = BTreeMap::new();
        edges.insert("c0".to_string(), Vec::new());
        for i in 1..=len {
            edges.insert(format!("c{i}"), vec![format!("c{}", i - 1)]);
        }
        Graph(edges)
    };
    let inside = MAX_VERSION_COMPARE_WALK_COMMITS - 1;
    let graph = line(inside);
    assert_eq!(
        compare(&graph, "c0", &format!("c{inside}")).await,
        VersionComparison::Ahead,
        "an ancestor within the bound is found"
    );

    let past = MAX_VERSION_COMPARE_WALK_COMMITS + 10;
    let graph = line(past);
    assert_eq!(
        compare(&graph, "c0", &format!("c{past}")).await,
        VersionComparison::Unknown,
        "past the bound the walk says it cannot tell, never ahead or diverged"
    );
}

#[test]
fn a_compare_range_is_two_commit_ids() {
    let base = "1".repeat(40);
    let head = "a".repeat(40);
    assert_eq!(
        parse_compare_range(&format!("{base}...{head}")).expect("valid range"),
        (base.clone(), head.clone())
    );
    for bad in [
        format!("{base}..{head}"),
        format!("{base}...short"),
        format!("{}...{head}", "G".repeat(40)),
        base.clone(),
    ] {
        assert!(parse_compare_range(&bad).is_err(), "{bad} must be refused");
    }
}

#[test]
fn the_status_travels_as_lowercase_words() {
    let body = serde_json::to_value(GenesisVersionCompareResponse {
        status: VersionComparison::Ahead,
    })
    .expect("serialize");
    assert_eq!(body, serde_json::json!({"status": "ahead"}));
    for word in ["identical", "ahead", "behind", "diverged", "unknown"] {
        serde_json::from_value::<GenesisVersionCompareResponse>(
            serde_json::json!({ "status": word }),
        )
        .unwrap_or_else(|error| panic!("{word}: {error}"));
    }
}
