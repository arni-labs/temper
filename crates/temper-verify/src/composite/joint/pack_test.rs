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
