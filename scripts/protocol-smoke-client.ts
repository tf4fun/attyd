// The protocol v2 client the smokes drive a real server with: the shared
// `ReplicaClient` and `OpsClient`, fed by a fetch-based SSE reader, since
// Node has no `EventSource`. It adds a wait on a replica and nothing else.

import type { GlobalLive, LogEntry, Operation, SessionLive } from "../shared/protocol.js";
import { OpsClient, ReplicaClient, type HttpFetch, type StreamSource } from "../shared/protocol-client.js";
import type { Follow, Replica } from "../shared/replica.js";

export class SmokeClient {
  readonly replicas: ReplicaClient;
  readonly ops: OpsClient;
  /** Each stream that failed, with why; a smoke that keeps its stream expects none. */
  readonly streamErrors: string[] = [];

  constructor(readonly origin: string) {
    this.replicas = new ReplicaClient(
      (url) => openStream(new URL(url, origin), (error) => this.streamErrors.push(error)),
      (reconnect) => void setTimeout(reconnect, 100),
    );
    const http: HttpFetch = (url, init) => fetch(new URL(url, origin), init);
    this.ops = new OpsClient(http, this.replicas);
  }

  follow(follow: readonly Follow[]): void {
    this.replicas.follow(follow);
  }

  /** Waits until `read` gives a value for the replica of `scope` as the stream changes it. */
  until<T>(scope: string, read: (replica: Replica | undefined) => T | undefined, label: string, timeout = 10_000): Promise<T> {
    return new Promise<T>((resolve, reject) => {
      const check = () => {
        let value: T | undefined;
        try {
          value = read(this.replicas.get(scope));
        } catch (error) {
          finish();
          reject(error);
          return;
        }
        if (value === undefined) return;
        finish();
        resolve(value);
      };
      const unsubscribe = this.replicas.subscribe((changed) => {
        if (changed === scope) check();
      });
      const timer = setTimeout(() => {
        finish();
        reject(new Error(`Timed out waiting for ${label}: ${JSON.stringify(this.replicas.get(scope) ?? null)}`));
      }, timeout);
      const finish = () => {
        clearTimeout(timer);
        unsubscribe();
      };
      check();
    });
  }

  /**
   * Waits until the replica of `scope` shows its `op` ended, as a write
   * starting the next needs; with `id`, that operation.
   */
  ended(scope: string, id?: string): Promise<Operation> {
    return this.until(scope, (replica) => {
      const op = (replica?.live as { op?: Operation } | undefined)?.op;
      return op !== undefined && op.status !== "running" && (id === undefined || op.id === id) ? op : undefined;
    }, `${scope} to show ${id ?? "its operation"} ended`);
  }

  close(): void {
    this.replicas.close();
  }
}

export function sessionScope(sessionId: string): string {
  return `session/${sessionId}`;
}

export function sessionLive(replica: Replica | undefined): SessionLive | undefined {
  return replica?.live as SessionLive | undefined;
}

export function globalLive(replica: Replica | undefined): GlobalLive | undefined {
  return replica?.live as GlobalLive | undefined;
}

/** The text of a log entry's reply, its text blocks joined. */
export function replyText(entry: LogEntry): string {
  return entry.turn.reply.map((block) => (block.type === "text" ? block.text : "")).join("");
}

/** One SSE stream as `EventSource` offers it; it reports a failure once, through `onerror`. */
function openStream(url: URL, failed: (error: string) => void): StreamSource {
  const controller = new AbortController();
  const source: StreamSource = { onmessage: null, onerror: null, close: () => controller.abort() };
  void (async () => {
    const response = await fetch(url, { signal: controller.signal, headers: { accept: "text/event-stream" } });
    if (response.status !== 200 || response.body == null) {
      throw new Error(`${url.pathname}${url.search} returned ${response.status}: ${await response.text()}`);
    }
    const decoder = new TextDecoder();
    const reader = response.body.getReader();
    let pending = "";
    for (let chunk = await reader.read(); !chunk.done; chunk = await reader.read()) {
      pending += decoder.decode(chunk.value, { stream: true });
      let boundary: number;
      while ((boundary = pending.indexOf("\n\n")) >= 0) {
        const frame = pending.slice(0, boundary);
        pending = pending.slice(boundary + 2);
        const data = frame.split("\n").filter((line) => line.startsWith("data:"))
          .map((line) => line.slice(5).replace(/^ /u, "")).join("\n");
        // Heartbeats are comments, with no data.
        if (data !== "") source.onmessage?.({ data });
      }
    }
    throw new Error(`${url.pathname}${url.search} ended`);
  })().catch((error: unknown) => {
    if (controller.signal.aborted) return;
    failed(String(error));
    source.onerror?.();
  });
  return source;
}
