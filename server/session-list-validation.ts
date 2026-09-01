import type { ListSessionsResponse } from "@agentclientprotocol/sdk";
import { isAbsoluteWorkspacePath } from "../shared/bridge.js";
import { validateSessionMetadata } from "./session-update-validation.js";

const MAX_LISTED_SESSIONS = 10_000;
const MAX_CURSOR_LENGTH = 4_096;
const MAX_SESSION_PATH_LENGTH = 16_384;
const MAX_ADDITIONAL_DIRECTORIES = 256;
const MAX_SESSION_LIST_PAGE_BYTES = 4_000_000;
const MAX_SESSION_LIST_TOTAL_BYTES = 16_000_000;

export interface SessionListValidationContext {
  listedSessionIds: ReadonlySet<string>;
  listedCursors: ReadonlySet<string>;
  accumulatedBytes: number;
}

export function validateSessionListPage(
  response: ListSessionsResponse,
  requestedCursor: string | undefined,
  requestedCwd: string | undefined,
  context: SessionListValidationContext,
): number {
  const pageBytes = Buffer.byteLength(JSON.stringify(response), "utf8");
  if (pageBytes > MAX_SESSION_LIST_PAGE_BYTES) {
    throw new Error(`Agent session/list page exceeds ${MAX_SESSION_LIST_PAGE_BYTES} bytes`);
  }
  const previousBytes = requestedCursor === undefined ? 0 : context.accumulatedBytes;
  if (previousBytes + pageBytes > MAX_SESSION_LIST_TOTAL_BYTES) {
    throw new Error(
      `Agent session/list results exceed ${MAX_SESSION_LIST_TOTAL_BYTES} cumulative bytes`,
    );
  }
  if (response.sessions.length > MAX_LISTED_SESSIONS) {
    throw new Error(`Agent returned more than ${MAX_LISTED_SESSIONS} sessions in one page`);
  }
  if (
    response.nextCursor != null &&
    (response.nextCursor.length === 0 || response.nextCursor.length > MAX_CURSOR_LENGTH)
  ) {
    throw new Error("Agent returned an invalid session/list cursor");
  }
  if (response.nextCursor != null && response.nextCursor === requestedCursor) {
    throw new Error("Agent returned the same session/list cursor that was requested");
  }
  if (
    response.nextCursor != null &&
    requestedCursor !== undefined &&
    context.listedCursors.has(response.nextCursor)
  ) {
    throw new Error(`Agent returned a reused session/list cursor: ${response.nextCursor}`);
  }

  const pageIds = new Set<string>();
  let added = 0;
  for (const session of response.sessions) {
    if (session.sessionId.length === 0 || session.sessionId.length > 1_024) {
      throw new Error("Agent returned an invalid listed session ID");
    }
    const newlySeenOnPage = !pageIds.has(session.sessionId);
    pageIds.add(session.sessionId);

    validateAbsoluteSessionPath(session.cwd, `session cwd: ${session.sessionId}`);
    const additionalDirectories = session.additionalDirectories ?? [];
    if (additionalDirectories.length > MAX_ADDITIONAL_DIRECTORIES) {
      throw new Error(
        `Agent listed session ${session.sessionId} with more than ${MAX_ADDITIONAL_DIRECTORIES} additional directories`,
      );
    }
    for (const directory of additionalDirectories) {
      validateAbsoluteSessionPath(
        directory,
        `additional directory for session ${session.sessionId}`,
      );
    }
    validateSessionMetadata(
      session.title,
      session.updatedAt,
      `Agent listed session ${session.sessionId}`,
    );
    if (!context.listedSessionIds.has(session.sessionId) && newlySeenOnPage) added += 1;
  }

  const existing = requestedCursor === undefined ? 0 : context.listedSessionIds.size;
  if (existing + added > MAX_LISTED_SESSIONS) {
    throw new Error(`Agent session/list results exceed ${MAX_LISTED_SESSIONS} sessions`);
  }
  return pageBytes;
}

function validateAbsoluteSessionPath(path: string, subject: string): void {
  if (
    path.length === 0 ||
    path.length > MAX_SESSION_PATH_LENGTH ||
    path.includes("\0") ||
    !isAbsoluteWorkspacePath(path)
  ) {
    throw new Error(
      `Agent returned an invalid absolute ${subject} (maximum ${MAX_SESSION_PATH_LENGTH} characters)`,
    );
  }
}
