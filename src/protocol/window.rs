//! Text windows (docs/protocol-r4-design.md §6.4): a client that follows a path with
//! `window=<path>:<N>` holds each text position under it as the shortest run of whole trailing
//! segments of at least `N` bytes, with what it needs to verify and continue the hash.
#![cfg_attr(not(test), allow(dead_code))]

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Serialize;

use super::hash::{self, Hash, L, ZERO};

/// A text position held as a window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct TextWindow {
    /// UTF-8 bytes of the whole text.
    pub length: u64,
    /// Bytes before the window: a segment boundary.
    pub offset: u64,
    /// The chain state before the window's first segment: 32 bytes, unpadded base64url; all
    /// zeros when `offset` is 0.
    pub before: String,
    /// The segments from `offset` to the end.
    pub text: String,
}

/// The chain state as it goes on the wire.
pub(crate) fn state_wire(state: &Hash) -> String {
    URL_SAFE_NO_PAD.encode(state)
}

/// The byte offset of the first segment of the window of `text` for `bytes = N`: the shortest
/// run of whole trailing segments of at least `N` bytes, or 0 when the text is shorter.
pub(crate) fn window_offset(text: &str, bytes: usize) -> usize {
    let lengths: Vec<usize> = hash::segments(text).map(str::len).collect();
    let mut held = 0;
    for length in lengths.iter().rev() {
        if held >= bytes {
            break;
        }
        held += length;
    }
    if held >= bytes { text.len() - held } else { 0 }
}

/// The chain state before the segment of `text` that starts at `offset` (a segment boundary).
pub(crate) fn state_before(text: &str, offset: usize) -> Hash {
    hash::segments(&text[..offset]).fold(ZERO, |state, segment| hash::chain_step(&state, segment))
}

/// The window of `text` for `bytes = N`.
pub(crate) fn window(text: &str, bytes: usize) -> TextWindow {
    let offset = window_offset(text, bytes);
    TextWindow {
        length: text.len() as u64,
        offset: offset as u64,
        before: state_wire(&state_before(text, offset)),
        text: text[offset..].to_string(),
    }
}

/// Whether appending `appended` bytes to a window is sent as an `append` (design §6.4): the
/// decision depends on the appended bytes alone, so a client needs at most the window it holds
/// plus one event's text.
pub(crate) fn appends(appended: usize, bytes: usize) -> bool {
    appended < bytes + L
}

#[cfg(test)]
mod tests;
