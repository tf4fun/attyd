import { isAbsoluteWorkspacePath } from "../../../shared/bridge";

const MAX_SESSION_ID_LENGTH = 1_024;
const MAX_PROJECT_PATH_LENGTH = 16_384;

export function readProjectCwdFromPath(pathname: string): string | undefined {
  const match = /^\/projects\/([^/?#]+)(?:\/sessions\/([^/?#]+))?$/.exec(pathname);
  if (!match) return undefined;
  try {
    const cwd = decodeURIComponent(match[1]);
    if (encodeProjectCwd(cwd) == null) return undefined;
    if (match[2] != null && encodeSessionId(decodeURIComponent(match[2])) == null) return undefined;
    return cwd;
  } catch {
    return undefined;
  }
}

export function readSessionIdFromPath(pathname: string): string | undefined {
  const nested = /^\/projects\/[^/?#]+\/sessions\/([^/?#]+)$/.exec(pathname);
  if (nested != null && readProjectCwdFromPath(pathname) == null) return undefined;
  const match = nested ?? /^\/sessions\/([^/?#]+)$/.exec(pathname);
  if (!match) return undefined;
  try {
    const sessionId = decodeURIComponent(match[1]);
    return encodeSessionId(sessionId) == null ? undefined : sessionId;
  } catch {
    return undefined;
  }
}

export function projectPath(cwd: string): string {
  const encoded = encodeProjectCwd(cwd);
  return encoded == null ? "/" : `/projects/${encoded}`;
}

export function sessionPath(sessionId?: string, cwd?: string): string {
  const encoded = sessionId == null ? undefined : encodeSessionId(sessionId);
  if (encoded == null) return "/";
  if (cwd == null) return `/sessions/${encoded}`;
  const project = projectPath(cwd);
  return project === "/" ? "/" : `${project}/sessions/${encoded}`;
}

function encodeProjectCwd(cwd: string): string | undefined {
  if (cwd.length > MAX_PROJECT_PATH_LENGTH || cwd.includes("\0") || !isAbsoluteWorkspacePath(cwd)) {
    return undefined;
  }
  // Keep the Agent's exact path, including separators, case, and trailing slash.
  try {
    return encodeURIComponent(cwd);
  } catch {
    return undefined;
  }
}

function encodeSessionId(sessionId: string): string | undefined {
  if (sessionId.length === 0 || sessionId.length > MAX_SESSION_ID_LENGTH) return undefined;
  // Browsers normalize literal and percent-encoded dot path segments away.
  if (sessionId === "." || sessionId === "..") return undefined;
  try {
    return encodeURIComponent(sessionId);
  } catch {
    return undefined;
  }
}
