//! The revision 4 hashes against the vectors an independent implementation computed
//! (tests/fixtures/protocol_r4_vectors.py); the TypeScript hashes reproduce the same file.
use serde_json::{Map, Value};

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

// --- values and nodes ----------------------------------------------------------------

fn rule(name: &str) -> TextRule {
    match name {
        "none" => TextRule::None,
        "entry" => TextRule::Entry,
        "terminal" => TextRule::Terminal,
        "outputView" => TextRule::OutputView,
        other => panic!("unknown rule {other}"),
    }
}

#[test]
fn values_hash_like_the_vectors_with_their_text_positions_through_the_chain() {
    for case in vectors()["values"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let hash = value_hash(rule(case["rule"].as_str().unwrap()), &case["value"]);
        assert_eq!(hex(&hash), case["hash"].as_str().unwrap(), "{name}");
    }
}

/// The hash of a node tree as the vectors write it.
fn node(tree: &Value) -> Hash {
    let children = |tree: &Value| -> Vec<(String, Hash)> {
        tree["children"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(name, child)| (name.clone(), node(child)))
            .collect()
    };
    match tree["kind"].as_str().unwrap() {
        "value" => value_hash(rule(tree["rule"].as_str().unwrap()), &tree["value"]),
        "object" => {
            let children = children(tree);
            object_hash(
                tree["fields"].as_object().unwrap(),
                children.iter().map(|(name, hash)| (name.as_str(), *hash)),
            )
        }
        "map" => {
            let children = children(tree);
            map_hash(children.iter().map(|(name, hash)| (name.as_str(), *hash)))
        }
        "list" => {
            let items: Vec<Hash> = tree["items"].as_array().unwrap().iter().map(node).collect();
            list_hash(&items)
        }
        other => panic!("unknown kind {other}"),
    }
}

#[test]
fn nodes_hash_like_the_vectors() {
    for case in vectors()["nodes"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        assert_eq!(hex(&node(&case["node"])), case["hash"].as_str().unwrap(), "{name}");
    }
}

#[test]
fn text_outside_a_text_position_is_hashed_as_plain_json() {
    let plan = serde_json::json!({ "kind": "plan", "planId": "p", "text": "x" });
    let other = serde_json::json!({ "kind": "plan", "planId": "p", "text": "y" });
    assert_ne!(value_hash(TextRule::Entry, &plan), value_hash(TextRule::Entry, &other));
}

#[test]
fn a_text_position_is_hashed_by_its_chain_and_not_by_its_characters() {
    // The same message hashed as a plain value differs: the text became its chain hash.
    let message = serde_json::json!({ "kind": "message", "role": "agent",
        "chunks": [{ "content": { "type": "text", "text": "hi" } }] });
    assert_ne!(value_hash(TextRule::Entry, &message), value_hash(TextRule::None, &message));
}

#[test]
fn the_kinds_have_their_own_domain() {
    let empty_object = object_hash(&Map::new(), []);
    assert_ne!(empty_object, map_hash([]));
    assert_ne!(map_hash([]), list_hash([]));
    assert_ne!(empty_object, list_hash([]));
}

#[test]
fn locating_text_positions_does_not_change_what_is_hashed() {
    // Entries without the members a rule looks at hash as plain values.
    for entry in [
        serde_json::json!({ "kind": "message", "role": "user" }),
        serde_json::json!({ "kind": "tool" }),
        serde_json::json!({ "kind": "tool", "call": {} }),
        serde_json::json!({ "kind": "compaction" }),
        serde_json::json!({ "kind": "plan" }),
    ] {
        assert_eq!(value_hash(TextRule::Entry, &entry), value_hash(TextRule::None, &entry), "{entry}");
    }
    let terminal = serde_json::json!({ "terminalId": "t" });
    assert_eq!(value_hash(TextRule::Terminal, &terminal), value_hash(TextRule::None, &terminal));
    assert_eq!(value_hash(TextRule::OutputView, &terminal), value_hash(TextRule::None, &terminal));
}
