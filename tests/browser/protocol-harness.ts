// Barriers for the protocol v2 browser gates.
//
// `BarrierProxy` sits between a page and the real Rust server. It forwards every
// request unchanged and records what actually crossed the wire, so tests count
// the browser's real POSTs and read their bodies. It can hold a request before it
// reaches the server, hold or lose a response after the server answered, and
// pause, break or inject into each SSE stream at event granularity. Every
// response is sent with `Connection: close`, so the browser never retries a
// request on a reused socket and a lost response is never resent by the network
// stack itself.
//
// `Gates` drives the scripted Agent (`tests/fixtures/protocol-ui-agent.ts`)
// through files: barriers it waits at, commands it runs outside any request, and
// the ACP calls it received.
import { expect } from "@playwright/test";
import { existsSync } from "node:fs";
import { mkdtemp, readFile, rename, rm, writeFile } from "node:fs/promises";
import { createServer, request as httpRequest, type IncomingMessage, type Server, type ServerResponse } from "node:http";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { Block, Event, GlobalLive, LogEntry, OpRequest, SessionLive } from "../../shared/protocol";
import { applyEvent, createReplica, headOf, type Replica } from "../../shared/replica";
import { startRustTestServer, type RustTestServer } from "../../scripts/rust-test-server";

export interface Exchange {
  method: string;
  path: string;
  url: URL;
  body: string;
  status?: number;
  responseBody?: string;
  /** The response never reached the browser. */
  lost?: boolean;
}

export interface OpPost {
  scope: string;
  request: OpRequest;
  exchange: Exchange;
}

type Matcher = (exchange: Exchange) => boolean;

interface Rule {
  matches: Matcher;
  remaining: number;
  beforeForward?: (exchange: Exchange) => Promise<void>;
  beforeRespond?: (exchange: Exchange) => Promise<void>;
  lose?: boolean;
}

export class Barrier {
  private reachedResolve!: () => void;
  private openResolve!: () => void;
  readonly reached = new Promise<void>((resolve) => { this.reachedResolve = resolve; });
  private readonly opened = new Promise<void>((resolve) => { this.openResolve = resolve; });
  exchange?: Exchange;

  async wait(exchange: Exchange): Promise<void> {
    this.exchange = exchange;
    this.reachedResolve();
    await this.opened;
  }

  open(): void {
    this.openResolve();
  }
}

export class StreamTap {
  readonly url: URL;
  /** Events the server sent on this stream, in order. */
  readonly received: Event[] = [];
  /** Events handed to the browser, in order (injected ones included). */
  readonly delivered: Event[] = [];
  private paused = false;
  private held: string[] = [];
  private holdBefore?: (event: Event) => boolean;
  private closedResolve!: () => void;
  /** Resolves when the browser closed this stream or the tap broke it. */
  readonly closed = new Promise<void>((resolve) => { this.closedResolve = resolve; });
  isClosed = false;

  constructor(url: URL, private readonly response: ServerResponse, private readonly upstream: () => void) {
    this.url = url;
    response.on("close", () => {
      this.isClosed = true;
      this.upstream();
      this.closedResolve();
    });
  }

  get scopes(): string[] {
    return this.url.searchParams.getAll("scope");
  }

  /** The `have` value for `scope`, as `<n>:<hash>`. */
  have(scope: string): string | undefined {
    const prefix = `${scope}:`;
    return this.url.searchParams.getAll("have").find((value) => value.startsWith(prefix))?.slice(prefix.length);
  }

  cwd(scope: string): string | undefined {
    const prefix = `${scope}:`;
    return this.url.searchParams.getAll("cwd").find((value) => value.startsWith(prefix))?.slice(prefix.length);
  }

  /** Stops delivering events; later ones queue in order. */
  pause(): void {
    this.paused = true;
  }

  /** Pauses right before the first received event that matches. */
  pauseBefore(matches: (event: Event) => boolean): void {
    this.holdBefore = matches;
  }

  resume(): void {
    this.paused = false;
    this.holdBefore = undefined;
    const frames = this.held;
    this.held = [];
    for (const frame of frames) this.forward(frame);
  }

  get queued(): number {
    return this.held.length;
  }

  /** Ends the stream as a network break, without a clean end. */
  break(): void {
    this.response.socket?.destroy();
  }

  inject(raw: string): void {
    this.response.write(`data: ${raw}\n\n`);
    try {
      this.delivered.push(JSON.parse(raw) as Event);
    } catch {
      // A malformed event is delivered as it is.
    }
  }

  /** @internal */
  frame(frame: string): void {
    const event = parseFrame(frame);
    if (event != null) {
      this.received.push(event);
      if (this.holdBefore?.(event)) {
        this.holdBefore = undefined;
        this.paused = true;
      }
    }
    if (this.paused) this.held.push(frame);
    else this.forward(frame);
  }

  private forward(frame: string): void {
    if (this.isClosed) return;
    this.response.write(`${frame}\n\n`);
    const event = parseFrame(frame);
    if (event != null) this.delivered.push(event);
  }
}

function parseFrame(frame: string): Event | undefined {
  const data = frame.split("\n").filter((line) => line.startsWith("data:"))
    .map((line) => line.slice(5).replace(/^ /u, "")).join("\n");
  return data === "" ? undefined : JSON.parse(data) as Event;
}

export class BarrierProxy {
  readonly exchanges: Exchange[] = [];
  readonly streams: StreamTap[] = [];
  private readonly rules: Rule[] = [];
  private readonly streamWaiters: Array<{ matches: (tap: StreamTap) => boolean; resolve: (tap: StreamTap) => void }> = [];

  private constructor(private readonly server: Server, readonly port: number, private readonly target: number) {}

  static async start(target: number): Promise<BarrierProxy> {
    let proxy!: BarrierProxy;
    const server = createServer((request, response) => void proxy.handle(request, response));
    await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
    const address = server.address();
    if (address == null || typeof address === "string") throw new Error("proxy has no port");
    proxy = new BarrierProxy(server, address.port, target);
    return proxy;
  }

  get origin(): string {
    return `http://127.0.0.1:${this.port}`;
  }

  async close(): Promise<void> {
    for (const tap of this.streams) tap.break();
    this.server.closeAllConnections();
    await new Promise<void>((resolve) => this.server.close(() => resolve()));
  }

  /** Holds the next matching request before it reaches the server. */
  holdRequest(matches: Matcher): Barrier {
    const barrier = new Barrier();
    this.rules.push({ matches, remaining: 1, beforeForward: (exchange) => barrier.wait(exchange) });
    return barrier;
  }

  /** Lets the next matching request reach the server and holds its response. */
  holdResponse(matches: Matcher): Barrier {
    const barrier = new Barrier();
    this.rules.push({ matches, remaining: 1, beforeRespond: (exchange) => barrier.wait(exchange) });
    return barrier;
  }

  /** Lets the next matching request reach the server and drops its response. */
  loseResponse(matches: Matcher): void {
    this.rules.push({ matches, remaining: 1, lose: true });
  }

  /** Every request the browser sent whose path starts with `/api/v1`. */
  v1(): Exchange[] {
    return this.exchanges.filter((exchange) => exchange.path.startsWith("/api/v1"));
  }

  /**
   * Every operation write the browser sent to the proxy, optionally of one scope
   * and kind. It is recorded on arrival, before any `holdRequest` barrier; its
   * `exchange.status` shows whether and how the server answered.
   */
  ops(scope?: string, kind?: string): OpPost[] {
    return this.exchanges.flatMap((exchange) => {
      const scopeName = opScope(exchange);
      if (scopeName == null || (scope != null && scopeName !== scope)) return [];
      const request = JSON.parse(exchange.body) as OpRequest;
      return kind == null || request.kind === kind ? [{ scope: scopeName, request, exchange }] : [];
    });
  }

  /** Resource actions the browser sent to the proxy (recorded on arrival), by path suffix. */
  actions(scope: string, action: string): Exchange[] {
    const prefix = `/api/v2/scopes/${encodeURIComponent(scope)}/resources/`;
    return this.exchanges.filter((exchange) => exchange.method === "POST" &&
      exchange.path.startsWith(prefix) && exchange.path.endsWith(`/${action}`));
  }

  /** One-off GETs of scope documents. */
  scopeReads(): Exchange[] {
    return this.exchanges.filter((exchange) => exchange.method === "GET" && exchange.path.startsWith("/api/v2/scopes/"));
  }

  /** Resolves with the first stream (already open or later) that matches. */
  stream(matches: (tap: StreamTap) => boolean = () => true): Promise<StreamTap> {
    const open = this.streams.find(matches);
    if (open != null) return Promise.resolve(open);
    return new Promise((resolve) => this.streamWaiters.push({ matches, resolve }));
  }

  /** Resolves with the next stream opened after this call that matches. */
  nextStream(matches: (tap: StreamTap) => boolean = () => true): Promise<StreamTap> {
    return new Promise((resolve) => this.streamWaiters.push({ matches, resolve }));
  }

  /** The last stream the browser opened. */
  latestStream(): StreamTap | undefined {
    return this.streams.at(-1);
  }

  private takeRule(exchange: Exchange): Rule | undefined {
    const rule = this.rules.find((candidate) => candidate.remaining > 0 && candidate.matches(exchange));
    if (rule != null) rule.remaining -= 1;
    return rule;
  }

  private async handle(request: IncomingMessage, response: ServerResponse): Promise<void> {
    const chunks: Buffer[] = [];
    for await (const chunk of request) chunks.push(chunk as Buffer);
    const url = new URL(request.url ?? "/", this.origin);
    const exchange: Exchange = {
      method: request.method ?? "GET",
      path: url.pathname,
      url,
      body: Buffer.concat(chunks).toString("utf8"),
    };
    this.exchanges.push(exchange);
    const rule = this.takeRule(exchange);
    await rule?.beforeForward?.(exchange);
    // The server checks that Origin matches Host, so the page's own Host goes
    // through unchanged; only the connection target differs. Identity encoding
    // keeps every body byte-exact and lets the tap read SSE frames as text.
    const headers = { ...request.headers, "accept-encoding": "identity" };
    delete headers.connection;
    const upstream = httpRequest({
      host: "127.0.0.1",
      port: this.target,
      method: exchange.method,
      path: request.url,
      headers,
    });
    upstream.on("error", () => response.socket?.destroy());
    upstream.end(chunks.length === 0 ? undefined : Buffer.concat(chunks));
    const reply = await new Promise<IncomingMessage>((resolve) => upstream.on("response", resolve));
    exchange.status = reply.statusCode;
    const replyHeaders = { ...reply.headers, connection: "close" };
    const eventStream = String(reply.headers["content-type"] ?? "").startsWith("text/event-stream");
    if (eventStream && url.pathname === "/api/v2/stream") {
      response.writeHead(reply.statusCode ?? 200, replyHeaders);
      response.flushHeaders();
      const tap = new StreamTap(url, response, () => upstream.destroy());
      this.streams.push(tap);
      for (const waiter of [...this.streamWaiters]) {
        if (!waiter.matches(tap)) continue;
        this.streamWaiters.splice(this.streamWaiters.indexOf(waiter), 1);
        waiter.resolve(tap);
      }
      let buffer = "";
      reply.setEncoding("utf8");
      reply.on("data", (chunk: string) => {
        buffer += chunk;
        let end = buffer.indexOf("\n\n");
        while (end >= 0) {
          tap.frame(buffer.slice(0, end));
          buffer = buffer.slice(end + 2);
          end = buffer.indexOf("\n\n");
        }
      });
      reply.on("end", () => response.end());
      return;
    }
    if (eventStream) {
      // Other event streams (the v1 UI's) pass through untouched.
      response.writeHead(reply.statusCode ?? 200, replyHeaders);
      response.flushHeaders();
      response.on("close", () => upstream.destroy());
      reply.pipe(response);
      return;
    }
    const body: Buffer[] = [];
    for await (const chunk of reply) body.push(chunk as Buffer);
    const bytes = Buffer.concat(body);
    exchange.responseBody = bytes.toString("utf8");
    await rule?.beforeRespond?.(exchange);
    if (rule?.lose) {
      exchange.lost = true;
      response.socket?.destroy();
      return;
    }
    delete replyHeaders["transfer-encoding"];
    response.writeHead(reply.statusCode ?? 502, { ...replyHeaders, "content-length": bytes.length });
    response.end(bytes);
  }
}

export function opScope(exchange: Exchange): string | undefined {
  const match = /^\/api\/v2\/scopes\/(.+)\/ops$/u.exec(exchange.path);
  return exchange.method === "POST" && match != null ? decodeURIComponent(match[1]!) : undefined;
}

/** Matches an operation write of `scope` (and `kind`, when given). */
export function isOp(scope: string, kind?: string): Matcher {
  return (exchange) => opScope(exchange) === scope &&
    (kind == null || (JSON.parse(exchange.body) as OpRequest).kind === kind);
}

/** Matches a resource action on `scope`. */
export function isAction(scope: string, action: string): Matcher {
  const prefix = `/api/v2/scopes/${encodeURIComponent(scope)}/resources/`;
  return (exchange) => exchange.method === "POST" && exchange.path.startsWith(prefix) &&
    exchange.path.endsWith(`/${action}`);
}

export interface AgentCall {
  method: string;
  sessionId?: string;
  text?: string;
  [key: string]: unknown;
}

export class Gates {
  private commands = 0;

  private constructor(readonly dir: string) {}

  static async create(): Promise<Gates> {
    return new Gates(await mkdtemp(join(tmpdir(), "attyd-ui-gates-")));
  }

  async dispose(): Promise<void> {
    await rm(this.dir, { recursive: true, force: true });
  }

  /** Makes an optional Agent barrier active. */
  async arm(name: string): Promise<void> {
    await writeFile(join(this.dir, `${name}.arm`), "");
  }

  async reached(name: string): Promise<void> {
    await expect.poll(() => existsSync(join(this.dir, `${name}.reached`)), { timeout: 15_000 }).toBe(true);
  }

  isReached(name: string): boolean {
    return existsSync(join(this.dir, `${name}.reached`));
  }

  async open(name: string): Promise<void> {
    await writeFile(join(this.dir, `${name}.open`), "");
  }

  /** Lets a terminal started by `terminal <name>` print its last line. */
  async finishTerminal(name: string): Promise<void> {
    await writeFile(join(this.dir, `${name}.finish`), "");
  }

  /** Runs an Agent command and waits until the Agent finished it. */
  async run(command: Record<string, unknown>): Promise<unknown> {
    const done = this.send(command);
    return done();
  }

  /** Starts an Agent command; the returned function waits for its result. */
  send(command: Record<string, unknown>): () => Promise<unknown> {
    const index = ++this.commands;
    const path = join(this.dir, `cmd-${index}.json`);
    const written = writeFile(`${path}.tmp`, JSON.stringify(command))
      .then(() => rename(`${path}.tmp`, path));
    return async () => {
      await written;
      const done = join(this.dir, `cmd-${index}.done`);
      await expect.poll(() => existsSync(done), { timeout: 15_000 }).toBe(true);
      const outcome = JSON.parse(await readFile(done, "utf8")) as { result?: unknown; error?: string };
      if (outcome.error != null) throw new Error(outcome.error);
      return outcome.result;
    };
  }

  async calls(method?: string): Promise<AgentCall[]> {
    const path = join(this.dir, "calls.jsonl");
    if (!existsSync(path)) return [];
    const calls = (await readFile(path, "utf8")).split("\n").filter(Boolean)
      .map((line) => JSON.parse(line) as AgentCall);
    return method == null ? calls : calls.filter((call) => call.method === method);
  }
}

export interface ProtocolFixture {
  server: RustTestServer;
  gates: Gates;
  /** A proxy per page: `proxies[0]` is the page fixture's `baseURL`. */
  proxy(): Promise<BarrierProxy>;
  close(): Promise<void>;
}

export async function startProtocolFixture(options: {
  args?: string[];
  env?: NodeJS.ProcessEnv;
} = {}): Promise<ProtocolFixture> {
  const gates = await Gates.create();
  const server = await startRustTestServer({
    cwd: process.cwd(),
    env: { ...process.env, ...options.env, ATTYD_UI_GATES: gates.dir },
    args: options.args,
    command: [process.execPath, "--import", "tsx", join(process.cwd(), "tests/fixtures/protocol-ui-agent.ts")],
  });
  const proxies: BarrierProxy[] = [];
  return {
    server,
    gates,
    async proxy() {
      const proxy = await BarrierProxy.start(server.port);
      proxies.push(proxy);
      return proxy;
    },
    async close() {
      for (const proxy of proxies) await proxy.close();
      await server.close();
      await gates.dispose();
    },
  };
}

/** The scope document as the server holds it now (a read holds nothing). */
export async function readScope(port: number, scope: string): Promise<{ status: number; body: { live?: unknown; log?: LogEntry[] } }> {
  const response = await fetch(`http://127.0.0.1:${port}/api/v2/scopes/${encodeURIComponent(scope)}`);
  return { status: response.status, body: await response.json() as { live?: unknown; log?: LogEntry[] } };
}

/** Whether an event is `scope`'s commit or alignment carrying a log change. */
export function hasLog(scope: string): (event: Event) => boolean {
  return (event) => event.scope === scope && "log" in event;
}

/** The blocks of a log entry's prompt (its first user message) or of its Agent messages. */
export function entryBlocks(entry: LogEntry, part: "prompt" | "reply"): Block[] {
  const [first, ...rest] = entry.turn.items;
  const messages = part === "prompt"
    ? (first?.type === "message" && first.role === "user" ? [first] : [])
    : (first?.type === "message" && first.role === "user" ? rest : entry.turn.items)
      .flatMap((item) => item.type === "message" && item.role === "agent" ? [item] : []);
  return messages.flatMap(({ chunks }) => chunks.map(({ content }) => content));
}

/** The text of a log entry's prompt or Agent messages. */
export function entryText(entry: LogEntry, part: "prompt" | "reply"): string {
  return entryBlocks(entry, part).map((block) => block.type === "text" ? block.text : `[${block.type}]`).join("");
}

/**
 * The document of `scope` as the browser received it on `tap`, starting from
 * the log it held when it subscribed. Events after one that does not apply are
 * not part of what the browser holds.
 */
export function received(tap: StreamTap, scope: string, held: readonly LogEntry[] = []): Replica {
  let replica = createReplica(scope, held);
  for (const event of tap.delivered) {
    if (event.scope !== scope) continue;
    const applied = applyEvent(replica, event);
    if (applied.kind !== "applied") break;
    replica = applied.replica;
  }
  return replica;
}

/** `<n>:<hash>`, the head of what the browser received, as `have` names it. */
export function headParam(replica: Replica): string {
  const head = headOf(replica);
  return `${head.n}:${head.hash}`;
}

export function sessionLive(replica: Replica): SessionLive {
  return replica.live as SessionLive;
}

export function globalLive(replica: Replica): GlobalLive {
  return replica.live as GlobalLive;
}
