//! Protocol v2 wire types ([protocol](../../docs/protocol.md)).
//!
//! These DTOs are the only definition of the v2 wire shapes. `shared/protocol.ts`
//! is generated from them by `typescript::generated_typescript_is_current`;
//! embedded ACP values stay `serde_json::Value` in Rust and name their
//! `@agentclientprotocol/sdk` type in TypeScript.
//!
//! So far: the control reduction (`controls`), the JSON Pointer ops (`diff`),
//! `canon` with the hash chain and the turn log (`canon`), the documents and
//! events (`documents`), operation slots (`op`, `registry`), the projections
//! (`project`), the commit step (`publish`) and the Hub (`hub`). The bridge actor
//! runs the commit step from migration step 4b on (docs/state-sync.md §5).
#![allow(dead_code)]

pub(crate) mod admission;
pub(crate) mod canon;
mod controls;
#[cfg(test)]
mod convergence_tests;
pub(crate) mod diff;
pub(crate) mod digest;
pub(crate) mod documents;
pub(crate) mod engine;
pub(crate) mod feed;
pub(crate) mod hub;
pub(crate) mod live_doc;
#[cfg(test)]
mod hub_tests;
pub(crate) mod op;
pub(crate) mod pacing;
pub(crate) mod project;
pub(crate) mod publish;
pub(crate) mod registry;
#[cfg(test)]
pub(crate) mod stream_check;
pub(crate) mod subscription;
pub(crate) mod timeline;
pub(crate) mod timeline_doc;
#[cfg(test)]
mod typescript;
pub(crate) mod wire;

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Controls (protocol §4.5); the reduction lives in `controls`.

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionControls {
    pub modes: Option<SessionModes>,
    /// `null`: never provided; `[]`: provided empty. Preferred over `modes`
    /// whenever non-null.
    #[cfg_attr(test, ts(type = "SessionConfigOption[] | null"))]
    pub config_options: Option<Vec<Value>>,
    #[cfg_attr(test, ts(type = "AvailableCommand[]"))]
    pub commands: Vec<Value>,
    pub info: SessionInfo,
    pub usage: Option<SessionUsage>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionModes {
    pub current_mode_id: String,
    #[cfg_attr(test, ts(type = "SessionMode[]"))]
    pub available_modes: Vec<Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionInfo {
    pub title: Option<String>,
    pub updated_at: Option<String>,
    #[cfg_attr(test, ts(type = "unknown | null"))]
    pub meta: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionUsage {
    pub used: u64,
    pub size: u64,
    #[cfg_attr(test, ts(type = "Cost | null"))]
    pub cost: Option<Value>,
}

// ---------------------------------------------------------------------------
// Ops (protocol §5.3); the diff and the patcher live in `diff`.

/// A JSON Pointer operation on a `live` document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(tag = "op", rename_all = "snake_case")]
pub(crate) enum Op {
    /// Create or replace; an array index modifies in place.
    Set {
        path: String,
        #[cfg_attr(test, ts(type = "unknown"))]
        value: Value,
    },
    /// A map key or array index.
    Remove { path: String },
    /// Array or string tail.
    Append {
        path: String,
        #[serde(flatten)]
        tail: AppendTail,
    },
    /// Drops the first `count` array items or UTF-16 units.
    Trim { path: String, count: usize },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(untagged)]
pub(crate) enum AppendTail {
    Values {
        #[cfg_attr(test, ts(type = "unknown[]"))]
        values: Vec<Value>,
    },
    Text {
        text: String,
    },
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn ops_use_the_documented_wire_shape() {
        let ops = vec![
            Op::Set {
                path: "/op".into(),
                value: Value::Null,
            },
            Op::Remove {
                path: "/interactions/i".into(),
            },
            Op::Append {
                path: "/op/process".into(),
                tail: AppendTail::Values {
                    values: vec![json!({ "sessionUpdate": "plan" })],
                },
            },
            Op::Append {
                path: "/terminals/t/output".into(),
                tail: AppendTail::Text { text: "ok".into() },
            },
            Op::Trim {
                path: "/terminals/t/output".into(),
                count: 2,
            },
        ];
        let wire = json!([
            { "op": "set", "path": "/op", "value": null },
            { "op": "remove", "path": "/interactions/i" },
            { "op": "append", "path": "/op/process", "values": [{ "sessionUpdate": "plan" }] },
            { "op": "append", "path": "/terminals/t/output", "text": "ok" },
            { "op": "trim", "path": "/terminals/t/output", "count": 2 },
        ]);
        assert_eq!(serde_json::to_value(&ops).unwrap(), wire);
        assert_eq!(serde_json::from_value::<Vec<Op>>(wire).unwrap(), ops);
    }
}
