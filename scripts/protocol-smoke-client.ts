// The revision 3 client the smokes drive a real server with: the shared `FeedClient` and
// `OpsClient`, fed by a fetch-based SSE reader, since Node has no `EventSource`. It adds
// a wait on a replica, and a write that is read to its end, and nothing else.

import type { DocumentReplica, Follow } from "../shared/document-replica.js";
import type { CatalogLive3, GlobalLive3, SessionLive3, ShownOp } from "../shared/documents.js";
import type { Entry, PromptState } from "../shared/entries.js";
import { FeedClient, type StreamSource } from "../shared/feed-client.js";
import type { HttpFetch } from "../shared/protocol-client.js";
import { ActionsClient, OpsClient, type StartingWrite } from "../shared/write-client.js";

export class SmokeClient {
  readonly replicas: FeedClient;
  readonly ops: OpsClient;
  readonly actions: ActionsClient;
  /** Each stream that failed, with why; a smoke that keeps its stream expects none. */
  readonly streamErrors: string[] = [];

  constructor(readonly origin: string) {
    this.replicas = new FeedClient(
      (url) => openStream(new URL(url, origin), (error) => this.streamErrors.push(error)),
      (reconnect) => void setTimeout(reconnect, 100),
    );
    const http: HttpFetch = (url, init) => fetch(new URL(url, origin), init);
    this.ops = new OpsClient(http, this.replicas);
    this.actions = new ActionsClient(http);
  }

  follow(follow: readonly Follow[]): void {
    this.replicas.follow(follow);
  }

  /** Waits until `read` gives a value for the replica of `scope` as the stream changes it. */
  until<T>(scope: string, read: (replica: DocumentReplica | undefined) => T | undefined, label: string, timeout = 10_000): Promise<T> {
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
        reject(new Error(`Timed out waiting for ${label}: ${JSON.stringify(this.replicas.get(scope)?.doc ?? null)}`));
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
  ended(scope: string, id?: string): Promise<ShownOp> {
    return this.until(scope, (replica) => {
      const op = (replica?.doc.live as { op?: ShownOp } | undefined)?.op;
      return op !== undefined && op.status !== "running" && (id === undefined || op.id === id) ? op : undefined;
    }, `${scope} to show ${id ?? "its operation"} ended`);
  }

  /** Writes an operation that starts one, and reads it to its end from the document of `scope`. */
  async run(scope: string, write: StartingWrite): Promise<ShownOp> {
    await this.ended(scope);
    const outcome = await this.ops.write(scope, write);
    if (outcome.kind !== "accepted") throw new Error(`${write.kind} was not admitted: ${JSON.stringify(outcome)}`);
    return this.ended(scope, outcome.op);
  }

  close(): void {
    this.replicas.close();
  }
}

export function sessionScope(sessionId: string): string {
  return `session/${sessionId}`;
}

export function sessionLive(replica: DocumentReplica | undefined): SessionLive3 | undefined {
  return replica?.doc.live as SessionLive3 | undefined;
}

export function globalLive(replica: DocumentReplica | undefined): GlobalLive3 | undefined {
  return replica?.doc.live as GlobalLive3 | undefined;
}

export function catalogLive(replica: DocumentReplica | undefined): CatalogLive3 | undefined {
  return replica?.doc.live as CatalogLive3 | undefined;
}

/** The entries of a session's document; none while its history is unknown. */
export function entriesOf(replica: DocumentReplica | undefined): readonly Entry[] {
  return (replica?.doc.entries as readonly Entry[] | undefined) ?? [];
}

export function promptsOf(replica: DocumentReplica | undefined): readonly PromptState[] {
  return (replica?.doc.prompts as readonly PromptState[] | undefined) ?? [];
}

/** The text of an Agent message entry, its text chunks joined. */
export function replyText(entry: Entry): string {
  if (entry.kind !== "message") return "";
  return entry.chunks.map((chunk) => {
    const content = chunk.content as { type?: string; text?: string };
    return content.type === "text" ? content.text ?? "" : "";
  }).join("");
}

/** The Agent messages of a document, in order. */
export function agentMessages(replica: DocumentReplica | undefined): Entry[] {
  return entriesOf(replica).filter((entry) => entry.kind === "message" && entry.role === "agent");
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
