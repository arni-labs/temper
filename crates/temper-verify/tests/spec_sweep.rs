//! Runs the cascade over the repo's specs (measurement only, scratch branches).

#[test]
#[ignore]
fn spec_sweep() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../../");
    let mut lines = Vec::new();
    for path in include_str!("sweep_list.txt").lines().filter(|l| !l.is_empty()) {
        let Ok(src) = std::fs::read_to_string(format!("{root}{path}")) else {
            lines.push(format!("{path}\tMISSING"));
            continue;
        };
        if let Err(e) = temper_spec::automaton::parse_automaton(&src) {
            lines.push(format!("{path}\tUNPARSED\t0\t{e}"));
            continue;
        }
        let started = std::time::Instant::now();
        let result = std::panic::catch_unwind(|| {
            temper_verify::VerificationCascade::from_ioa(&src)
                .with_sim_seeds(3)
                .with_prop_test_cases(20)
                .run()
        });
        let ms = started.elapsed().as_millis();
        match result {
            Ok(r) => {
                let levels: Vec<String> = r
                    .levels
                    .iter()
                    .map(|l| l.summary.replace(['\t', '\n'], " "))
                    .collect();
                lines.push(format!("{path}\t{}\t{ms}\t{}", r.all_passed, levels.join(" | ")));
            }
            Err(_) => lines.push(format!("{path}\tPANIC\t{ms}")),
        }
    }
    for line in &lines {
        println!("SWEEP\t{line}");
    }
    panic!("sweep done: {} specs", lines.len());
}
