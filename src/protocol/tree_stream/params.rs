//! The query of `GET /api/v2/stream` (design §6.1).
use std::collections::BTreeMap;

use super::super::documents::{ErrorCode, ProtocolError};
use super::super::hash::L;
use super::super::tree::unescape;

/// One path a stream follows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Follow {
    pub path: String,
    /// The path version the client holds and has applied without error, or verified.
    pub have: Option<u64>,
    /// Hold the text positions under the path as windows of this many bytes (§6.4).
    pub window: Option<usize>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct StreamQuery {
    /// The instance the client's holdings belong to; none when it holds nothing.
    pub instance: Option<String>,
    pub follow: Vec<Follow>,
    /// The working directory to load an unheld session with, by session ID (§7.2).
    pub cwd: BTreeMap<String, String>,
}

fn invalid(message: String) -> ProtocolError {
    ProtocolError::new(ErrorCode::InvalidRequest, message)
}

/// Whether `path` is a JSON Pointer naming a node below the root: it starts with `/` and
/// every `~` is `~0` or `~1`.
pub(crate) fn is_path(path: &str) -> bool {
    path.starts_with('/')
        && path.len() > 1
        && path.match_indices('~').all(|(at, _)| matches!(path.as_bytes().get(at + 1), Some(b'0' | b'1')))
}

/// Whether the tree has a node at `path` (design §4.2): the shape `shared/tree-shape.ts` writes
/// down. A position inside a value, such as `/sessions/s/controls/modes`, is no node and cannot be
/// followed: it could never exist, and following it would hold and load its session for nothing.
pub(crate) fn is_node(path: &str) -> bool {
    let tokens: Vec<String> = path.strip_prefix('/').map(|rest| rest.split('/').map(unescape).collect()).unwrap_or_default();
    let tokens: Vec<&str> = tokens.iter().map(String::as_str).collect();
    // A collection is itself, or one item of it.
    let collection = |rest: &[&str]| rest.len() <= 1;
    let session_child = |rest: &[&str]| match rest {
        ["controls" | "op" | "opDetail", tail @ ..] => tail.is_empty(),
        ["ops" | "prompts" | "turns" | "entries" | "interactions" | "urlFlows" | "terminals", tail @ ..] => collection(tail),
        _ => false,
    };
    match tokens.as_slice() {
        ["global"] | ["catalog"] | ["sessions"] | ["views"] => true,
        ["global", rest @ ..] => match rest {
            ["op" | "opDetail", tail @ ..] => tail.is_empty(),
            ["ops" | "sessions" | "loadFailures" | "interactions" | "urlFlows" | "terminals", tail @ ..] => collection(tail),
            _ => false,
        },
        ["catalog", rest @ ..] => match rest {
            ["op" | "opDetail", tail @ ..] => tail.is_empty(),
            ["ops" | "pages" | "interactions" | "urlFlows", tail @ ..] => collection(tail),
            _ => false,
        },
        ["sessions", _id] => true,
        ["sessions", _id, rest @ ..] => session_child(rest),
        ["views", _id, rest @ ..] => match rest {
            [] | ["output"] | ["output", "latest"] | ["history"] => true,
            ["history", _] | ["history", _, _] => true,
            ["history", _instance, _rev, "output" | "turn"] => true,
            ["history", _instance, _rev, "output", _start] => true,
            ["history", _instance, _rev, "turn", _start] => true,
            ["history", _instance, _rev, "turn", _start, "entries", tail @ ..] => collection(tail),
            _ => false,
        },
        _ => false,
    }
}

/// Whether one of the paths contains the other (or they are the same).
fn overlap(a: &str, b: &str) -> bool {
    let contains = |outer: &str, inner: &str| {
        inner == outer || inner.strip_prefix(outer).is_some_and(|rest| rest.starts_with('/'))
    };
    contains(a, b) || contains(b, a)
}

/// The session a path belongs to: `/sessions/<id>/…` and `/views/<id>/…` hold session `<id>`
/// (§7.2).
pub(crate) fn session_of(path: &str) -> Option<String> {
    let mut tokens = path.strip_prefix('/')?.split('/');
    matches!(tokens.next()?, "sessions" | "views").then_some(())?;
    tokens.next().map(unescape)
}

/// `<path>:<value>`, the value following the last `:`.
fn split_last(parameter: &str, name: &str) -> Result<(String, String), ProtocolError> {
    let (path, value) = parameter
        .rsplit_once(':')
        .ok_or_else(|| invalid(format!("{name} is not <path>:<value>: {parameter:?}")))?;
    Ok((path.to_string(), value.to_string()))
}

/// The query of `GET /api/v2/stream?instance=…&follow=…&have=…&window=…&cwd=…`.
pub(crate) fn parse_stream_query(query: &str) -> Result<StreamQuery, ProtocolError> {
    let mut parsed = StreamQuery::default();
    let mut haves = Vec::new();
    let mut windows = Vec::new();
    let mut cwds = Vec::new();
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "instance" => {
                if value.is_empty() || parsed.instance.replace(value.into_owned()).is_some() {
                    return Err(invalid("instance is named once and not empty".into()));
                }
            }
            "follow" => {
                if !is_path(&value) {
                    return Err(invalid(format!("not a path: {value:?}")));
                }
                if !is_node(&value) {
                    return Err(invalid(format!("not a node of the tree: {value:?}")));
                }
                if let Some(other) = parsed.follow.iter().find(|follow| overlap(&follow.path, &value)) {
                    return Err(invalid(format!("paths overlap: {:?} and {value:?}", other.path)));
                }
                parsed.follow.push(Follow { path: value.into_owned(), have: None, window: None });
            }
            "have" => haves.push(value.into_owned()),
            "window" => windows.push(value.into_owned()),
            "cwd" => cwds.push(value.into_owned()),
            _ => return Err(invalid(format!("unknown stream parameter: {key}"))),
        }
    }
    if parsed.follow.is_empty() {
        return Err(invalid("a stream follows at least one path".into()));
    }
    for have in haves {
        let (path, version) = split_last(&have, "have")?;
        let version: u64 = version
            .parse()
            .map_err(|_| invalid(format!("have names no version: {have:?}")))?;
        if parsed.instance.is_none() {
            return Err(invalid("have needs the instance it belongs to".into()));
        }
        let follow = followed(&mut parsed, &path, "have")?;
        if follow.have.replace(version).is_some() {
            return Err(invalid(format!("have named twice for {path}")));
        }
    }
    for window in windows {
        let (path, bytes) = split_last(&window, "window")?;
        let bytes: usize = bytes
            .parse()
            .map_err(|_| invalid(format!("window names no size: {window:?}")))?;
        if bytes < L {
            return Err(invalid(format!("a window holds at least {L} bytes: {window:?}")));
        }
        let follow = followed(&mut parsed, &path, "window")?;
        if follow.window.replace(bytes).is_some() {
            return Err(invalid(format!("window named twice for {path}")));
        }
    }
    for cwd in cwds {
        // The ID is percent-encoded and ends at the first `:`; the directory is the rest.
        let (id, dir) = cwd
            .split_once(':')
            .ok_or_else(|| invalid(format!("cwd is not <id>:<dir>: {cwd:?}")))?;
        let id = percent_decode(id)
            .ok_or_else(|| invalid(format!("cwd names an ID that is not percent-encoded UTF-8: {cwd:?}")))?;
        if dir.is_empty() {
            return Err(invalid(format!("cwd names no directory: {cwd:?}")));
        }
        if !parsed.follow.iter().any(|follow| session_of(&follow.path).as_deref() == Some(id.as_str())) {
            return Err(invalid(format!("cwd of a session not followed: {cwd:?}")));
        }
        if parsed.cwd.insert(id, dir.to_string()).is_some() {
            return Err(invalid(format!("cwd named twice: {cwd:?}")));
        }
    }
    Ok(parsed)
}

/// `text` with each `%XX` replaced by its byte; none when that is not UTF-8 or a `%` is not
/// followed by two hexadecimal digits.
fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let digits = std::str::from_utf8(bytes.get(index + 1..index + 3)?).ok()?;
            out.push(u8::from_str_radix(digits, 16).ok()?);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn followed<'a>(
    query: &'a mut StreamQuery,
    path: &str,
    name: &str,
) -> Result<&'a mut Follow, ProtocolError> {
    query
        .follow
        .iter_mut()
        .find(|follow| follow.path == path)
        .ok_or_else(|| invalid(format!("{name} of a path not followed: {path:?}")))
}

#[cfg(test)]
mod tests;
