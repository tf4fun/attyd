// A minimal client of protocol revision 4 for a small device (docs/protocol-r4-design.md scenario
// S1): it follows the latest output of one session as a window of its text and the head of the
// session's operation, and nothing else. It is built from the shared replica set and shares no state
// with the page: it reads a stream or polls, writes a prompt with the `expect` it read, and keeps
// the figures a device cares about: the largest event, and the text held while one is applied.
//
// A window of `window` bytes holds fewer than `window + L` bytes of text, and one event carries
// fewer than `window + L` bytes of one window's text, so applying an event on a copy needs under
// `2 (window + L)` bytes of text, whatever the output's length.

import { newOperationId } from "../shared/write-resolution.js";
import { RequestOrder } from "../shared/request-order.js";
import { isTextWindow } from "../shared/tree-hash.js";
import { TreeFeedClient, type StreamSource } from "../shared/tree-feed.js";
import { TreeReplicaSet, type Change, type Follow, type PathReplica, type TreeEvent } from "../shared/tree-replica.js";
import { openStream } from "./protocol-smoke-client.js";

export interface ReferenceStats {
  /** Events (or poll answers) received. */
  events: number;
  /** The largest event, in bytes of JSON. */
  maxEventBytes: number;
  /** The most text of windows one event carried, in UTF-8 bytes. */
  maxEventTextBytes: number;
  /** The most text of windows held after an event, in UTF-8 bytes. */
  maxHeldTextBytes: number;
  /** The most text held while an event is applied: what was held plus what the event carried. */
  peakTextBytes: number;
}

export interface OpHead {
  id: string;
  kind: string;
  status: string;
}

export interface OutputWindow {
  turn: { state: "running" | "ended" } | null;
  entry: number | null;
  text: { length: number; offset: number; before: string; text: string };
  other: number;
}

const encoder = new TextEncoder();
const byteLength = (text: string): number => encoder.encode(text).length;

/** The UTF-8 bytes of text a JSON value carries as windows and as the tails of appends. */
function textBytes(value: unknown): number {
  if (typeof value !== "object" || value === null) return 0;
  if (isTextWindow(value)) return byteLength(value.text);
  const record = value as Record<string, unknown>;
  if (record.op === "append" && typeof record.text === "string") return byteLength(record.text);
  return Object.values(record).reduce<number>((total, member) => total + textBytes(member), 0);
}

function emptyStats(): ReferenceStats {
  return { events: 0, maxEventBytes: 0, maxEventTextBytes: 0, maxHeldTextBytes: 0, peakTextBytes: 0 };
}

export class ReferenceClient {
  readonly stats: ReferenceStats = emptyStats();
  readonly streamErrors: string[] = [];
  readonly #follow: readonly Follow[];
  readonly #feed: TreeFeedClient;
  readonly #set = new TreeReplicaSet("/api/v2/stream");
  /** Which answers of a poll may be installed (design §7.3). */
  readonly #order = new RequestOrder();
  readonly #fetch: typeof fetch;
  #held = 0;

  constructor(
    readonly origin: string,
    readonly session: string,
    options: { window?: number; cwd?: string; fetch?: typeof fetch } = {},
  ) {
    this.#fetch = options.fetch ?? ((input, init) => globalThis.fetch(input, init));
    this.#follow = [
      { path: this.outputPath, window: options.window ?? 4096, ...(options.cwd === undefined ? {} : { cwd: options.cwd }) },
      { path: this.opPath },
    ];
    this.#feed = new TreeFeedClient(
      (url) => this.#observe(openStream(new URL(url, origin), (error) => this.streamErrors.push(error))),
      (reconnect) => void setTimeout(reconnect, 100),
    );
  }

  /** The instance of the bridge the replicas belong to. */
  get instance(): string | undefined {
    return this.#feed.instance ?? this.#set.instance;
  }

  get outputPath(): string {
    return `/views/${this.session}/output/latest`;
  }

  get opPath(): string {
    return `/sessions/${this.session}/op`;
  }

  /** Follows the two paths on a stream. */
  stream(): void {
    this.#feed.follow(this.#follow);
  }

  /** Asks for the two paths once, with what the client holds (`GET /api/v2/nodes/…`). */
  async poll(): Promise<"changed" | "unchanged"> {
    let changed = false;
    for (const follow of this.#follow) {
      const ticket = this.#order.issue();
      const node = new URL(`/api/v2/nodes${follow.path}`, this.origin);
      const held = this.#set.get(follow.path);
      if (held !== undefined && this.#set.instance !== undefined) {
        node.searchParams.set("instance", this.#set.instance);
        node.searchParams.set("have", String(held.version));
      }
      if (follow.window !== undefined) node.searchParams.set("window", String(follow.window));
      if (follow.cwd !== undefined) node.searchParams.set("cwd", follow.cwd);
      const response = await this.#fetch(node);
      if (response.status !== 200) throw new Error(`${node.pathname} returned ${response.status}: ${await response.text()}`);
      const answer = await response.json() as { instance: string; rev: number; unchanged?: true; change?: Change };
      if (answer.unchanged === true || answer.change === undefined) continue;
      // A path the client reads but does not follow on a stream: installed only if the instance it
      // was issued under is still the one adopted, and its version is newer than the one held.
      const version = "snapshot" in answer.change ? answer.change.snapshot.at : answer.rev;
      const decision = this.#order.decide(ticket, { instance: answer.instance, version }, this.#replica(follow.path)?.version ?? null, false);
      if (decision === "discard") continue;
      // A concurrent read may already have advanced this path since the request's `have`.
      // Its differential snapshot cannot be applied to that newer base. The next poll asks
      // from the version now held; this is ordinary request ordering, not corrupt content.
      if (decision === "install" && "snapshot" in answer.change && answer.change.snapshot.base !== null &&
          answer.change.snapshot.base !== this.#set.get(follow.path)?.version) continue;
      if (decision === "adopt-and-install") this.#order.adopt(answer.instance);
      const event: TreeEvent = { instance: answer.instance, rev: answer.rev, changes: [answer.change] };
      this.#measure(JSON.stringify(event), event);
      // Polls may overlap: their arrival order is governed above, not by a stream ID allocated
      // when the request was sent. Open the application scope only after accepting the answer.
      const { stream } = this.#set.open(this.#follow);
      const result = this.#set.receive(stream, event);
      if (result.kind === "resubscribe") throw new Error(`the answer for ${follow.path} could not be applied: ${this.#set.lastResubscribe?.reason}`);
      this.#heldAfter();
      changed ||= result.changed.length > 0;
    }
    return changed ? "changed" : "unchanged";
  }

  get output(): OutputWindow | undefined {
    return this.#replica(this.outputPath)?.value as OutputWindow | undefined;
  }

  get op(): OpHead | undefined {
    return this.#replica(this.opPath)?.value as OpHead | undefined;
  }

  /** The path version whose hash the replica of the output last matched. */
  get verified(): number | undefined {
    return this.#replica(this.outputPath)?.verified;
  }

  /** The path version of the operation head held. */
  get opVersion(): number | undefined {
    return this.#replica(this.opPath)?.version;
  }

  get version(): number | undefined {
    return this.#replica(this.outputPath)?.version;
  }

  /** Writes a prompt to the session with the `expect` it read; the answer's status. */
  async prompt(
    text: string,
    request: { id?: string; expect?: string } = {},
  ): Promise<{ status: number; body: unknown; id: string; expect: string }> {
    const expect = request.expect ?? this.op?.id;
    if (expect === undefined) throw new Error("the operation head is not held yet");
    const id = request.id ?? newOperationId();
    const response = await fetch(new URL(`/api/v2/targets/sessions/${encodeURIComponent(this.session)}/ops`, this.origin), {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ id, expect, kind: "prompt", args: { prompt: [{ type: "text", text }] } }),
    });
    return { status: response.status, body: await response.json().catch(() => undefined), id, expect };
  }

  /** Waits until `read` of this client gives a value. */
  until<T>(read: () => T | undefined, label: string, timeout = 20_000): Promise<T> {
    return new Promise<T>((resolve, reject) => {
      const check = (): void => {
        const value = read();
        if (value === undefined) return;
        done();
        resolve(value);
      };
      // A checkpoint changes nothing the feed reports, so the client looks again now and then.
      const unsubscribe = this.#feed.subscribe(check);
      const again = setInterval(check, 50);
      const timer = setTimeout(() => {
        done();
        reject(new Error(`Timed out waiting for ${label}: ${JSON.stringify({ op: this.op, output: this.output && { ...this.output, text: { ...this.output.text, text: this.output.text.text.slice(-40) } } })}`));
      }, timeout);
      const done = (): void => {
        clearTimeout(timer);
        clearInterval(again);
        unsubscribe();
      };
      check();
    });
  }

  close(): void {
    this.#feed.close();
  }

  #replica(path: string): PathReplica | undefined {
    return this.#feed.get(path) ?? this.#set.get(path);
  }

  /** Counts what each event carries and what is held after it, on the way to the feed. */
  #observe(source: StreamSource): StreamSource {
    const mine: StreamSource = { onmessage: null, onerror: null, close: () => source.close() };
    source.onmessage = ({ data }) => {
      this.#measure(data, JSON.parse(data) as TreeEvent);
      mine.onmessage?.({ data });
      this.#heldAfter();
    };
    source.onerror = () => mine.onerror?.();
    return mine;
  }

  #measure(data: string, event: TreeEvent): void {
    const carried = textBytes(event);
    this.stats.events += 1;
    this.stats.maxEventBytes = Math.max(this.stats.maxEventBytes, byteLength(data));
    this.stats.maxEventTextBytes = Math.max(this.stats.maxEventTextBytes, carried);
    this.stats.peakTextBytes = Math.max(this.stats.peakTextBytes, this.#held + carried);
  }

  #heldAfter(): void {
    this.#held = this.output === undefined ? 0 : byteLength(this.output.text.text);
    this.stats.maxHeldTextBytes = Math.max(this.stats.maxHeldTextBytes, this.#held);
  }
}
