//! JSON Pointer ops (protocol §4): the generic diff the commit step uses for
//! small values, and the patcher every replica applies.
use serde_json::{Map, Value};

use super::{AppendTail, Op};

/// How the diff treats a changed string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Text {
    /// Growth appends; anything else is a `set`.
    Grow,
    /// A bounded tail (terminal output): front trimming plus growth.
    Tail,
}

/// Appends `token` to a JSON Pointer.
pub(crate) fn child(parent: &str, token: &str) -> String {
    let mut path = String::with_capacity(parent.len() + token.len() + 1);
    path.push_str(parent);
    path.push('/');
    for character in token.chars() {
        match character {
            '~' => path.push_str("~0"),
            '/' => path.push_str("~1"),
            character => path.push(character),
        }
    }
    path
}

/// Emits the ops that turn `old` into `new` at `path`. Objects diff per key,
/// arrays per index plus an appended tail, and strings by growth; everything
/// else is a `set` of the smallest enclosing value.
pub(crate) fn diff(
    path: &str,
    old: &Value,
    new: &Value,
    text: &impl Fn(&str) -> Text,
    ops: &mut Vec<Op>,
) {
    if old == new {
        return;
    }
    match (old, new) {
        (Value::Object(old), Value::Object(new)) => diff_object(path, old, new, text, ops),
        (Value::Array(old), Value::Array(new)) if new.len() >= old.len() && !old.is_empty() => {
            for (index, (old, new)) in old.iter().zip(new).enumerate() {
                diff(&child(path, &index.to_string()), old, new, text, ops);
            }
            if new.len() > old.len() {
                ops.push(Op::Append {
                    path: path.to_string(),
                    tail: AppendTail::Values {
                        values: new[old.len()..].to_vec(),
                    },
                });
            }
        }
        (Value::String(old), Value::String(new)) => diff_text(path, old, new, text(path), ops),
        _ => ops.push(Op::Set {
            path: path.to_string(),
            value: new.clone(),
        }),
    }
}

fn diff_object(
    path: &str,
    old: &Map<String, Value>,
    new: &Map<String, Value>,
    text: &impl Fn(&str) -> Text,
    ops: &mut Vec<Op>,
) {
    for key in old.keys().filter(|key| !new.contains_key(*key)) {
        ops.push(Op::Remove {
            path: child(path, key),
        });
    }
    for (key, value) in new {
        match old.get(key) {
            Some(previous) => diff(&child(path, key), previous, value, text, ops),
            None => ops.push(Op::Set {
                path: child(path, key),
                value: value.clone(),
            }),
        }
    }
}

fn diff_text(path: &str, old: &str, new: &str, mode: Text, ops: &mut Vec<Op>) {
    let kept = match mode {
        Text::Grow => (!old.is_empty() && new.starts_with(old)).then_some(0),
        Text::Tail => {
            let start = tail_overlap(old, new);
            (start < old.len()).then_some(start)
        }
    };
    let Some(start) = kept else {
        ops.push(Op::Set {
            path: path.to_string(),
            value: Value::String(new.to_string()),
        });
        return;
    };
    if start > 0 {
        ops.push(Op::Trim {
            path: path.to_string(),
            count: utf16_len(&old[..start]),
        });
    }
    let appended = &new[old.len() - start..];
    if !appended.is_empty() {
        ops.push(Op::Append {
            path: path.to_string(),
            tail: AppendTail::Text {
                text: appended.to_string(),
            },
        });
    }
}

/// The smallest byte offset `start` at a character boundary such that
/// `old[start..]` is a prefix of `new`, or `old.len()` when only the empty
/// suffix is. Linear: the prefix function of `new`, then a scan of `old`.
fn tail_overlap(old: &str, new: &str) -> usize {
    let pattern = new.as_bytes();
    let mut failure = vec![0; pattern.len()];
    let mut matched = 0;
    for index in 1..pattern.len() {
        while matched > 0 && pattern[index] != pattern[matched] {
            matched = failure[matched - 1];
        }
        if pattern[index] == pattern[matched] {
            matched += 1;
        }
        failure[index] = matched;
    }
    matched = 0;
    for &byte in old.as_bytes() {
        while matched > 0 && (matched == pattern.len() || byte != pattern[matched]) {
            matched = failure[matched - 1];
        }
        if matched < pattern.len() && byte == pattern[matched] {
            matched += 1;
        }
    }
    // The longest suffix of `old` that prefixes `new`. A match always starts at
    // a character boundary: both strings are UTF-8 and the match is a prefix.
    old.len() - matched
}

fn utf16_len(text: &str) -> usize {
    text.chars().map(char::len_utf16).sum()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ApplyError {
    pub path: String,
    pub reason: &'static str,
}

/// Applies one op. A violation is a protocol error; the replica resets the
/// scope (protocol §4).
pub(crate) fn apply(document: &mut Value, op: &Op) -> Result<(), ApplyError> {
    let path = match op {
        Op::Set { path, .. }
        | Op::Remove { path }
        | Op::Append { path, .. }
        | Op::Trim { path, .. } => path,
    };
    let error = |reason| ApplyError {
        path: path.clone(),
        reason,
    };
    let tokens = parse(path).ok_or_else(|| error("invalid pointer"))?;
    if let Op::Set { value, .. } = op {
        let Some((leaf, parent)) = tokens.split_last() else {
            *document = value.clone();
            return Ok(());
        };
        return match walk(document, parent).ok_or_else(|| error("missing parent"))? {
            Value::Object(fields) => {
                fields.insert(leaf.clone(), value.clone());
                Ok(())
            }
            Value::Array(values) => {
                let slot = index(leaf)
                    .and_then(|index| values.get_mut(index))
                    .ok_or_else(|| error("array index out of range"))?;
                *slot = value.clone();
                Ok(())
            }
            _ => Err(error("parent is not a container")),
        };
    }
    if let Op::Remove { .. } = op {
        let (leaf, parent) = tokens
            .split_last()
            .ok_or_else(|| error("cannot remove root"))?;
        return match walk(document, parent).ok_or_else(|| error("missing parent"))? {
            Value::Object(fields) => fields
                .remove(leaf)
                .map(drop)
                .ok_or_else(|| error("missing key")),
            Value::Array(values) => match index(leaf) {
                Some(index) if index < values.len() => {
                    values.remove(index);
                    Ok(())
                }
                _ => Err(error("array index out of range")),
            },
            _ => Err(error("parent is not a container")),
        };
    }
    let target = walk(document, &tokens).ok_or_else(|| error("missing target"))?;
    match (op, target) {
        (
            Op::Append {
                tail: AppendTail::Values { values },
                ..
            },
            Value::Array(target),
        ) => target.extend(values.iter().cloned()),
        (
            Op::Append {
                tail: AppendTail::Text { text },
                ..
            },
            Value::String(target),
        ) => target.push_str(text),
        (Op::Trim { count, .. }, Value::Array(target)) if *count <= target.len() => {
            target.drain(..count);
        }
        (Op::Trim { count, .. }, Value::String(target)) => {
            let mut units = 0;
            let start = target
                .char_indices()
                .map(|(offset, character)| {
                    let start = (units, offset);
                    units += character.len_utf16();
                    start
                })
                .chain(std::iter::once((utf16_len(target), target.len())))
                .find(|(units, _)| units >= count)
                .filter(|(units, _)| units == count)
                .map(|(_, offset)| offset)
                .ok_or_else(|| error("trim splits a character or exceeds the text"))?;
            target.drain(..start);
        }
        _ => return Err(error("target type does not match the op")),
    }
    Ok(())
}

fn parse(path: &str) -> Option<Vec<String>> {
    if path.is_empty() {
        return Some(Vec::new());
    }
    path.strip_prefix('/')?
        .split('/')
        .map(|token| {
            let mut decoded = String::with_capacity(token.len());
            let mut characters = token.chars();
            while let Some(character) = characters.next() {
                match character {
                    '~' => match characters.next()? {
                        '0' => decoded.push('~'),
                        '1' => decoded.push('/'),
                        _ => return None,
                    },
                    character => decoded.push(character),
                }
            }
            Some(decoded)
        })
        .collect()
}

fn index(token: &str) -> Option<usize> {
    (token == "0" || !token.starts_with('0'))
        .then(|| token.parse().ok())
        .flatten()
}

fn walk<'a>(mut value: &'a mut Value, tokens: &[String]) -> Option<&'a mut Value> {
    for token in tokens {
        value = match value {
            Value::Object(fields) => fields.get_mut(token)?,
            Value::Array(values) => values.get_mut(index(token)?)?,
            _ => return None,
        };
    }
    Some(value)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn grow(_: &str) -> Text {
        Text::Grow
    }

    fn ops(old: &Value, new: &Value, text: impl Fn(&str) -> Text) -> Vec<Op> {
        let mut ops = Vec::new();
        diff("", old, new, &text, &mut ops);
        let mut replica = old.clone();
        for op in &ops {
            apply(&mut replica, op).unwrap();
        }
        assert_eq!(&replica, new, "{ops:?}");
        ops
    }

    #[test]
    fn objects_diff_per_key_and_escape_pointer_tokens() {
        let old = json!({ "keep": 1, "drop": 2, "a/b": { "x": 1 }, "t~": "v" });
        let new = json!({ "keep": 1, "a/b": { "x": 2 }, "t~": "v", "add": [1] });
        assert_eq!(
            serde_json::to_value(ops(&old, &new, grow)).unwrap(),
            json!([
                { "op": "remove", "path": "/drop" },
                { "op": "set", "path": "/a~1b/x", "value": 2 },
                { "op": "set", "path": "/add", "value": [1] },
            ])
        );
    }

    #[test]
    fn arrays_fold_in_place_append_tails_and_replace_when_shrinking() {
        let old = json!({ "updates": [{ "id": 1, "status": "running" }, { "id": 2 }] });
        let new = json!({ "updates": [{ "id": 1, "status": "done" }, { "id": 2 }, { "id": 3 }] });
        assert_eq!(
            serde_json::to_value(ops(&old, &new, grow)).unwrap(),
            json!([
                { "op": "set", "path": "/updates/0/status", "value": "done" },
                { "op": "append", "path": "/updates", "values": [{ "id": 3 }] },
            ])
        );
        let shrunk = json!({ "updates": [{ "id": 9 }] });
        assert_eq!(
            serde_json::to_value(ops(&old, &shrunk, grow)).unwrap(),
            json!([{ "op": "set", "path": "/updates", "value": [{ "id": 9 }] }])
        );
        let grown = json!({ "updates": [1] });
        assert_eq!(
            serde_json::to_value(ops(&json!({ "updates": [] }), &grown, grow)).unwrap(),
            json!([{ "op": "set", "path": "/updates", "value": [1] }])
        );
    }

    #[test]
    fn text_growth_appends_and_other_changes_set() {
        let old = json!({ "text": "Hello", "title": "a" });
        let new = json!({ "text": "Hello, world", "title": "b" });
        assert_eq!(
            serde_json::to_value(ops(&old, &new, grow)).unwrap(),
            json!([
                { "op": "append", "path": "/text", "text": ", world" },
                { "op": "set", "path": "/title", "value": "b" },
            ])
        );
    }

    #[test]
    fn bounded_tails_trim_utf16_units_and_append() {
        let tail = |path: &str| {
            if path.ends_with("/output") {
                Text::Tail
            } else {
                Text::Grow
            }
        };
        let old = json!({ "t": { "output": "😀ab\ncd" } });
        let new = json!({ "t": { "output": "ab\ncdef" } });
        assert_eq!(
            serde_json::to_value(ops(&old, &new, tail)).unwrap(),
            json!([
                { "op": "trim", "path": "/t/output", "count": 2 },
                { "op": "append", "path": "/t/output", "text": "ef" },
            ])
        );
        // Repetitive output: any overlap gives the same document.
        ops(
            &json!({ "t": { "output": "aaaa" } }),
            &json!({ "t": { "output": "aaab" } }),
            tail,
        );
        // Only trimming.
        assert_eq!(
            serde_json::to_value(ops(
                &json!({ "t": { "output": "abcd" } }),
                &json!({ "t": { "output": "cd" } }),
                tail,
            ))
            .unwrap(),
            json!([{ "op": "trim", "path": "/t/output", "count": 2 }])
        );
        // No overlap at all.
        assert_eq!(
            serde_json::to_value(ops(
                &json!({ "t": { "output": "abc" } }),
                &json!({ "t": { "output": "xyz" } }),
                tail,
            ))
            .unwrap(),
            json!([{ "op": "set", "path": "/t/output", "value": "xyz" }])
        );
    }

    #[test]
    fn violations_are_errors_not_silent_repairs() {
        let mut document = json!({ "a": [1, 2], "s": "😀x", "n": 1 });
        for op in [
            json!({ "op": "set", "path": "/missing/leaf", "value": 1 }),
            json!({ "op": "set", "path": "/a/2", "value": 1 }),
            json!({ "op": "set", "path": "/a/01", "value": 1 }),
            json!({ "op": "remove", "path": "/missing" }),
            json!({ "op": "remove", "path": "" }),
            json!({ "op": "append", "path": "/n", "values": [1] }),
            json!({ "op": "append", "path": "/a", "text": "x" }),
            json!({ "op": "trim", "path": "/a", "count": 3 }),
            json!({ "op": "trim", "path": "/s", "count": 1 }),
            json!({ "op": "trim", "path": "/s", "count": 4 }),
            json!({ "op": "set", "path": "a", "value": 1 }),
            json!({ "op": "set", "path": "/~2", "value": 1 }),
        ] {
            let op: Op = serde_json::from_value(op).unwrap();
            assert!(apply(&mut document, &op).is_err(), "{op:?}");
        }
        assert_eq!(document, json!({ "a": [1, 2], "s": "😀x", "n": 1 }));

        let op: Op =
            serde_json::from_value(json!({ "op": "trim", "path": "/s", "count": 2 })).unwrap();
        apply(&mut document, &op).unwrap();
        let op: Op = serde_json::from_value(json!({ "op": "remove", "path": "/a/0" })).unwrap();
        apply(&mut document, &op).unwrap();
        let op: Op =
            serde_json::from_value(json!({ "op": "set", "path": "", "value": [] })).unwrap();
        assert_eq!(document, json!({ "a": [2], "s": "x", "n": 1 }));
        apply(&mut document, &op).unwrap();
        assert_eq!(document, json!([]));
    }
}
