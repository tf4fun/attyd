// The stream client of revision 3 (docs/protocol.md §5.1, §5.4). It follows a set
// of scopes on one stream at a time and keeps their verified replicas; it reads
// nothing over HTTP and holds no authoritative state (C1).
//
// A new set closes the stream it replaces. A stream that breaks, an event that
// does not apply and a replica that does not match the bridge's digest close the
// stream too, and a new one subscribes after a delay that grows while nothing is
// delivered, with the version held (or, when the content disagreed, without it,
// so the bridge sends the whole document). The client says whether it is
// synchronized or retrying.

import { DocumentReplicaSet, type DocumentReplica, type Follow, type StreamEvent } from "./document-replica";

/** One SSE connection, as `EventSource` offers it. */
export interface StreamSource {
  onmessage: ((message: { data: string }) => void) | null;
  onerror: (() => void) | null;
  close(): void;
}

export type OpenStream = (url: string) => StreamSource;

/** Runs `reconnect` after `delayMs`. */
export type Schedule = (reconnect: () => void, delayMs: number) => void;

export type FeedStatus =
  | { readonly state: "synchronized" }
  /** The stream failed `attempt` times in a row without delivering; the next try is `delayMs` away. */
  | { readonly state: "retrying"; readonly attempt: number; readonly delayMs: number; readonly reason: string };

const FIRST_DELAY_MS = 1000;
const LAST_DELAY_MS = 30_000;

export class FeedClient {
  readonly #set = new DocumentReplicaSet("/api/v3/stream");
  readonly #listeners = new Set<(scope: string) => void>();
  readonly #statusListeners = new Set<(status: FeedStatus) => void>();
  readonly #openStream: OpenStream;
  readonly #schedule: Schedule;
  #follow: readonly Follow[] = [];
  #source: StreamSource | undefined;
  /** Counts connections; a callback or reconnect of an earlier one changes nothing. */
  #generation = 0;
  #closed = false;
  #failures = 0;
  #status: FeedStatus | undefined;

  constructor(
    openStream: OpenStream,
    schedule: Schedule = (reconnect, delayMs) => void setTimeout(reconnect, delayMs),
  ) {
    this.#openStream = openStream;
    this.#schedule = schedule;
  }

  follow(follow: readonly Follow[]): void {
    if (this.#closed) return;
    this.#follow = follow;
    this.#connect();
  }

  get(scope: string): DocumentReplica | undefined {
    return this.#set.get(scope);
  }

  /** Calls `listener` with a scope each time its replica changes or ends. */
  subscribe(listener: (scope: string) => void): () => void {
    this.#listeners.add(listener);
    return () => {
      this.#listeners.delete(listener);
    };
  }

  /** Calls `listener` whenever the client becomes synchronized or retries. */
  onStatus(listener: (status: FeedStatus) => void): () => void {
    this.#statusListeners.add(listener);
    return () => {
      this.#statusListeners.delete(listener);
    };
  }

  close(): void {
    this.#closed = true;
    this.#disconnect();
  }

  #connect(): void {
    this.#disconnect();
    const generation = this.#generation;
    const { stream, url } = this.#set.open(this.#follow);
    const source = this.#openStream(url);
    this.#source = source;
    source.onmessage = ({ data }) => {
      if (generation === this.#generation) this.#receive(stream, data);
    };
    source.onerror = () => {
      if (generation === this.#generation) this.#broken("the stream failed");
    };
  }

  /** Closes the current stream, if any, and lets nothing of it through. */
  #disconnect(): void {
    this.#generation += 1;
    const source = this.#source;
    this.#source = undefined;
    if (source === undefined) return;
    source.onmessage = null;
    source.onerror = null;
    source.close();
  }

  #receive(stream: number, data: string): void {
    let scope: string;
    let received: ReturnType<DocumentReplicaSet["receive"]>;
    try {
      const event = JSON.parse(data) as StreamEvent;
      scope = event.scope;
      received = this.#set.receive(stream, event);
    } catch {
      // Not an event of the protocol: the stream cannot be trusted further.
      this.#broken("the stream sent something that is not an event");
      return;
    }
    if (received === "resubscribe") {
      this.#broken(this.#set.lastResubscribe?.reason ?? "the replica could not be applied");
    } else if (received !== "ignored") {
      this.#failures = 0;
      this.#report({ state: "synchronized" });
      this.#listeners.forEach((listener) => listener(scope));
    }
  }

  /** The stream failed: close it, and subscribe again later (§5.4). */
  #broken(reason: string): void {
    this.#disconnect();
    const generation = this.#generation;
    this.#failures += 1;
    const delayMs = Math.min(FIRST_DELAY_MS * 2 ** (this.#failures - 1), LAST_DELAY_MS);
    this.#report({ state: "retrying", attempt: this.#failures, delayMs, reason });
    this.#schedule(() => {
      if (!this.#closed && generation === this.#generation) this.#connect();
    }, delayMs);
  }

  #report(status: FeedStatus): void {
    const previous = this.#status;
    this.#status = status;
    if (previous?.state === status.state && status.state === "synchronized") return;
    this.#statusListeners.forEach((listener) => listener(status));
  }
}
