//! Measurement only (scratch branches): never-loses-a-place over the repo's specs.

#[test]
#[ignore]
fn never_loses_sweep() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../../");
    let mut lines = Vec::new();
    for path in include_str!("sweep_list.txt")
        .lines()
        .filter(|l| !l.is_empty())
    {
        let src = std::fs::read_to_string(format!("{root}{path}")).unwrap();
        lines.extend(temper_verify::never_loses::measure(path, &src));
    }
    for line in &lines {
        println!("{line}");
    }
    panic!("measured {} lines", lines.len());
}
