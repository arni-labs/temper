use super::*;

/// A packed state of `len` bytes that differs for each `n`.
fn packed(n: u32, len: usize) -> Vec<u8> {
    n.to_le_bytes().iter().copied().cycle().take(len).collect()
}

#[test]
fn every_state_is_found_again_and_read_back_whole_across_blocks() {
    let room = super::super::room::Bytes::new(16 * BLOCK);
    let progress = super::super::room::Progress::default();
    let mut held = Held::new(super::super::room::Bound {
        room: &room,
        progress: &progress,
    });
    let mut states = States::default();
    // 400-byte states: more than two blocks' worth, so some do not fit the
    // end of a block and start the next one.
    let count = 3 * BLOCK as u32 / 400;
    for n in 0..count {
        assert_eq!(states.add(&packed(n, 400), &mut held).unwrap(), (n, true));
    }
    for n in 0..count {
        assert_eq!(states.add(&packed(n, 400), &mut held).unwrap(), (n, false));
        assert_eq!(states.get(n as usize), packed(n, 400));
    }
    assert_eq!(states.len(), count as usize);
}

#[test]
fn a_state_bigger_than_a_block_is_kept_whole() {
    let room = super::super::room::Bytes::new(16 * BLOCK);
    let progress = super::super::room::Progress::default();
    let mut held = Held::new(super::super::room::Bound {
        room: &room,
        progress: &progress,
    });
    let mut states = States::default();
    states.add(&packed(1, 10), &mut held).unwrap();
    let big = packed(2, BLOCK + 1);
    assert_eq!(states.add(&big, &mut held).unwrap(), (1, true));
    states.add(&packed(3, 10), &mut held).unwrap();
    assert_eq!(states.get(0), packed(1, 10));
    assert_eq!(states.get(1), big);
    assert_eq!(states.get(2), packed(3, 10));
}

#[test]
fn refused_blocks_leave_the_states_unchanged() {
    use super::super::room::{Bound, Bytes, Progress};
    for size in [10, BLOCK + 1] {
        let room = Bytes::new(BLOCK / 2);
        let progress = Progress::default();
        {
            let mut held = Held::new(Bound {
                room: &room,
                progress: &progress,
            });
            let mut states = States::default();
            assert!(states.add(&packed(1, size), &mut held).is_err());
            assert_eq!(states.len(), 0);
            assert!(states.blocks.is_empty());
            assert!(states.index.is_empty());
        }
        assert_eq!(room.held(), 0);
    }
}

#[test]
fn refusing_the_next_block_preserves_existing_states() {
    use super::super::room::{Bound, Bytes, Progress};
    let room = Bytes::new(BLOCK + BLOCK / 2);
    let progress = Progress::default();
    let mut held = Held::new(Bound {
        room: &room,
        progress: &progress,
    });
    let mut states = States::default();
    let first = packed(1, BLOCK);
    assert_eq!(states.add(&first, &mut held).unwrap(), (0, true));
    assert!(held.bytes >= states.bytes());
    assert!(states.add(&packed(2, 10), &mut held).is_err());
    assert_eq!(states.len(), 1);
    assert_eq!(states.blocks.len(), 1);
    assert_eq!(states.get(0), first);
    assert_eq!(states.add(&first, &mut held).unwrap(), (0, false));
}
