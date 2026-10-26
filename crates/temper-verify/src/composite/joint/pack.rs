//! Joint states packed small for the walk. Every string is a number in one
//! table, and every number is written in as few bytes as it needs, seven
//! bits to a byte, so a walked state is a short run of bytes rather than
//! maps on the heap: a state of the Katagami control specs takes about
//! 12.7 KB unpacked, and nearly every number in it fits one byte.

use std::collections::{BTreeMap, HashMap};

use super::state::{Call, Dispatch, JointState};
use crate::model::{ParamValue, TemperModelState};

/// The table of strings packed states refer to.
#[derive(Default)]
pub(super) struct Packer {
    ids: HashMap<String, u32>,
    names: Vec<String>,
    /// Bytes the strings take on the heap, each held twice.
    pub(super) heap: usize,
}

impl Packer {
    /// Write `state` into `out`, in place of what it held.
    pub(super) fn pack(&mut self, state: &JointState, out: &mut Vec<u8>) {
        out.clear();
        state.put(self, out);
    }

    pub(super) fn unpack(&self, mut packed: &[u8]) -> JointState {
        JointState::take(self, &mut packed)
    }

    /// The number for `name`, added to the table when new.
    pub(super) fn id(&mut self, name: &str) -> u32 {
        if let Some(&id) = self.ids.get(name) {
            return id;
        }
        let id = u32::try_from(self.names.len()).expect("fewer than 2^32 strings");
        self.ids.insert(name.to_string(), id);
        self.names.push(name.to_string());
        self.heap += 2 * super::room::block(name.len());
        id
    }

    pub(super) fn name(&self, id: u32) -> &str {
        &self.names[id as usize]
    }

    /// Bytes the table holds: its strings, and its list and index at their
    /// capacity.
    pub(super) fn bytes(&self) -> usize {
        self.heap
            + self.names.capacity() * size_of::<String>()
            + super::room::map_bytes::<String, u32>(self.ids.capacity())
    }
}

/// A value written as numbers and read back.
trait Pack: Sized {
    fn put(&self, packer: &mut Packer, out: &mut Vec<u8>);
    fn take(packer: &Packer, input: &mut &[u8]) -> Self;
}

/// Write `number` seven bits to a byte, low bits first, with the top bit of
/// each byte but the last set.
fn put_number(out: &mut Vec<u8>, mut number: u32) {
    while number >= 0x80 {
        out.push((number & 0x7f) as u8 | 0x80);
        number >>= 7;
    }
    out.push(number as u8);
}

/// Read a number [`put_number`] wrote.
fn take_number(input: &mut &[u8]) -> u32 {
    let mut number = 0;
    let mut shift = 0;
    loop {
        let (&byte, rest) = input
            .split_first()
            .expect("a packed state is read as it was written");
        *input = rest;
        number |= u32::from(byte & 0x7f) << shift;
        if byte < 0x80 {
            return number;
        }
        shift += 7;
    }
}

impl Pack for u32 {
    fn put(&self, _: &mut Packer, out: &mut Vec<u8>) {
        put_number(out, *self);
    }
    fn take(_: &Packer, input: &mut &[u8]) -> Self {
        take_number(input)
    }
}

impl Pack for usize {
    fn put(&self, _: &mut Packer, out: &mut Vec<u8>) {
        put_number(
            out,
            u32::try_from(*self).expect("the model bounds its counts"),
        );
    }
    fn take(_: &Packer, input: &mut &[u8]) -> Self {
        take_number(input) as usize
    }
}

impl Pack for bool {
    fn put(&self, _: &mut Packer, out: &mut Vec<u8>) {
        put_number(out, u32::from(*self));
    }
    fn take(_: &Packer, input: &mut &[u8]) -> Self {
        take_number(input) != 0
    }
}

impl Pack for String {
    fn put(&self, packer: &mut Packer, out: &mut Vec<u8>) {
        let id = packer.id(self);
        put_number(out, id);
    }
    fn take(packer: &Packer, input: &mut &[u8]) -> Self {
        packer.name(take_number(input)).to_string()
    }
}

impl<T: Pack> Pack for Option<T> {
    fn put(&self, packer: &mut Packer, out: &mut Vec<u8>) {
        put_number(out, u32::from(self.is_some()));
        if let Some(value) = self {
            value.put(packer, out);
        }
    }
    fn take(packer: &Packer, input: &mut &[u8]) -> Self {
        (take_number(input) != 0).then(|| T::take(packer, input))
    }
}

impl<T: Pack> Pack for Vec<T> {
    fn put(&self, packer: &mut Packer, out: &mut Vec<u8>) {
        self.len().put(packer, out);
        for value in self {
            value.put(packer, out);
        }
    }
    fn take(packer: &Packer, input: &mut &[u8]) -> Self {
        let len = usize::take(packer, input);
        (0..len).map(|_| T::take(packer, input)).collect()
    }
}

impl<K: Pack + Ord, V: Pack> Pack for BTreeMap<K, V> {
    fn put(&self, packer: &mut Packer, out: &mut Vec<u8>) {
        self.len().put(packer, out);
        for (key, value) in self {
            key.put(packer, out);
            value.put(packer, out);
        }
    }
    fn take(packer: &Packer, input: &mut &[u8]) -> Self {
        let len = usize::take(packer, input);
        (0..len)
            .map(|_| (K::take(packer, input), V::take(packer, input)))
            .collect()
    }
}

impl Pack for ParamValue {
    fn put(&self, packer: &mut Packer, out: &mut Vec<u8>) {
        match self {
            ParamValue::Count(count) => {
                put_number(out, 0);
                count.put(packer, out);
            }
            ParamValue::Bool(value) => {
                put_number(out, 1);
                value.put(packer, out);
            }
            ParamValue::Str(text) => {
                put_number(out, 2);
                text.put(packer, out);
            }
            ParamValue::Fresh => put_number(out, 3),
        }
    }
    fn take(packer: &Packer, input: &mut &[u8]) -> Self {
        match take_number(input) {
            0 => ParamValue::Count(usize::take(packer, input)),
            1 => ParamValue::Bool(bool::take(packer, input)),
            2 => ParamValue::Str(String::take(packer, input)),
            _ => ParamValue::Fresh,
        }
    }
}

impl Pack for TemperModelState {
    fn put(&self, packer: &mut Packer, out: &mut Vec<u8>) {
        self.status.put(packer, out);
        self.counters.put(packer, out);
        self.booleans.put(packer, out);
        self.lists.put(packer, out);
        self.strings.put(packer, out);
    }
    fn take(packer: &Packer, input: &mut &[u8]) -> Self {
        TemperModelState {
            status: Pack::take(packer, input),
            counters: Pack::take(packer, input),
            booleans: Pack::take(packer, input),
            lists: Pack::take(packer, input),
            strings: Pack::take(packer, input),
        }
    }
}

impl Pack for Dispatch {
    fn put(&self, packer: &mut Packer, out: &mut Vec<u8>) {
        self.to.put(packer, out);
        self.action.put(packer, out);
        self.target_state.put(packer, out);
        self.params.put(packer, out);
        self.read.put(packer, out);
        self.depth.put(packer, out);
        self.via.put(packer, out);
        self.by.put(packer, out);
        self.inline.put(packer, out);
    }
    fn take(packer: &Packer, input: &mut &[u8]) -> Self {
        Dispatch {
            to: Pack::take(packer, input),
            action: Pack::take(packer, input),
            target_state: Pack::take(packer, input),
            params: Pack::take(packer, input),
            read: Pack::take(packer, input),
            depth: Pack::take(packer, input),
            via: Pack::take(packer, input),
            by: Pack::take(packer, input),
            inline: Pack::take(packer, input),
        }
    }
}

impl Pack for Call {
    fn put(&self, packer: &mut Packer, out: &mut Vec<u8>) {
        self.to.put(packer, out);
        self.module.put(packer, out);
        self.on_success.put(packer, out);
        self.on_failure.put(packer, out);
        self.by.put(packer, out);
        self.asked.put(packer, out);
    }
    fn take(packer: &Packer, input: &mut &[u8]) -> Self {
        Call {
            to: Pack::take(packer, input),
            module: Pack::take(packer, input),
            on_success: Pack::take(packer, input),
            on_failure: Pack::take(packer, input),
            by: Pack::take(packer, input),
            asked: Pack::take(packer, input),
        }
    }
}

impl Pack for JointState {
    fn put(&self, packer: &mut Packer, out: &mut Vec<u8>) {
        self.records.put(packer, out);
        self.inflight.put(packer, out);
        self.calls.put(packer, out);
        self.armed.put(packer, out);
    }
    fn take(packer: &Packer, input: &mut &[u8]) -> Self {
        JointState {
            records: Pack::take(packer, input),
            inflight: Pack::take(packer, input),
            calls: Pack::take(packer, input),
            armed: Pack::take(packer, input),
        }
    }
}

#[cfg(test)]
#[path = "pack_test.rs"]
mod tests;
