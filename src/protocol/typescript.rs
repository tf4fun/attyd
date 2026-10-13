//! Generates `shared/protocol.ts` from the protocol DTOs and fails when the
//! checked-in file is stale. Regenerate with `npm run protocol:types`.
use std::path::Path;

use ts_rs::{Config, TS};

use super::*;

const OUTPUT: &str = "shared/protocol.ts";

/// ACP types embedded verbatim; they come from the SDK and are not regenerated.
const ACP_TYPES: &[&str] = &[
    "AvailableCommand",
    "ContentBlock",
    "Cost",
    "CreateElicitationRequest",
    "CreateElicitationResponse",
    "InitializeResponse",
    "PromptResponse",
    "RequestPermissionOutcome",
    "RequestPermissionRequest",
    "SessionConfigOption",
    "SessionMode",
    "SessionUpdate",
];

fn declarations(cfg: &Config) -> Vec<String> {
    macro_rules! decls {
        ($($ty:ty),* $(,)?) => { vec![$(<$ty as TS>::decl(cfg)),*] };
    }
    decls![
        Position,
        GlobalState,
        AgentState,
        AgentPhase,
        AgentLaunch,
        AgentTransport,
        McpServerSummary,
        McpServerKind,
        EpochPredecessor,
        EpochEndReason,
        CatalogState,
        SessionSummary,
        SessionLifecycle,
        SessionWork,
        PendingCounts,
        Request,
        RequestOperation,
        RequestTarget,
        RequestStatus,
        RequestResult,
        AuthTerminalState,
        SessionState,
        SessionPhase,
        SessionHistory,
        CollapsedTurn,
        TimelineRange,
        ActiveTurn,
        TurnOutcome,
        TurnOutcomeStatus,
        SessionOperation,
        OperationKind,
        Interaction,
        UrlFlow,
        UrlFlowStatus,
        TerminalState,
        TerminalExitStatus,
        SessionControls,
        SessionModes,
        SessionInfo,
        SessionUsage,
        Entry,
        ScopeState,
        EndReason,
        Op,
        AppendTail,
        CommandResult<()>,
        Disposition,
        RequestAccepted,
        TurnAccepted,
        EmptyBody,
        AuthenticateBody,
        AuthTerminalStartBody,
        AuthTerminalInputBody,
        TerminalSizeBody,
        SessionCwdBody,
        TurnBody,
        SetModeBody,
        SetConfigBody,
        InteractionResponseBody,
        Snapshot<()>,
        ProtocolError,
        ErrorCode,
        ErrorBody,
    ]
}

fn render() -> String {
    // Positions and incarnations are JSON numbers below 2^53.
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
    for declaration in declarations(&cfg) {
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
