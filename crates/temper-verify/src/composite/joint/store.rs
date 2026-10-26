//! The joint states a walk has seen, each packed ([`super::pack`]) and
//! written once, after the one before it, into large blocks of bytes, and
//! found again by a hash of its bytes.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hasher};

use super::room::{Held, block, grow, grow_map, map_bytes};

/// Bytes in one block of packed states. A block is made whole and never
/// grows, so the table never copies the states it holds.
const BLOCK: usize = 1 << 20;

/// Where a packed state starts.
#[derive(Clone, Copy)]
struct Start {
    block: u32,
    offset: u32,
}

/// The packed states a walk has seen, in the order it saw them.
#[derive(Default)]
pub(super) struct States {
    /// The packed states. A state never spans two blocks; it ends where the
    /// next state in its block starts, or where its block's bytes end.
    blocks: Vec<Vec<u8>>,
    starts: Vec<Start>,
    /// Each state's index, by the hash of its bytes.
    index: HashMap<u64, u32>,
    /// The states whose hash an earlier, different state has.
    collided: HashMap<Box<[u8]>, u32>,
    /// Bytes the blocks and the collided states' copies take.
    heap: usize,
}

impl States {
    pub(super) fn len(&self) -> usize {
        self.starts.len()
    }

    /// The bytes of the state seen `index`th.
    pub(super) fn get(&self, index: usize) -> &[u8] {
        let Start { block, offset } = self.starts[index];
        let bytes = &self.blocks[block as usize];
        let end = match self.starts.get(index + 1) {
            Some(next) if next.block == block => next.offset as usize,
            _ => bytes.len(),
        };
        &bytes[offset as usize..end]
    }

    /// The index of the state packed as `bytes`, and whether it is new.
    /// A new state is added.
    pub(super) fn add(&mut self, bytes: &[u8], held: &mut Held<'_>) -> Result<(u32, bool), String> {
        let mut hasher = DefaultHasher::new();
        hasher.write(bytes);
        let hash = hasher.finish();
        let Some(&first) = self.index.get(&hash) else {
            self.reserve(1, held)?;
            let at = self.push(bytes, held)?;
            self.index.insert(hash, at);
            return Ok((at, true));
        };
        if self.get(first as usize) == bytes {
            return Ok((first, false));
        }
        if let Some(&at) = self.collided.get(bytes) {
            return Ok((at, false));
        }
        self.reserve(1, held)?;
        let before = self.bytes();
        grow_map(&mut self.collided, 1, held)?;
        held.add(self.bytes() - before)?;
        held.add(block(bytes.len()))?;
        let at = self.push(bytes, held)?;
        self.collided.insert(bytes.into(), at);
        self.heap += block(bytes.len());
        Ok((at, true))
    }

    fn push(&mut self, bytes: &[u8], held: &mut Held<'_>) -> Result<u32, String> {
        let fits = self
            .blocks
            .last()
            .is_some_and(|last| last.capacity() - last.len() >= bytes.len());
        if !fits {
            let size = BLOCK.max(bytes.len());
            let before = super::room::bytes(&self.blocks);
            grow(&mut self.blocks, 1, held)?;
            held.add(super::room::bytes(&self.blocks) - before)?;
            held.add(size)?;
            self.blocks.push(Vec::with_capacity(size));
            self.heap += size;
        }
        let last = self.blocks.len() - 1;
        let block = &mut self.blocks[last];
        self.starts.push(Start {
            block: u32::try_from(last).expect("fewer than 2^32 blocks"),
            offset: u32::try_from(block.len()).expect("a block holds less than 4 GB"),
        });
        block.extend_from_slice(bytes);
        Ok(u32::try_from(self.starts.len() - 1).expect("fewer than 2^32 states"))
    }

    /// Make the lists that grow by one entry for each state hold `spare`
    /// more states, as [`grow`] does. A new block is made whole when a state
    /// does not fit, with its memory taken before allocation.
    pub(super) fn reserve(&mut self, spare: usize, held: &mut Held<'_>) -> Result<(), String> {
        let before = self.bytes();
        grow(&mut self.starts, spare, held)?;
        held.add(self.bytes() - before)?;
        let before = self.bytes();
        grow_map(&mut self.index, spare, held)?;
        held.add(self.bytes() - before)
    }

    /// Bytes the table holds.
    pub(super) fn bytes(&self) -> usize {
        self.heap
            + super::room::bytes(&self.blocks)
            + super::room::bytes(&self.starts)
            + map_bytes::<u64, u32>(self.index.capacity())
            + map_bytes::<Box<[u8]>, u32>(self.collided.capacity())
    }
}

#[cfg(test)]
#[path = "store_test.rs"]
mod tests;
