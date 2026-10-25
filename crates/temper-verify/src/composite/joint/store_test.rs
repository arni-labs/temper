use super::*;

/// A packed state of `len` bytes that differs for each `n`.
fn packed(n: u32, len: usize) -> Vec<u8> {
    n.to_le_bytes().iter().copied().cycle().take(len).collect()
}

#[test]
fn every_state_is_found_again_and_read_back_whole_across_blocks() {
    let mut states = States::default();
    // 400-byte states: more than two blocks' worth, so some do not fit the
    // end of a block and start the next one.
    let count = 3 * BLOCK as u32 / 400;
    for n in 0..count {
        assert_eq!(states.add(&packed(n, 400)), (n, true));
    }
    for n in 0..count {
        assert_eq!(states.add(&packed(n, 400)), (n, false));
        assert_eq!(states.get(n as usize), packed(n, 400));
    }
    assert_eq!(states.len(), count as usize);
}

#[test]
fn a_state_bigger_than_a_block_is_kept_whole() {
    let mut states = States::default();
    states.add(&packed(1, 10));
    let big = packed(2, BLOCK + 1);
    assert_eq!(states.add(&big), (1, true));
    states.add(&packed(3, 10));
    assert_eq!(states.get(0), packed(1, 10));
    assert_eq!(states.get(1), big);
    assert_eq!(states.get(2), packed(3, 10));
}
