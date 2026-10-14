import type {
  CommandResult,
  GlobalState,
  Position,
  Request,
  RequestAccepted,
  SessionState,
  Snapshot,
} from "../../../shared/protocol";
import { GLOBAL_SCOPE, Replica, ReplicaError, entryScope } from "../../../shared/replica";
import { ApiError, RequestTimeoutError, requestJson } from "./api-client";

export interface EventSourceLike {
  readonly readyState: number;
  onopen: ((event: Event) => void) | null;
  onmessage: ((event: MessageEvent) => void) | null;
  onerror: ((event: Event) => void) | null;
  close(): void;
}

export interface V2ClientOptions {
  eventSource?: (url: string) => EventSourceLike;
  /** Called after the replica changed, with the scopes that changed. */
  onChange?: (scopes: ReadonlySet<string>) => void;
  onOpen?: () => void;
  onError?: () => void;
  /** A replica or transport failure that is not a single command's. */
  onFailure?: (error: unknown) => void;
}

export interface CommandInit {
  method?: string;
  body?: unknown;
  /** `Idempotency-Key`: operation commands and turn start (protocol §5.2). */
  key?: string;
  /** `If-Match`, unquoted (protocol §5.3). */
  ifMatch?: string;
}

const CLOSED = 2;
const LOST_RESPONSE_RETRIES = 2;
const RETRY_MS = 1000;

/** The v2 code of an error response, if it has one. */
export function protocolErrorCode(error: unknown): string | undefined {
  if (!(error instanceof ApiError)) return undefined;
  const body = error.body;
  if (body == null || typeof body !== "object") return undefined;
  const inner = (body as { error?: unknown }).error;
  return inner != null && typeof inner === "object" && typeof (inner as { code?: unknown }).code === "string"
    ? (inner as { code: string }).code
    : undefined;
}

function lostResponse(error: unknown): boolean {
  return error instanceof RequestTimeoutError || error instanceof TypeError;
}

/**
 * One browser's connection to protocol v2: a stream over the subscribed
 * scopes feeding a {@link Replica}, snapshot fetches for scopes that reset,
 * and commands that resolve once their effect is applied (read your writes).
 */
export class V2Client {
  readonly replica = new Replica();
  private source: EventSourceLike | undefined;
  private lastEventId: string | undefined;
  private fetching = new Set<string>();
  private requestWaiters = new Set<() => void>();
  private disposed = false;

  constructor(private readonly options: V2ClientOptions = {}) {
    this.replica.onChange((scopes) => {
      if (scopes.has(GLOBAL_SCOPE)) for (const waiter of [...this.requestWaiters]) waiter();
      this.options.onChange?.(scopes);
    });
  }

  /** Opens a new stream for `subscriptions`; every scope starts pending. */
  connect(subscriptions: readonly string[]): void {
    if (this.disposed) return;
    this.source?.close();
    this.replica.subscribe(subscriptions);
    this.lastEventId = undefined;
    this.fetching.clear();
    const query = new URLSearchParams();
    for (const subscription of subscriptions) query.append("scope", subscription);
    const create = this.options.eventSource ?? ((url: string) => new EventSource(url));
    const source = create(`/api/v2/stream?${query}`);
    this.source = source;
    let opened = false;
    source.onopen = () => {
      if (this.source !== source) return;
      // A native reconnect resumes the same stream after its Last-Event-ID.
      if (opened) this.replica.reconnect(this.lastEventId);
      opened = true;
      this.options.onOpen?.();
    };
    source.onmessage = (event) => {
      if (this.source !== source) return;
      try {
        this.replica.receive(String(event.data), event.lastEventId ?? "");
        if (event.lastEventId) this.lastEventId = event.lastEventId;
      } catch (error) {
        // A stream that breaks the protocol is started over from snapshots.
        this.options.onFailure?.(error);
        this.connect(this.replica.subscriptions());
        return;
      }
      this.fetchSnapshots();
    };
    source.onerror = () => {
      if (this.source !== source) return;
      this.options.onError?.();
      // EventSource gives up on a non-200 answer, such as 503 while the bridge
      // replaces its Agent connection. Resync from scratch shortly after.
      if (source.readyState === CLOSED) {
        setTimeout(() => {
          if (this.source === source && !this.disposed) this.connect(this.replica.subscriptions());
        }, RETRY_MS);
      }
    };
  }

  /** The current stream is closed and will not retry by itself. */
  closed(): boolean {
    return this.source == null || this.source.readyState === CLOSED;
  }

  dispose(): void {
    this.disposed = true;
    this.source?.close();
    this.source = undefined;
    for (const waiter of [...this.requestWaiters]) waiter();
    this.requestWaiters.clear();
  }

  global(): GlobalState | undefined {
    return this.replica.document<GlobalState>(GLOBAL_SCOPE);
  }

  session(sessionId: string): SessionState | undefined {
    return this.replica.document<SessionState>(`session/${sessionId}`);
  }

  private fetchSnapshots(): void {
    for (const subscription of this.replica.fetches()) {
      if (this.fetching.has(subscription)) continue;
      this.fetching.add(subscription);
      const source = this.source;
      const scope = entryScope(subscription);
      const path = scope === GLOBAL_SCOPE
        ? "/api/v2/global"
        : `/api/v2/sessions/${encodeURIComponent(scope.slice("session/".length))}`;
      void requestJson<Snapshot<unknown>>(path).then((snapshot) => {
        if (this.source !== source) return;
        this.fetching.delete(subscription);
        // A snapshot from before the reset, or of another owner, waits for the next.
        if (!this.replica.install(subscription, snapshot) && this.replica.fetches().includes(subscription)) {
          setTimeout(() => this.source === source && this.fetchSnapshots(), 250);
        }
      }, (error: unknown) => {
        if (this.source !== source) return;
        this.fetching.delete(subscription);
        // An unheld session ends on the stream; anything else is retried.
        if (protocolErrorCode(error) === "not_found") return;
        setTimeout(() => this.source === source && this.fetchSnapshots(), 1000);
      });
    }
  }

  /**
   * Sends a command and resolves once its effect is applied. A response lost
   * in transit is recovered by sending the same `Idempotency-Key` again: the
   * bridge answers a known key from its intent table and never runs it twice.
   */
  async command<T>(path: string, init: CommandInit = {}): Promise<CommandResult<T>> {
    const headers: Record<string, string> = {};
    if (init.key != null) headers["Idempotency-Key"] = init.key;
    if (init.ifMatch != null) headers["If-Match"] = `"${init.ifMatch}"`;
    for (let attempt = 0; ; attempt += 1) {
      try {
        const result = await requestJson<CommandResult<T>>(path, {
          method: init.method ?? "POST",
          headers,
          body: init.body === undefined ? undefined : JSON.stringify(init.body),
        });
        await this.replica.whenApplied(result.at);
        return result;
      } catch (error) {
        if (init.key == null || attempt >= LOST_RESPONSE_RETRIES || !lostResponse(error)) throw error;
      }
    }
  }

  /** An operation command, resolving with its finished request record. */
  async operation(path: string, init: CommandInit & { key: string }): Promise<{ accepted: RequestAccepted; record?: Request }> {
    const { result } = await this.command<RequestAccepted>(path, init);
    return { accepted: result, record: await this.finished(result.requestId) };
  }

  /**
   * Resolves with the request record once it finished, or `undefined` once
   * it is no longer public (acknowledged, retained out, or another epoch).
   */
  finished(requestId: string): Promise<Request | undefined> {
    const epoch = this.replica.epoch;
    return new Promise((resolve) => {
      const check = () => {
        const record = this.global()?.requests[requestId];
        if (this.disposed || this.replica.epoch !== epoch || (record == null && !this.replica.pending(GLOBAL_SCOPE))) {
          this.requestWaiters.delete(check);
          resolve(record?.status === "running" ? undefined : record);
          return;
        }
        if (record != null && record.status !== "running") {
          this.requestWaiters.delete(check);
          resolve(record);
        }
      };
      this.requestWaiters.add(check);
      check();
    });
  }

  /** Waits until the replica applied `at`, or rejects on an epoch change. */
  applied(at: Position): Promise<void> {
    return this.replica.whenApplied(at);
  }
}

export { ReplicaError };
