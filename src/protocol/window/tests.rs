use super::*;
use serde_json::Value;

fn texts() -> Vec<String> {
    vec![
        String::new(),
        "a".into(),
        "x".repeat(4096),
        "x".repeat(4097),
        "y".repeat(9000),
        "日".repeat(5000),
        "😀".repeat(7000),
        format!("a{}", "é".repeat(9000)),
        "héllo, 世界 😀 ".repeat(3000),
    ]
}

#[test]
fn a_window_is_the_shortest_run_of_whole_trailing_segments_of_at_least_n_bytes() {
    for text in texts() {
        for bytes in [4096, 5000, 8192, 20_000, 1 << 20] {
            let window = window(&text, bytes);
            let held = window.text.len();
            assert_eq!(window.length, text.len() as u64);
            assert_eq!(window.offset as usize + held, text.len());
            assert!(window.offset == 0 || held >= bytes, "{} {bytes}", text.len());
            assert!(held < bytes + L, "|text| < N + L: {held} {bytes}");
            // Shortest: without its first segment the run would hold less than N bytes.
            if let Some(first) = hash::segments(&window.text).next() {
                assert!(held - first.len() < bytes);
            }
            // The offset is a segment boundary of the whole text.
            let mut boundary = 0;
            let on_boundary = hash::segments(&text).any(|segment| {
                let hit = boundary == window.offset as usize;
                boundary += segment.len();
                hit
            });
            assert!(on_boundary || window.offset as usize == text.len(), "{} {bytes}", text.len());
        }
    }
}

#[test]
fn a_text_shorter_than_n_is_held_whole_from_the_zero_state() {
    let window = window("hello", 4096);
    assert_eq!(window, TextWindow { length: 5, offset: 0, before: state_wire(&ZERO), text: "hello".into() });
    assert_eq!(window.before.len(), 43, "32 bytes in unpadded base64url");
}

#[test]
fn a_window_hashes_to_the_hash_of_the_whole_text() {
    for text in texts() {
        for bytes in [4096, 8192, 20_000] {
            let window = window(&text, bytes);
            let before = URL_SAFE_NO_PAD.decode(&window.before).unwrap();
            let before: Hash = before.try_into().unwrap();
            assert_eq!(
                hash::window_hash(&before, window.length, &window.text),
                hash::text_hash(&text),
                "{} {bytes}",
                text.len()
            );
        }
    }
}

#[test]
fn an_append_is_sent_as_an_append_below_n_plus_l_bytes_whatever_the_window_held() {
    assert!(appends(1, 4096));
    assert!(appends(4096 + L - 1, 4096));
    assert!(!appends(4096 + L, 4096));
    assert!(!appends(1 << 20, 4096));
}

#[test]
fn the_window_moves_forward_only_by_whole_segments_as_text_grows() {
    let mut text = String::new();
    let mut last = 0;
    for _ in 0..40 {
        text.push_str(&"z".repeat(1500));
        let offset = window(&text, 8192).offset;
        assert!(offset >= last && offset % L as u64 == 0, "{offset}");
        last = offset;
    }
    assert!(last > 0);
}

use serde_json::json;

use crate::protocol::hash::TextRule;

fn entry(text: &str) -> Value {
    json!({ "kind": "message", "role": "agent", "messageId": "m", "chunks": [
        { "content": { "type": "text", "text": text } },
        { "content": { "type": "image", "data": "AAAA" } },
        { "content": { "type": "text", "text": "short" } },
    ]})
}

#[test]
fn a_value_holds_its_text_positions_as_windows_and_nothing_else_changes() {
    let long = "x".repeat(30_000);
    let held = window_value(TextRule::Entry, &entry(&long), 8192);
    assert_eq!(held["role"], "agent");
    assert_eq!(held["chunks"][1], json!({ "content": { "type": "image", "data": "AAAA" } }));
    let first = &held["chunks"][0]["content"]["text"];
    assert_eq!(first["length"], 30_000);
    assert_eq!(first["offset"], 20_480);
    assert_eq!(first["text"].as_str().unwrap().len(), 30_000 - 20_480);
    let second = &held["chunks"][2]["content"]["text"];
    assert_eq!(second, &json!({ "length": 5, "offset": 0, "before": state_wire(&ZERO), "text": "short" }));
}

#[test]
fn a_value_with_windows_hashes_like_the_whole_value() {
    for long in ["".to_string(), "x".repeat(100), "日".repeat(9000), "y".repeat(100_000)] {
        let whole = entry(&long);
        for bytes in [4096, 8192] {
            let held = window_value(TextRule::Entry, &whole, bytes);
            assert_eq!(
                windowed_value_hash(TextRule::Entry, &held),
                Some(hash::value_hash(TextRule::Entry, &whole)),
                "{} {bytes}",
                long.len()
            );
        }
    }
}

#[test]
fn a_text_position_that_is_not_a_window_does_not_verify() {
    assert_eq!(windowed_value_hash(TextRule::Terminal, &json!({ "output": "plain" })), None);
}
