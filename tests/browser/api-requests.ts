import type { Page, Request } from "@playwright/test";

export interface ApiRequests {
  /** Every `/api/v1` request the page sent. */
  v1: string[];
  /** Every stream subscription the page opened. */
  streams: URL[];
  /** One-off `GET …/scopes/…` reads. */
  reads: URL[];
  /** Operation writes: the scope and the request body. */
  ops: Array<{ scope: string; kind: string; body: Record<string, unknown> }>;
  /** Resource actions, as `<scope> <resource> <action>`. */
  actions: string[];
  /** Controls and close (`…/actions/<action>`): the scope, the action and the request body. */
  controls: Array<{ scope: string; action: string; body: Record<string, unknown> }>;
  /** Any request for a turn's process (removed with v1). */
  process: string[];
}

/**
 * Records the API requests a page sends. Assertions on counts first establish
 * that the page uses v2 at all (a stream was opened), so a zero count means the
 * page did not send the request, not that a matcher found nothing.
 */
export function trackApi(page: Page): ApiRequests {
  const requests: ApiRequests = { v1: [], streams: [], reads: [], ops: [], actions: [], controls: [], process: [] };
  page.on("request", (request: Request) => {
    const url = new URL(request.url());
    const path = url.pathname;
    if (path.startsWith("/api/v1")) requests.v1.push(`${request.method()} ${path}`);
    if (path.includes("/process")) requests.process.push(path);
    if (path === "/api/v3/stream") requests.streams.push(url);
    if (!path.startsWith("/api/v3/scopes/")) return;
    const ops = /^\/api\/v3\/scopes\/(.+)\/ops$/u.exec(path);
    const action = /^\/api\/v3\/scopes\/(.+)\/resources\/([^/]+)\/([^/]+)$/u.exec(path);
    const control = /^\/api\/v3\/scopes\/(.+)\/actions\/([^/]+)$/u.exec(path);
    if (request.method() === "GET") requests.reads.push(url);
    else if (ops != null) {
      const body = request.postDataJSON() as Record<string, unknown>;
      requests.ops.push({ scope: decodeURIComponent(ops[1]!), kind: String(body.kind), body });
    } else if (action != null) {
      requests.actions.push(`${decodeURIComponent(action[1]!)} ${decodeURIComponent(action[2]!)} ${action[3]}`);
    } else if (control != null) {
      requests.controls.push({
        scope: decodeURIComponent(control[1]!), action: control[2]!, body: request.postDataJSON() as Record<string, unknown>,
      });
    }
  });
  return requests;
}

/** The scopes a stream subscription follows. */
export function scopesOf(url: URL): string[] {
  return url.searchParams.getAll("scope");
}

/** Controls of one kind (`set_mode`, `set_config`, `close`), optionally on one scope. */
export function controlsOf(requests: ApiRequests, action: string, scope?: string): Array<Record<string, unknown>> {
  return requests.controls.filter((control) => control.action === action && (scope == null || control.scope === scope))
    .map((control) => control.body);
}

/** Operation writes of one kind, optionally on one scope. */
export function opsOf(requests: ApiRequests, kind: string, scope?: string): Array<Record<string, unknown>> {
  return requests.ops.filter((op) => op.kind === kind && (scope == null || op.scope === scope)).map((op) => op.body);
}
