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
fn packed_dispatches_keep_literals_distinct_from_symbols() {
    let state = JointState {
        records: BTreeMap::new(),
        inflight: vec![Dispatch {
            to: "T".into(),
            action: "Go".into(),
            target_state: None,
            params: BTreeMap::from([
                ("literal".into(), ParamValue::Str(ModelString::from("#1"))),
                ("fresh".into(), ParamValue::Str(ModelString::Symbol(1))),
            ]),
            read: false,
            depth: 1,
            via: None,
            by: 0,
            inline: false,
        }],
        calls: Vec::new(),
        armed: BTreeMap::new(),
    };
    let mut packer = Packer::default();
    let mut packed = Vec::new();
    packer.pack(&state, &mut packed);
    let restored = packer.unpack(&packed);
    assert_eq!(restored, state);
    assert_ne!(
        restored.inflight[0].params["literal"],
        restored.inflight[0].params["fresh"]
    );
}
