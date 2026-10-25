//! Document digests (docs/protocol.md §5.5).
//!
//! A digest states that a document, at its `epoch` and `rev`, has exactly this
//! content: the bridge sends it in snapshots and checkpoints and the client
//! recomputes it over the replica it holds. Entries are hashed one by one, so a
//! change to one entry costs one hash; the document digest then combines the
//! document's head (everything but the entries) with the entries' digest.
//!
//! ```text
//! e_i    = SHA-256(canon(entries[i]))
//! E      = SHA-256(e_0 ‖ e_1 ‖ …)              (no entries known: SHA-256("unknown"))
//! digest = SHA-256(canon(head) ‖ E)            (global, catalog: SHA-256(canon(doc)))
//! ```
//!
//! `canon` is JCS (RFC 8785) as `canon::canonical_json` writes it. The wire form
//! is the unpadded base64url encoding of the first 16 bytes.
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
#[cfg(test)]
use serde_json::Value;
#[cfg(test)]
use sha2::{Digest, Sha256};

#[cfg(test)]
use super::canon::canonical_json;

/// A SHA-256 hash.
pub(crate) type Hash = [u8; 32];

/// `e_i`: the hash of one entry as it is published.
#[cfg(test)]
pub(crate) fn entry_hash(entry: &Value) -> Hash {
    Sha256::digest(canonical_json(entry)).into()
}

/// `E`: the digest of the entry hashes in order. A session whose history is
/// unknown has no entries, which is not the same as an empty list.
#[cfg(test)]
pub(crate) fn entries_digest(hashes: Option<&[Hash]>) -> Hash {
    match hashes {
        None => Sha256::digest(b"unknown").into(),
        Some(hashes) => {
            let mut hasher = Sha256::new();
            for hash in hashes {
                hasher.update(hash);
            }
            hasher.finalize().into()
        }
    }
}

/// The digest of a session document: `head` is the document without its
/// entries (`epoch`, `rev`, `history`, `prompts`, `live`; `history` is absent
/// while the history is unknown), `entries` their hashes, or `None` while unknown.
#[cfg(test)]
pub(crate) fn session_digest(head: &Value, entries: Option<&[Hash]>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(canonical_json(head));
    hasher.update(entries_digest(entries));
    let digest: Hash = hasher.finalize().into();
    wire(&digest)
}

/// The digest of a `global` or `catalog` document (`epoch`, `rev`, `live`).
#[cfg(test)]
pub(crate) fn scope_digest(document: &Value) -> String {
    let digest: Hash = Sha256::digest(canonical_json(document)).into();
    wire(&digest)
}

/// A hash on the wire: the unpadded base64url of its first 16 bytes.
pub(crate) fn wire(hash: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(&hash[..16])
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// Computed by an independent implementation; the TypeScript digest
    /// reproduces the same file.
    fn vectors() -> Vec<Value> {
        let file: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/protocol-digest-vectors.json"
        ))
        .expect("the vectors are JSON");
        file["cases"].as_array().expect("cases").clone()
    }

    fn hex(hash: &[u8]) -> String {
        hash.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    /// A session document's head and the hashes of its entries.
    fn split(document: &Value) -> (Value, Option<Vec<Hash>>) {
        let mut head = document.clone();
        let entries = head
            .as_object_mut()
            .expect("a document is an object")
            .remove("entries");
        let hashes = entries.map(|entries| {
            entries
                .as_array()
                .expect("entries are an array")
                .iter()
                .map(entry_hash)
                .collect()
        });
        (head, hashes)
    }

    #[test]
    fn digests_match_the_independent_vectors() {
        for case in vectors() {
            let name = case["name"].as_str().unwrap();
            let document = &case["doc"];
            let digest = match case["kind"].as_str().unwrap() {
                "session" => {
                    let (head, hashes) = split(document);
                    session_digest(&head, hashes.as_deref())
                }
                "scope" => scope_digest(document),
                other => panic!("unknown vector kind {other}"),
            };
            assert_eq!(digest, case["digest"].as_str().unwrap(), "{name}");
        }
    }

    #[test]
    fn entry_hashes_and_their_digest_match_the_vectors() {
        for case in vectors().iter().filter(|case| case["kind"] == "session") {
            let name = case["name"].as_str().unwrap();
            let (_, hashes) = split(&case["doc"]);
            let expected: Option<Vec<&str>> = case["entryHashes"]
                .as_array()
                .map(|hashes| hashes.iter().map(|hash| hash.as_str().unwrap()).collect());
            assert_eq!(
                hashes
                    .as_ref()
                    .map(|hashes| hashes.iter().map(|hash| hex(hash)).collect::<Vec<_>>()),
                expected.map(|hashes| hashes.into_iter().map(String::from).collect::<Vec<String>>()),
                "{name}"
            );
            assert_eq!(
                hex(&entries_digest(hashes.as_deref())),
                case["entriesDigest"].as_str().unwrap(),
                "{name}"
            );
        }
    }

    #[test]
    fn unknown_entries_are_not_an_empty_list() {
        assert_ne!(entries_digest(None), entries_digest(Some(&[])));
    }

    #[test]
    fn a_wire_digest_is_twenty_two_url_safe_characters() {
        let digest = scope_digest(&json!({ "epoch": "e", "rev": 0, "live": {} }));
        assert_eq!(digest.len(), 22);
        assert!(
            digest
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "{digest}"
        );
    }

    #[test]
    fn any_part_of_a_session_document_changes_its_digest() {
        let document = json!({
            "epoch": "e", "rev": 1, "history": "complete",
            "entries": [{ "kind": "message", "role": "user", "chunks": [] }],
            "prompts": [], "live": { "closing": false },
        });
        let digest = |document: &Value| {
            let (head, hashes) = split(document);
            session_digest(&head, hashes.as_deref())
        };
        let original = digest(&document);
        for (path, value) in [
            ("/epoch", json!("other")),
            ("/rev", json!(2)),
            ("/history", json!("partial")),
            ("/prompts", json!([{ "entry": 0 }])),
            ("/live/closing", json!(true)),
            ("/entries/0/role", json!("agent")),
        ] {
            let mut changed = document.clone();
            *changed.pointer_mut(path).unwrap() = value;
            assert_ne!(digest(&changed), original, "{path}");
        }
        // The key order of an object is not part of its content.
        let reordered: Value =
            serde_json::from_str(r#"{"live":{"closing":false},"prompts":[],"history":"complete","entries":[{"chunks":[],"role":"user","kind":"message"}],"rev":1,"epoch":"e"}"#)
                .unwrap();
        assert_eq!(digest(&reordered), original);
    }
}
