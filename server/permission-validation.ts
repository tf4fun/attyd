import type { RequestPermissionRequest } from "@agentclientprotocol/sdk";
import {
  validateAndTrackToolCallPatch,
  type SessionUpdateValidationContext,
  type SessionUpdateValidationState,
} from "./session-update-validation.js";

const MAX_PERMISSION_OPTIONS = 100;
const MAX_PERMISSION_REQUEST_BYTES = 1_000_000;
const MAX_IDENTIFIER_LENGTH = 1_024;
const MAX_OPTION_NAME_LENGTH = 4_096;

export function validatePermissionRequest(
  request: RequestPermissionRequest,
  updates: SessionUpdateValidationState,
  context: SessionUpdateValidationContext = {},
): void {
  if (Buffer.byteLength(JSON.stringify(request), "utf8") > MAX_PERMISSION_REQUEST_BYTES) {
    throw new Error(`Agent permission request exceeds ${MAX_PERMISSION_REQUEST_BYTES} bytes`);
  }
  // The permission request itself carries a ToolCallUpdate and may be the
  // first mention of that tool. Treat it as an upsert, as Zed does.
  validateAndTrackToolCallPatch(updates, request.toolCall, context);
  if (request.options.length === 0 || request.options.length > MAX_PERMISSION_OPTIONS) {
    throw new Error(
      `Agent permission request must contain between 1 and ${MAX_PERMISSION_OPTIONS} options`,
    );
  }
  const optionIds = new Set<string>();
  for (const option of request.options) {
    if (option.optionId.length === 0 || option.optionId.length > MAX_IDENTIFIER_LENGTH) {
      throw new Error("Agent returned an invalid permission option ID");
    }
    if (optionIds.has(option.optionId)) {
      throw new Error(`Agent returned duplicate permission option ID: ${option.optionId}`);
    }
    optionIds.add(option.optionId);
    if (option.name.length === 0 || option.name.length > MAX_OPTION_NAME_LENGTH) {
      throw new Error(`Agent returned an invalid permission option name: ${option.optionId}`);
    }
  }
}
