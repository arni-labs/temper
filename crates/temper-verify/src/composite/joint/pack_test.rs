use super::*;

#[test]
fn a_number_reads_back_as_it_was_written_in_as_few_bytes_as_it_needs() {
    for (number, bytes) in [
        (0, 1),
        (127, 1),
        (128, 2),
        (16_383, 2),
        (16_384, 3),
        (u32::MAX, 5),
    ] {
        let mut out = Vec::new();
        put_number(&mut out, number);
        assert_eq!(out.len(), bytes, "{number}");
        let mut input = out.as_slice();
        assert_eq!(take_number(&mut input), number);
        assert!(input.is_empty(), "{number} read to its end");
    }
}

#[test]
fn packed_call_snapshots_keep_literals_distinct_from_symbols() {
    let state = JointState {
        records: BTreeMap::new(),
        inflight: Vec::new(),
        calls: vec![Call {
            to: "T".into(),
            module: "worker".into(),
            on_success: Vec::new(),
            on_failure: None,
            by: 0,
            asked: BTreeMap::from([
                ("literal".into(), ModelString::from("#1")),
                ("fresh".into(), ModelString::Symbol(1)),
            ]),
        }],
        armed: BTreeMap::new(),
    };
    let mut packer = Packer::default();
    let mut packed = Vec::new();
    packer.pack(&state, &mut packed);
    let restored = packer.unpack(&packed);
    assert_eq!(restored, state);
    assert_ne!(
        restored.calls[0].asked["literal"],
        restored.calls[0].asked["fresh"]
    );
}
