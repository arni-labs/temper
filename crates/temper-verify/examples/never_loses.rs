//! Measurement only: `never_loses <file of spec paths>`.

fn main() {
    let list = std::env::args().nth(1).expect("file of spec paths");
    for path in std::fs::read_to_string(list).unwrap().lines() {
        if path.is_empty() {
            continue;
        }
        let src = std::fs::read_to_string(path).unwrap();
        for line in temper_verify::never_loses::measure(path, &src) {
            println!("{line}");
        }
    }
}
