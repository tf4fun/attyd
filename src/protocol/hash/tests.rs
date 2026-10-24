//! The revision 4 hashes against the vectors an independent implementation computed
//! (tests/fixtures/protocol_r4_vectors.py); the TypeScript hashes reproduce the same file.
use serde_json::Value;

use super::*;

fn vectors() -> Value {
    serde_json::from_str(include_str!("../../../tests/fixtures/protocol-r4-hash-vectors.json"))
        .expect("the vectors are JSON")
}

fn hex(hash: &[u8]) -> String {
    hash.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn unhex(text: &str) -> Hash {
    let mut hash = [0u8; 32];
    for (index, byte) in hash.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).unwrap();
    }
    hash
}

#[test]
fn the_segment_length_is_the_vectors() {
    assert_eq!(vectors()["L"], L);
}

#[test]
fn texts_split_into_the_vectors_segments_and_chain_states() {
    for case in vectors()["text"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let text = case["text"].as_str().unwrap();
        let lengths: Vec<u64> = segments(text).map(|segment| segment.len() as u64).collect();
        let expected: Vec<u64> = case["segments"]
            .as_array()
            .unwrap()
            .iter()
            .map(|length| length.as_u64().unwrap())
            .collect();
        assert_eq!(lengths, expected, "{name}");
        let states: Vec<String> = chain_states(text).iter().map(|state| hex(state)).collect();
        let expected: Vec<&str> = case["chain"]
            .as_array()
            .unwrap()
            .iter()
            .map(|state| state.as_str().unwrap())
            .collect();
        assert_eq!(states, expected, "{name}");
        assert_eq!(hex(&text_hash(text)), case["hash"].as_str().unwrap(), "{name}");
        assert_eq!(text.len() as u64, case["length"].as_u64().unwrap(), "{name}");
    }
}

#[test]
fn a_window_from_a_chain_state_hashes_like_the_whole_text() {
    for case in vectors()["windows"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let before = unhex(case["before"].as_str().unwrap());
        let hash = window_hash(&before, case["length"].as_u64().unwrap(), case["text"].as_str().unwrap());
        assert_eq!(hex(&hash), case["hash"].as_str().unwrap(), "{name}");
    }
}

#[test]
fn segments_are_code_point_aligned_and_cover_the_text() {
    for text in ["", "a", &"日".repeat(5000), &"😀".repeat(3000), &format!("a{}", "é".repeat(4000))] {
        let parts: Vec<&str> = segments(text).collect();
        assert_eq!(parts.concat(), text);
        assert!(parts.iter().all(|part| !part.is_empty() && part.len() <= L));
        // Greedy: every segment but the last is as long as a code point boundary allows.
        for part in &parts[..parts.len().saturating_sub(1)] {
            assert!(part.len() > L - 4, "{}", part.len());
        }
    }
}

#[test]
fn appending_changes_only_the_last_segment_and_the_chain_after_it() {
    let before = "x".repeat(10_000);
    let after = format!("{before}{}", "y".repeat(500));
    let (a, b) = (chain_states(&before), chain_states(&after));
    assert_eq!(a[..a.len() - 1], b[..a.len() - 1]);
    assert_ne!(a[a.len() - 1], b[a.len() - 1]);
}

#[test]
fn the_length_is_part_of_the_hash() {
    // The same segment chain with another length is another text.
    let state = chain_states("abc");
    assert_ne!(finish(3, &state[0]), finish(4, &state[0]));
    assert_ne!(text_hash(""), finish(0, &[1; 32]));
}
