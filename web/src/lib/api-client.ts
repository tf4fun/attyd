import i18n from "../i18n";

export class ApiError extends Error {
  constructor(
    message: string,
    readonly status: number,
    readonly body?: unknown,
  ) {
    super(message);
    this.name = "ApiError";
  }
}

export const REQUEST_TIMEOUT_MS = 30_000;

export class RequestTimeoutError extends Error {
  constructor(readonly timeoutMs: number, readonly method = "GET") {
    super(i18n.t(method === "GET" || method === "HEAD" ? "errors.requestTimeout" : "errors.requestTimeoutUncertain", {
      seconds: Math.ceil(timeoutMs / 1000),
    }));
    this.name = "RequestTimeoutError";
  }
}

export async function requestJson<T>(
  path: string,
  init: RequestInit = {},
): Promise<T> {
  const caller = init.signal;
  if (caller?.aborted) throw caller.reason;
  const method = (init.method ?? "GET").toUpperCase();
  const headers = new Headers(init.headers);
  if (init.body != null && !headers.has("content-type")) {
    headers.set("content-type", "application/json");
  }
  const controller = new AbortController();
  const cancel = () => controller.abort(caller?.reason);
  let rejectAbort!: () => void;
  const aborted = new Promise<never>((_resolve, reject) => {
    rejectAbort = () => reject(controller.signal.reason);
    controller.signal.addEventListener("abort", rejectAbort, { once: true });
  });
  caller?.addEventListener("abort", cancel, { once: true });
  const timer = setTimeout(() => controller.abort(new RequestTimeoutError(REQUEST_TIMEOUT_MS, method)), REQUEST_TIMEOUT_MS);
  try {
    // Cover the entire read, including a body stalled after successful headers.
    // Race as well as abort so callers settle even if a transport ignores abort.
    return await Promise.race([aborted, (async () => {
      const response = await fetch(path, { ...init, headers, signal: controller.signal });
      const text = await response.text();
      let body: unknown;
      if (text !== "") {
        try { body = JSON.parse(text); } catch { body = text; }
      }
      if (!response.ok) {
        if (response.status === 504 && isRecord(body) && body.code === "request_timeout" &&
          typeof body.timeoutMs === "number" && Number.isFinite(body.timeoutMs) && body.timeoutMs > 0) {
          throw new RequestTimeoutError(body.timeoutMs, method);
        }
        const message = isRecord(body) && typeof body.error === "string"
          ? body.error
          : isRecord(body) && isRecord(body.error) && typeof body.error.message === "string"
            ? body.error.message
            : `${method} ${path} failed (${response.status})`;
        throw new ApiError(message, response.status, body);
      }
      return body as T;
    })()]);
  } finally {
    clearTimeout(timer);
    caller?.removeEventListener("abort", cancel);
    controller.signal.removeEventListener("abort", rejectAbort);
  }
}

export function workspaceContextSearchPath(query: string, sessionId: string): string {
  return `/api/v2/sessions/${encodeURIComponent(sessionId)}/files?${new URLSearchParams({ query })}`;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return value != null && typeof value === "object" && !Array.isArray(value);
}
