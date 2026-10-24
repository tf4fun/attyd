//! Content hashes of protocol revision 4 (docs/protocol-r4-design.md §4.6, §5).
//!
//! Text is hashed as a chain over fixed segments, so that a client can verify and continue a
//! window of a text it does not hold whole. Everything is `SHA-256`; node hashes are 32 bytes
//! and go on the wire as the unpadded base64url of their first 16 bytes (`digest::wire`).
#![cfg_attr(not(test), allow(dead_code))]

use sha2::{Digest, Sha256};

pub(crate) use super::digest::Hash;

/// The segment length of a text position, in UTF-8 bytes (§4.6). It is part of every text
/// hash: changing it changes the protocol.
pub(crate) const L: usize = 4096;

/// The chain state before the first segment.
pub(crate) const ZERO: Hash = [0; 32];

/// The segments of `text`: its UTF-8 bytes split greedily from the start into runs of at most
/// `L` bytes, each ending on a code point boundary. Appending to a text changes only its last
/// segment and adds new ones; earlier segments never change.
pub(crate) fn segments(text: &str) -> impl Iterator<Item = &str> {
    let mut rest = text;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let mut end = rest.len().min(L);
        while !rest.is_char_boundary(end) {
            end -= 1;
        }
        let (segment, tail) = rest.split_at(end);
        rest = tail;
        Some(segment)
    })
}

/// `c_i = H(c_{i-1} ‖ s_i)`: the chain state after `segment`.
pub(crate) fn chain_step(before: &Hash, segment: &str) -> Hash {
    let mut hasher = Sha256::new();
    hasher.update(before);
    hasher.update(segment.as_bytes());
    hasher.finalize().into()
}

/// The chain states after each segment of `text`.
pub(crate) fn chain_states(text: &str) -> Vec<Hash> {
    let mut state = ZERO;
    segments(text)
        .map(|segment| {
            state = chain_step(&state, segment);
            state
        })
        .collect()
}

/// `h = H("t" ‖ u64be(len) ‖ c_last)` from the chain state after the last segment (`ZERO` for
/// an empty text) and the text's length in UTF-8 bytes.
fn finish(length: u64, last: &Hash) -> Hash {
    let mut hasher = Sha256::new();
    hasher.update(b"t");
    hasher.update(length.to_be_bytes());
    hasher.update(last);
    hasher.finalize().into()
}

/// The hash of a whole text.
pub(crate) fn text_hash(text: &str) -> Hash {
    finish(text.len() as u64, &chain_states(text).last().copied().unwrap_or(ZERO))
}

/// The hash of a text of `length` bytes of which a client holds the tail `text`, starting at a
/// segment boundary whose chain state is `before`. It equals `text_hash` of the whole text.
pub(crate) fn window_hash(before: &Hash, length: u64, text: &str) -> Hash {
    let last = segments(text).fold(*before, |state, segment| chain_step(&state, segment));
    finish(length, &last)
}

#[cfg(test)]
mod tests;
