import {
  AGENT_METHODS,
  CLIENT_METHODS,
  PROTOCOL_METHODS,
  type ContentBlock,
  type NesSuggestion,
  type SessionUpdate,
} from "@agentclientprotocol/sdk";

type Values<T> = T[keyof T];
export type CoverageStatus = "supported" | "product-exclusion" | "unadvertised" | "sdk";

export const AGENT_METHOD_COVERAGE = {
  [AGENT_METHODS.initialize]: "supported",
  [AGENT_METHODS.authenticate]: "supported",
  [AGENT_METHODS.providers_list]: "product-exclusion",
  [AGENT_METHODS.providers_set]: "product-exclusion",
  [AGENT_METHODS.providers_disable]: "product-exclusion",
  [AGENT_METHODS.session_new]: "supported",
  [AGENT_METHODS.session_load]: "supported",
  [AGENT_METHODS.session_set_mode]: "supported",
  [AGENT_METHODS.session_set_config_option]: "supported",
  [AGENT_METHODS.session_prompt]: "supported",
  [AGENT_METHODS.session_cancel]: "supported",
  [AGENT_METHODS.mcp_message]: "supported",
  [AGENT_METHODS.session_list]: "supported",
  [AGENT_METHODS.session_delete]: "supported",
  [AGENT_METHODS.session_fork]: "supported",
  [AGENT_METHODS.session_resume]: "supported",
  [AGENT_METHODS.session_close]: "supported",
  [AGENT_METHODS.logout]: "supported",
  [AGENT_METHODS.nes_start]: "unadvertised",
  [AGENT_METHODS.nes_suggest]: "unadvertised",
  [AGENT_METHODS.nes_accept]: "unadvertised",
  [AGENT_METHODS.nes_reject]: "unadvertised",
  [AGENT_METHODS.nes_close]: "unadvertised",
  [AGENT_METHODS.document_did_open]: "unadvertised",
  [AGENT_METHODS.document_did_change]: "unadvertised",
  [AGENT_METHODS.document_did_close]: "unadvertised",
  [AGENT_METHODS.document_did_save]: "unadvertised",
  [AGENT_METHODS.document_did_focus]: "unadvertised",
} as const satisfies Record<Values<typeof AGENT_METHODS>, CoverageStatus>;

export const CLIENT_METHOD_COVERAGE = {
  [CLIENT_METHODS.session_request_permission]: "supported",
  [CLIENT_METHODS.session_update]: "supported",
  [CLIENT_METHODS.fs_write_text_file]: "supported",
  [CLIENT_METHODS.fs_read_text_file]: "supported",
  [CLIENT_METHODS.terminal_create]: "supported",
  [CLIENT_METHODS.terminal_output]: "supported",
  [CLIENT_METHODS.terminal_release]: "supported",
  [CLIENT_METHODS.terminal_wait_for_exit]: "supported",
  [CLIENT_METHODS.terminal_kill]: "supported",
  [CLIENT_METHODS.mcp_connect]: "supported",
  [CLIENT_METHODS.mcp_message]: "supported",
  [CLIENT_METHODS.mcp_disconnect]: "supported",
  [CLIENT_METHODS.elicitation_create]: "supported",
  [CLIENT_METHODS.elicitation_complete]: "supported",
} as const satisfies Record<Values<typeof CLIENT_METHODS>, CoverageStatus>;

export const PROTOCOL_METHOD_COVERAGE = {
  [PROTOCOL_METHODS.cancel_request]: "sdk",
} as const satisfies Record<Values<typeof PROTOCOL_METHODS>, CoverageStatus>;

export const SESSION_UPDATE_COVERAGE = {
  user_message_chunk: "supported",
  agent_message_chunk: "supported",
  agent_thought_chunk: "supported",
  tool_call: "supported",
  tool_call_update: "supported",
  plan: "supported",
  plan_update: "supported",
  plan_removed: "supported",
  available_commands_update: "supported",
  current_mode_update: "supported",
  config_option_update: "supported",
  session_info_update: "supported",
  usage_update: "supported",
  compaction_update: "supported",
  compaction_summary_chunk: "supported",
} as const satisfies Record<SessionUpdate["sessionUpdate"], CoverageStatus>;

export const CONTENT_BLOCK_COVERAGE = {
  text: "supported",
  image: "supported",
  audio: "supported",
  resource_link: "supported",
  resource: "supported",
} as const satisfies Record<ContentBlock["type"], CoverageStatus>;

export const NES_SUGGESTION_COVERAGE = {
  edit: "unadvertised",
  jump: "unadvertised",
  rename: "unadvertised",
  searchAndReplace: "unadvertised",
} as const satisfies Record<NesSuggestion["kind"], CoverageStatus>;
