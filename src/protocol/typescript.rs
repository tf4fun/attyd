//! Generates `shared/protocol.ts` from the protocol DTOs and fails when the
//! checked-in file is stale. Regenerate with `npm run protocol:types`.
use std::path::Path;

use ts_rs::{Config, TS};

use super::canon::*;
use super::documents::*;
use super::*;

const OUTPUT: &str = "shared/protocol.ts";

/// ACP types embedded verbatim; they come from the SDK and are not regenerated.
const ACP_TYPES: &[&str] = &[
    "AvailableCommand",
    "ContentBlock",
    "Cost",
    "CreateElicitationRequest",
    "InitializeResponse",
    "RequestPermissionRequest",
    "SessionConfigOption",
    "SessionMode",
    "SessionUpdate",
];

/// Projections of ACP types that ts-rs cannot express. A process update is the
/// SDK's `SessionUpdate` with each content position (the fields
/// `attachments::update_blocks` visits) normalized to a `Block` (§4.4).
const PROJECTED_TYPES: &[&str] = &[
    "type ProcessUpdate = ProjectedFields<Extract<SessionUpdate, { sessionUpdate: \"user_message_chunk\" | \"agent_message_chunk\" | \"agent_thought_chunk\" | \"compaction_summary_chunk\" }>, \"content\"> | ProjectedFields<Extract<SessionUpdate, { sessionUpdate: \"compaction_update\" }>, \"summary\"> | ProjectedToolCall<Extract<SessionUpdate, { sessionUpdate: \"tool_call\" | \"tool_call_update\" }>> | Exclude<SessionUpdate, { sessionUpdate: \"user_message_chunk\" | \"agent_message_chunk\" | \"agent_thought_chunk\" | \"compaction_summary_chunk\" | \"compaction_update\" | \"tool_call\" | \"tool_call_update\" }>;",
    "type ProjectedFields<T, K extends PropertyKey> = T extends unknown ? { [P in keyof T]: P extends K ? ProjectedBlocks<T[P]> : T[P] } : never;",
    "type ProjectedBlocks<V> = V extends ContentBlock ? Block : V extends Array<infer B> ? Array<ProjectedBlocks<B>> : V;",
    "type ProjectedToolCall<T> = T extends unknown ? { [P in keyof T]: P extends \"content\" ? ProjectedToolCallContents<T[P]> : T[P] } : never;",
    "type ProjectedToolCallContents<V> = V extends Array<infer C> ? Array<ProjectedToolCallContent<C>> : V;",
    "type ProjectedToolCallContent<C> = C extends { type: \"content\" } ? ProjectedFields<C, \"content\"> : C;",
];

fn declarations(cfg: &Config) -> Vec<String> {
    macro_rules! decls {
        ($($ty:ty),* $(,)?) => { vec![$(<$ty as TS>::decl(cfg)),*] };
    }
    decls![
        SessionControls,
        SessionModes,
        SessionInfo,
        SessionUsage,
        Op,
        AppendTail,
        Turn,
        Block,
        PromptBlock,
        AttachmentRef,
        AttachmentKind,
        LogEntry,
        ProtocolError,
        ErrorCode,
        OperationKind,
        OperationStatus,
        Operation,
        Interaction,
        UrlFlow,
        Terminal,
        ExitStatus,
        LoginTerminal,
        SessionLive,
        GlobalLive,
        CatalogLive,
        AgentInfo,
        AgentPhase,
        AgentLaunch,
        AgentTransport,
        McpServerSummary,
        McpServerKind,
        SessionSummary,
        OperationSummary,
        PendingCounts,
        Event,
        LogChange,
        LiveChange,
        End,
        EndReason,
    ]
}

fn render() -> String {
    // Counts and sizes are JSON numbers below 2^53.
    let cfg = Config::new().with_large_int("number");
    let mut output = String::from(
        "// Generated from src/protocol by `npm run protocol:types`. Do not edit.\n\
         // Protocol v2: docs/protocol.md.\n\n",
    );
    output.push_str("import type {\n");
    for name in ACP_TYPES {
        output.push_str(&format!("  {name},\n"));
    }
    output.push_str("} from \"@agentclientprotocol/sdk\";\n");
    let projected = PROJECTED_TYPES
        .iter()
        .map(|declaration| declaration.to_string());
    for declaration in declarations(&cfg).into_iter().chain(projected) {
        output.push_str("\nexport ");
        output.push_str(&declaration);
        output.push('\n');
    }
    output
}

#[test]
fn generated_typescript_is_current() {
    let generated = render();
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(OUTPUT);
    if std::env::var_os("ATTYD_UPDATE_PROTOCOL_TS").is_some() {
        std::fs::write(&path, &generated).expect("write generated protocol types");
        return;
    }
    let current = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        current == generated,
        "{OUTPUT} is stale; run `npm run protocol:types`.\n\
         ----- BEGIN GENERATED {OUTPUT} -----\n{generated}----- END GENERATED {OUTPUT} -----"
    );
}

#[test]
fn every_embedded_acp_type_is_imported_and_used() {
    let generated = render();
    for name in ACP_TYPES {
        let word = regex::Regex::new(&format!(r"\b{name}\b")).unwrap();
        let uses = word.find_iter(&generated).count();
        assert!(uses >= 2, "{name} is imported but never referenced");
    }
}
