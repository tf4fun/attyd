// Protocol v2 acceptance for the web UI (step 7, segment 2).
//
// Every test runs the real Rust server with the scripted Agent
// (`tests/fixtures/protocol-ui-agent.ts`) behind a `BarrierProxy` per page.
// Orderings are fixed by barriers on the Agent, on requests and on stream
// events, never by sleeping. Assertions read what the user sees and what
// actually crossed the wire: the operation writes and their bodies, the stream
// URLs and the events the browser received.
import { expect, test as baseTest, type Browser, type Locator, type Page } from "@playwright/test";
import type { Event, GlobalLive, LogEntry, OpRequest } from "../../shared/protocol";
import { projectPath, sessionPath } from "../../web/src/lib/session-route";
import {
  BarrierProxy,
  entryBlocks,
  entryText,
  globalLive,
  headParam,
  isAction,
  isOp,
  readScope,
  received,
  sessionLive,
  startProtocolFixture,
  type Exchange,
  type ProtocolFixture,
  type StreamTap,
} from "./protocol-harness";

const cwd = process.cwd();
const ALPHA = "session/alpha";
const BETA = "session/beta";

const test = baseTest.extend<{
  serverArgs: string[];
  serverEnv: NodeJS.ProcessEnv;
  fixture: ProtocolFixture;
  // Not `proxy`: Playwright already has a `proxy` option.
  pageProxy: BarrierProxy;
}>({
  serverArgs: [[], { option: true }],
  serverEnv: [{}, { option: true }],
  fixture: async ({ serverArgs, serverEnv }, use) => {
    const fixture = await startProtocolFixture({ args: serverArgs, env: serverEnv });
    try {
      await use(fixture);
    } finally {
      await fixture.close();
    }
  },
  pageProxy: async ({ fixture }, use) => use(await fixture.proxy()),
  baseURL: async ({ pageProxy }, use) => use(pageProxy.origin),
});

function composer(page: Page): Locator {
  return page.locator('textarea[role="combobox"]');
}

async function send(page: Page, text: string): Promise<void> {
  await composer(page).fill(text);
  await composer(page).press("Enter");
}

function follows(scope: string): (tap: StreamTap) => boolean {
  return (tap) => tap.scopes.includes(scope);
}

async function openAlpha(page: Page, proxy: BarrierProxy): Promise<StreamTap> {
  const stream = proxy.stream(follows(ALPHA));
  await page.goto(sessionPath("alpha", cwd));
  await expect(page.getByText("Alpha history answer.", { exact: true })).toBeVisible();
  await expect(composer(page)).toBeEnabled();
  return stream;
}

async function secondPage(browser: Browser, fixture: ProtocolFixture): Promise<{ page: Page; proxy: BarrierProxy }> {
  const proxy = await fixture.proxy();
  const page = await browser.newPage({ baseURL: proxy.origin });
  return { page, proxy };
}

async function openSessionPicker(page: Page): Promise<Locator> {
  const picker = page.locator("details.session-switcher");
  if (await picker.getAttribute("open") == null) {
    await page.getByRole("button", { name: "Switch project session", exact: true }).click();
  }
  await expect(picker.locator(".session-history")).toBeVisible();
  return picker;
}

function sessionRow(picker: Locator, title: string): Locator {
  return picker.locator(".session-row").filter({ hasText: title });
}

async function switchTo(page: Page, title: string): Promise<void> {
  const picker = await openSessionPicker(page);
  await sessionRow(picker, title).locator("button.session-open").click();
}

/** The page's response to its next operation write on `scope`, fully received. */
function opResponse(page: Page, scope: string): Promise<void> {
  const path = `/api/v2/scopes/${encodeURIComponent(scope)}/ops`;
  return page.waitForResponse((response) =>
    response.request().method() === "POST" && new URL(response.url()).pathname === path,
  ).then(async (response) => {
    await response.finished();
  });
}

/**
 * The exchange once the server's whole response has been read. Its status is
 * set when the headers arrive, so tests that parse the body wait for this.
 */
async function answered(exchange: () => Exchange | undefined): Promise<Exchange> {
  await expect.poll(() => exchange()?.responseBody !== undefined).toBe(true);
  return exchange()!;
}

/** What the page shows for a write whose outcome it cannot know (C3). */
function uncertainNotice(page: Page): Locator {
  return page.getByRole("alert").or(page.getByRole("status"))
    .filter({ hasText: /may have|unknown|not sure|uncertain/iu });
}

/** Writes to the server directly, as another client would. */
async function otherClient(port: number, scope: string, body: unknown): Promise<{ status: number; body: unknown }> {
  const response = await fetch(`http://127.0.0.1:${port}/api/v2/scopes/${encodeURIComponent(scope)}/ops`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(body),
  });
  return { status: response.status, body: await response.json() };
}

async function currentOp(port: number, scope: string): Promise<string> {
  return ((await readScope(port, scope)).body.live as { op: { id: string } }).op.id;
}

function permissionCard(page: Page, title: string): Locator {
  return page.getByRole("alertdialog", { name: "Agent permission request" }).filter({ hasText: title });
}

/** The request body of every operation write on `scope` of `kind`. */
function bodies(proxy: BarrierProxy, scope: string, kind: string): OpRequest[] {
  return proxy.ops(scope, kind).map((post) => post.request);
}

function promptText(request: OpRequest): string {
  if (request.kind !== "prompt") return "";
  return request.args.prompt.map((block) => block.type === "text" ? block.text : `[${block.type}]`).join("");
}

function opId(tap: StreamTap, scope: string): string {
  return sessionLive(received(tap, scope)).op.id;
}

/** Expands the last turn's folded process, once the turn has settled. */
async function expandSettledProcess(page: Page): Promise<void> {
  const turn = page.locator(".conversation-turn").last();
  await expect(turn.locator(".turn-stop")).toBeVisible();
  const expand = turn.getByRole("button", { name: "Expand execution process", exact: true });
  if (await expand.count()) await expand.click();
}

function logOf(tap: StreamTap, scope: string, held: readonly LogEntry[] = []): readonly LogEntry[] {
  return received(tap, scope, held).log;
}

// ---------------------------------------------------------------------------
// Harness self-checks. These drive the real server without the UI, so they pass
// before the UI moves to v2 and show that the proxy, the gates and the Agent
// work; the acceptance tests below fail only where the UI does not behave yet.
// ---------------------------------------------------------------------------

test.describe("protocol harness", () => {
  test("forwards a v2 stream, records operation writes and ends a cancelled turn as cancelled", async ({ fixture, pageProxy: proxy }) => {
    const { gates } = fixture;
    await gates.arm("load-alpha");
    const controller = new AbortController();
    const query = `scope=${encodeURIComponent(ALPHA)}&cwd=${encodeURIComponent(`${ALPHA}:${cwd}`)}`;
    const opened = proxy.stream(follows(ALPHA));
    const response = await fetch(`${proxy.origin}/api/v2/stream?${query}`, {
      signal: controller.signal,
      headers: { origin: proxy.origin },
    });
    expect(response.status).toBe(200);
    const reader = response.body!.getReader();
    void (async () => {
      try {
        for (;;) if ((await reader.read()).done) break;
      } catch {
        // Aborted at the end of the test.
      }
    })();
    const tap = await opened;
    expect(tap.cwd(ALPHA)).toBe(cwd);
    await gates.reached("load-alpha");
    await expect.poll(() => sessionLive(received(tap, ALPHA)).op?.kind).toBe("load");
    expect(received(tap, ALPHA).confirmed).toBe(false);
    await gates.open("load-alpha");
    await expect.poll(() => received(tap, ALPHA).confirmed).toBe(true);
    expect(logOf(tap, ALPHA).map((entry) => entryText(entry, "reply"))).toEqual(["Alpha history answer."]);

    const post = async (body: unknown) => fetch(`${proxy.origin}/api/v2/scopes/${encodeURIComponent(ALPHA)}/ops`, {
      method: "POST",
      headers: { "content-type": "application/json", origin: proxy.origin },
      body: JSON.stringify(body),
    });
    const prompt = await post({ expect: opId(tap, ALPHA), kind: "prompt", args: { prompt: [{ type: "text", text: "wait harness" }] } });
    expect(prompt.status).toBe(202);
    const { op } = await prompt.json() as { op: string };
    await gates.reached("harness");
    const cancel = await post({ expect: op, kind: "cancel", args: {} });
    expect(cancel.status).toBe(202);
    await gates.reached("cancel-alpha");
    // The cancel arrived; the turn still runs until its barrier lets it end.
    expect(sessionLive(received(tap, ALPHA)).op.status).toBe("running");
    await gates.open("harness");
    await expect.poll(() => sessionLive(received(tap, ALPHA)).op.status).toBe("completed");
    expect(sessionLive(received(tap, ALPHA)).op.result).toMatchObject({ stopReason: "cancelled" });
    expect(bodies(proxy, ALPHA, "prompt")).toHaveLength(1);
    expect(bodies(proxy, ALPHA, "cancel")).toEqual([{ expect: op, kind: "cancel", args: {} }]);
    expect((await gates.calls("session/prompt")).map((call) => call.text)).toEqual(["wait harness"]);

    // A lost response reaches the server once and never reaches the client.
    proxy.loseResponse(isOp(ALPHA, "prompt"));
    const lost = await post({ expect: op, kind: "prompt", args: { prompt: [{ type: "text", text: "echo lost" }] } })
      .then(() => "answered", () => "lost");
    expect(lost).toBe("lost");
    await expect.poll(async () => (await gates.calls("session/prompt")).map((call) => call.text))
      .toEqual(["wait harness", "echo lost"]);
    expect(proxy.ops(ALPHA, "prompt").at(-1)?.exchange).toMatchObject({ status: 202, lost: true });

    // Agent commands report their own failure.
    await expect(fixture.gates.run({ type: "unknown" })).rejects.toThrow("Unknown command");
    expect(await fixture.gates.run({ type: "notify", sessionId: "alpha", updates: [
      { sessionUpdate: "usage_update", used: 1, size: 10 },
    ] })).toEqual({});
    await expect.poll(() => sessionLive(received(tap, ALPHA)).controls.usage).toMatchObject({ used: 1, size: 10 });
    controller.abort();
  });

  test("serves the current UI unchanged through the proxy", async ({ page, pageProxy: proxy }) => {
    await page.goto(sessionPath("alpha", cwd));
    await expect(page.locator("#root")).not.toBeEmpty();
    const failures = proxy.exchanges.filter((exchange) => (exchange.status ?? 0) === 403);
    expect(failures).toEqual([]);
    const assets = proxy.exchanges.filter((exchange) => exchange.path.startsWith("/assets/"));
    expect(assets.length).toBeGreaterThan(0);
    expect(assets.every((exchange) => exchange.status === 200)).toBe(true);
  });
});

// ---------------------------------------------------------------------------
// 1. Replica, load and reconnection
// ---------------------------------------------------------------------------

test.describe("replica and reconnection", () => {
  test("opens a session by subscribing with its cwd and shows the load's history", async ({ page, fixture, pageProxy: proxy }) => {
    await fixture.gates.arm("load-alpha");
    const stream = proxy.stream(follows(ALPHA));
    await page.goto(sessionPath("alpha", cwd));
    const tap = await stream;
    expect(tap.scopes).toContain("global");
    expect(tap.cwd(ALPHA)).toBe(cwd);
    expect(tap.have(ALPHA)).toBeUndefined();
    await fixture.gates.reached("load-alpha");
    await expect(page.getByText("Alpha history answer.", { exact: true })).toHaveCount(0);
    await fixture.gates.open("load-alpha");
    await expect(page.getByText("Alpha history answer.", { exact: true })).toBeVisible();
    await expect(composer(page)).toBeEnabled();
    expect(proxy.v1()).toEqual([]);
    // Subscription loads the session; a read never does and is never needed.
    expect(proxy.scopeReads()).toEqual([]);
    expect(await fixture.gates.calls("session/load")).toHaveLength(1);
  });

  test("answers an interaction the load asks for while it runs", async ({ page, fixture, pageProxy: proxy }) => {
    await fixture.gates.arm("load-permission-alpha");
    const stream = proxy.stream(follows(ALPHA));
    await page.goto(sessionPath("alpha", cwd));
    const tap = await stream;
    const card = permissionCard(page, "Load check alpha");
    await expect(card).toBeVisible();
    const live = sessionLive(received(tap, ALPHA));
    expect(live.op.kind).toBe("load");
    const [interactionId] = Object.keys(live.op.interactions);
    await card.getByRole("button", { name: "Allow once", exact: true }).click();
    await expect(page.getByText("Alpha history answer.", { exact: true })).toBeVisible();
    expect(bodies(proxy, ALPHA, "answer")).toEqual([{
      expect: live.op.id,
      kind: "answer",
      args: { answers: { [interactionId!]: { outcome: { outcome: "selected", optionId: "allow" } } } },
    }]);
    expect(await fixture.gates.calls("load/permission")).toMatchObject([{ outcome: "allow" }]);
    expect(proxy.v1()).toEqual([]);
  });

  test.describe("cold reopen", () => {
    test.use({ serverArgs: ["--session-unobserved-timeout", "0"] });

    // The reload's replay is authoritative (protocol §3.3; acceptance 1, 27): a
    // replayed turn has no `end` and carries the Agent's message IDs, so it
    // differs from the live entry, and alignment replaces the held log whole.
    test("keeps the held log while a released session loads again and replaces it with the replay", async ({ page, fixture, pageProxy: proxy }) => {
      const first = await openAlpha(page, proxy);
      await send(page, "echo kept");
      await expect(page.getByText("Echo: echo kept", { exact: true })).toBeVisible();
      await expect.poll(() => logOf(first, ALPHA).length).toBe(2);
      const held = logOf(first, ALPHA);
      const head = headParam(received(first, ALPHA));

      const beta = proxy.nextStream(follows(BETA));
      await switchTo(page, "Beta session");
      await beta;
      await expect(page.getByText("Beta history answer.", { exact: true })).toBeVisible();
      // The bridge releases alpha once nothing needs it.
      await expect.poll(async () => Object.keys(((await readScope(fixture.server.port, "global")).body.live as GlobalLive).sessions))
        .not.toContain("alpha");

      await fixture.gates.arm("load-alpha");
      const reopened = proxy.nextStream(follows(ALPHA));
      await switchTo(page, "Alpha session");
      const tap = await reopened;
      expect(tap.have(ALPHA)).toBe(head);
      await fixture.gates.reached("load-alpha");
      // While the log is unknown to the bridge, the page keeps its own.
      await expect(page.getByText("Alpha history answer.", { exact: true })).toBeVisible();
      await expect(page.getByText("Echo: echo kept", { exact: true })).toBeVisible();
      await fixture.gates.open("load-alpha");
      await expect.poll(() => received(tap, ALPHA, held).confirmed).toBe(true);
      const confirming = tap.delivered.find((event): event is Extract<Event, { log: unknown }> =>
        event.scope === ALPHA && "log" in event);
      expect(held[1]?.turn.end).toMatchObject({ response: { stopReason: "end_turn" } });
      expect(confirming?.log).toMatchObject({ from: 0, prev: "AAAAAAAAAAAAAAAAAAAAAA" });
      const replayed = confirming!.log.entries;
      expect(replayed.map((entry) => entryText(entry, "reply"))).toEqual(held.map((entry) => entryText(entry, "reply")));
      expect(replayed[1]?.turn.end).toBeUndefined();
      expect(replayed[1]?.hash).not.toBe(held[1]?.hash);
      expect(logOf(tap, ALPHA, held)).toEqual(replayed);
      await expect(page.getByText("Echo: echo kept", { exact: true })).toHaveCount(1);
      await expect(composer(page)).toBeEnabled();
      expect(proxy.scopeReads()).toEqual([]);
    });
  });

  test("subscribes again with the head it holds after the stream breaks, without reading the scope", async ({ page, fixture, pageProxy: proxy }) => {
    const first = await openAlpha(page, proxy);
    const held = logOf(first, ALPHA);
    const head = headParam(received(first, ALPHA));
    const next = proxy.nextStream(follows(ALPHA));
    first.break();
    const tap = await next;
    expect(tap.have(ALPHA)).toBe(head);
    await expect.poll(() => received(tap, ALPHA, held).confirmed).toBe(true);
    expect(received(tap, ALPHA, held).log).toEqual(held);
    await send(page, "echo after break");
    await expect(page.getByText("Echo: echo after break", { exact: true })).toBeVisible();
    expect(bodies(proxy, ALPHA, "prompt").map((request) => request.expect)).toEqual([opId(first, ALPHA)]);
    expect(proxy.scopeReads()).toEqual([]);
    expect(await fixture.gates.calls("session/load")).toHaveLength(1);
  });

  for (const [name, raw] of [
    ["a log change past its log", JSON.stringify({ scope: ALPHA, log: { from: 9, prev: "AAAAAAAAAAAAAAAAAAAAAA", entries: [] } })],
    ["a patch that does not apply", JSON.stringify({ scope: ALPHA, live: { patch: [{ op: "remove", path: "/missing" }] } })],
    ["an event that is not JSON", "{"],
  ] as const) {
    test(`recovers from ${name} by closing the stream and subscribing with the current head`, async ({ page, pageProxy: proxy }) => {
      const first = await openAlpha(page, proxy);
      const head = headParam(received(first, ALPHA));
      const next = proxy.nextStream(follows(ALPHA));
      first.inject(raw);
      const tap = await next;
      await first.closed;
      expect(tap.have(ALPHA)).toBe(head);
      await expect(page.getByText("Alpha history answer.", { exact: true })).toHaveCount(1);
      await expect(composer(page)).toBeEnabled();
      expect(proxy.scopeReads()).toEqual([]);
    });
  }

  test("follows only the session it switches to and ignores the stream it replaced", async ({ page, pageProxy: proxy }) => {
    const alpha = await openAlpha(page, proxy);
    const next = proxy.nextStream(follows(BETA));
    await switchTo(page, "Beta session");
    const beta = await next;
    expect(beta.scopes).toEqual(expect.arrayContaining(["global", BETA]));
    expect(beta.scopes).not.toContain(ALPHA);
    await alpha.closed;
    await expect(page.getByText("Beta history answer.", { exact: true })).toBeVisible();
    await expect(page.getByText("Alpha history answer.", { exact: true })).toHaveCount(0);
    expect(proxy.v1()).toEqual([]);
  });
});

// ---------------------------------------------------------------------------
// 2. Operations, answers and uncertain results
// ---------------------------------------------------------------------------

test.describe("operations and answers", () => {
  test("lets one of two racing pages start the prompt and resubmits the refused input after the operation ends", async ({ browser, page, fixture, pageProxy: proxy }) => {
    const first = await openAlpha(page, proxy);
    const other = await secondPage(browser, fixture);
    try {
      await openAlpha(other.page, other.proxy);
      const expected = opId(first, ALPHA);
      const heldA = proxy.holdRequest(isOp(ALPHA, "prompt"));
      const heldB = other.proxy.holdRequest(isOp(ALPHA, "prompt"));
      await send(page, "wait race-a");
      await send(other.page, "echo race-b");
      await Promise.all([heldA.reached, heldB.reached]);
      heldA.open();
      expect((await answered(() => heldA.exchange)).status).toBe(202);
      heldB.open();
      expect((await answered(() => heldB.exchange)).status).toBe(409);
      expect(JSON.parse(heldB.exchange!.responseBody!)).toMatchObject({ error: { code: "stale" } });
      expect(bodies(proxy, ALPHA, "prompt").map((request) => request.expect)).toEqual([expected]);
      expect(bodies(other.proxy, ALPHA, "prompt").map((request) => request.expect)).toEqual([expected]);
      // A refused write leaves its input with the user.
      const queue = other.page.getByRole("region", { name: "Queued messages" });
      await expect(queue).toContainText("echo race-b");
      await fixture.gates.reached("race-a");
      const raceOp = (JSON.parse(heldA.exchange!.responseBody!) as { op: string }).op;
      expect(bodies(other.proxy, ALPHA, "prompt")).toHaveLength(1);

      await fixture.gates.open("race-a");
      for (const viewer of [page, other.page]) {
        await expect(viewer.getByText("Working on race-a. Finished race-a.", { exact: true })).toBeVisible();
        await expect(viewer.getByText("Echo: echo race-b", { exact: true })).toBeVisible();
      }
      const retried = bodies(other.proxy, ALPHA, "prompt");
      expect(retried).toHaveLength(2);
      expect(retried[1]).toMatchObject({ expect: raceOp, kind: "prompt" });
      expect(promptText(retried[1]!)).toBe("echo race-b");
      expect((await fixture.gates.calls("session/prompt")).map((call) => call.text)).toEqual(["wait race-a", "echo race-b"]);
      await expect(queue).toBeHidden();
    } finally {
      await other.page.close();
    }
  });

  test("answers every pending interaction in one write", async ({ page, pageProxy: proxy }) => {
    const tap = await openAlpha(page, proxy);
    await send(page, "two-permissions");
    await expect(permissionCard(page, "Tool one")).toBeVisible();
    await expect(permissionCard(page, "Tool two")).toBeVisible();
    await expect.poll(() => Object.keys(sessionLive(received(tap, ALPHA)).op.interactions ?? {}).length).toBe(2);
    const prompt = sessionLive(received(tap, ALPHA)).op;
    const interactions = Object.values(prompt.interactions);
    const id = (title: string) => interactions.find((interaction) =>
      interaction.kind === "permission" && interaction.request.toolCall.title === title)!.interactionId;
    await permissionCard(page, "Tool one").getByRole("button", { name: "Allow once", exact: true }).click();
    // The 202 arrives while the stream is held: it settles the page's write
    // but does not remove the interactions; their removal comes from the stream.
    tap.pause();
    const accepted = opResponse(page, ALPHA);
    await permissionCard(page, "Tool two").getByRole("button", { name: "Reject", exact: true }).click();
    await accepted;
    expect(proxy.ops(ALPHA, "answer").map((post) => post.exchange.status)).toEqual([202]);
    expect(Object.keys(sessionLive(received(tap, ALPHA)).op.interactions)).toHaveLength(2);
    await expect(permissionCard(page, "Tool one")).toBeVisible();
    await expect(permissionCard(page, "Tool two")).toBeVisible();
    expect(proxy.ops(ALPHA, "answer")).toHaveLength(1);
    tap.resume();
    await expect(page.getByText("Answers: allow, reject.", { exact: true })).toBeVisible();
    await expect(permissionCard(page, "Tool one")).toBeHidden();
    await expect(permissionCard(page, "Tool two")).toBeHidden();
    const answers = bodies(proxy, ALPHA, "answer");
    expect(answers).toHaveLength(1);
    expect(answers[0]).toEqual({
      expect: prompt.id,
      kind: "answer",
      args: { answers: {
        [id("Tool one")]: { outcome: { outcome: "selected", optionId: "allow" } },
        [id("Tool two")]: { outcome: { outcome: "selected", optionId: "reject" } },
      } },
    });
    expect(proxy.ops(ALPHA, "answer").map((post) => post.exchange.status)).toEqual([202]);
  });

  test("keeps a filled form when its answer is refused because another interaction joined", async ({ page, fixture, pageProxy: proxy }) => {
    const tap = await openAlpha(page, proxy);
    await send(page, "form-join");
    const form = page.getByRole("dialog", { name: "Agent input request" });
    await expect(form).toBeVisible();
    await form.getByLabel(/Note/).fill("keep this note");
    const held = proxy.holdRequest(isOp(ALPHA, "answer"));
    await form.getByRole("button", { name: "Submit", exact: true }).click();
    await held.reached;
    await fixture.gates.open("join");
    await expect.poll(() => Object.keys(sessionLive(received(tap, ALPHA)).op.interactions ?? {}).length).toBe(2);
    held.open();
    expect((await answered(() => held.exchange)).status).toBe(409);
    expect(JSON.parse(held.exchange!.responseBody!)).toMatchObject({ error: { code: "not_allowed" } });
    await expect(form).toBeVisible();
    await expect(form.getByLabel(/Note/)).toHaveValue("keep this note");
    await expect(permissionCard(page, "Joined check")).toBeVisible();

    await permissionCard(page, "Joined check").getByRole("button", { name: "Allow once", exact: true }).click();
    await form.getByRole("button", { name: "Submit", exact: true }).click();
    await expect(page.getByText("Note: keep this note; joined: allow.", { exact: true })).toBeVisible();
    const answers = bodies(proxy, ALPHA, "answer");
    expect(answers).toHaveLength(2);
    const complete = answers[1];
    expect(complete?.kind === "answer" ? Object.keys(complete.args.answers).length : 0).toBe(2);
  });

  test("keeps a surviving form when the pending set shrinks and the earlier full answer is refused", async ({ page, fixture, pageProxy: proxy }) => {
    const tap = await openAlpha(page, proxy);
    await send(page, "form-shrink");
    const form = page.getByRole("dialog", { name: "Agent input request" });
    const check = permissionCard(page, "Shrinking check");
    await expect(form).toBeVisible();
    await expect(check).toBeVisible();
    await expect.poll(() => Object.keys(sessionLive(received(tap, ALPHA)).op.interactions ?? {}).length).toBe(2);
    await form.getByLabel(/Note/).fill("survives");
    const held = proxy.holdRequest(isOp(ALPHA, "answer"));
    await check.getByRole("button", { name: "Allow once", exact: true }).click();
    await form.getByRole("button", { name: "Submit", exact: true }).click();
    await held.reached;
    const stale = held.exchange!;
    expect(Object.keys((JSON.parse(stale.body) as { args: { answers: object } }).args.answers)).toHaveLength(2);
    await fixture.gates.open("shrink");
    await expect.poll(() => Object.keys(sessionLive(received(tap, ALPHA)).op.interactions ?? {}).length).toBe(1);
    const [formId] = Object.keys(sessionLive(received(tap, ALPHA)).op.interactions);
    held.open();
    expect((await answered(() => stale)).status).toBe(409);
    expect(JSON.parse(stale.responseBody!)).toMatchObject({ error: { code: "not_allowed" } });
    await expect(check).toBeHidden();
    await expect(form).toBeVisible();
    await expect(form.getByLabel(/Note/)).toHaveValue("survives");
    await form.getByRole("button", { name: "Submit", exact: true }).click();
    await expect(page.getByText("Note: survives; check: withdrawn.", { exact: true })).toBeVisible();
    const answers = bodies(proxy, ALPHA, "answer");
    expect(answers).toHaveLength(2);
    const last = answers[1];
    expect(last?.kind === "answer" ? Object.keys(last.args.answers) : []).toEqual([formId]);
  });

  test("settles a prompt whose stream events arrive before its HTTP response", async ({ page, pageProxy: proxy }) => {
    await openAlpha(page, proxy);
    const held = proxy.holdResponse(isOp(ALPHA, "prompt"));
    await send(page, "echo early stream");
    await held.reached;
    await expect(page.getByText("Echo: echo early stream", { exact: true })).toBeVisible();
    held.open();
    await send(page, "echo next");
    await expect(page.getByText("Echo: echo next", { exact: true })).toBeVisible();
    await expect(page.locator(".message-user").filter({ hasText: "echo early stream" })).toHaveCount(1);
    expect(bodies(proxy, ALPHA, "prompt").map(promptText)).toEqual(["echo early stream", "echo next"]);
    await expect(page.getByRole("alert")).toHaveCount(0);
  });

  test("settles a prompt whose HTTP response arrives before its stream events", async ({ page, fixture, pageProxy: proxy }) => {
    const tap = await openAlpha(page, proxy);
    tap.pause();
    const answered = opResponse(page, ALPHA);
    await send(page, "echo late stream");
    // The page has the whole 202 before any event of the write reaches it.
    await answered;
    expect(proxy.ops(ALPHA, "prompt")[0]?.exchange.status).toBe(202);
    await expect.poll(async () => (await fixture.gates.calls("session/prompt")).length).toBe(1);
    await expect.poll(() => tap.queued).toBeGreaterThan(0);
    await expect(page.getByText("Echo: echo late stream", { exact: true })).toHaveCount(0);
    tap.resume();
    await expect(page.getByText("Echo: echo late stream", { exact: true })).toBeVisible();
    await send(page, "echo then");
    await expect(page.getByText("Echo: echo then", { exact: true })).toBeVisible();
    expect(bodies(proxy, ALPHA, "prompt").map(promptText)).toEqual(["echo late stream", "echo then"]);
    await expect(page.getByRole("alert")).toHaveCount(0);
  });

  test("navigates to a created session from its own create result after a successor operation replaced it", async ({ page, fixture, pageProxy: proxy }) => {
    const opened = proxy.stream(follows("global"));
    await page.goto(projectPath(cwd));
    const global = await opened;
    const project = page.url();
    const held = proxy.holdResponse(isOp("global", "create"));
    await page.getByRole("button", { name: "New session in project", exact: true }).click();
    await page.getByRole("dialog", { name: "New thread" }).getByRole("button", { name: "Create thread" }).click();
    await held.reached;
    const created = (JSON.parse(held.exchange!.responseBody!) as { result: { sessionId: string } }).result.sessionId;
    // The stream already shows the session; the page waits for its own result.
    await expect.poll(() => Object.keys(globalLive(received(global, "global")).sessions ?? {})).toContain(created);
    expect(page.url()).toBe(project);
    // Another client's create replaces the operation the page wrote.
    const successor = await otherClient(fixture.server.port, "global", {
      expect: await currentOp(fixture.server.port, "global"), kind: "create", args: { cwd },
    });
    expect(successor.status).toBe(200);
    const other = (successor.body as { result: { sessionId: string } }).result.sessionId;
    await expect.poll(() => globalLive(received(global, "global")).op.kind === "create" &&
      Object.keys(globalLive(received(global, "global")).sessions).includes(other)).toBe(true);
    expect(page.url()).toBe(project);
    held.open();
    await expect(page).toHaveURL(new RegExp(`/sessions/${created}$`, "u"));
    await expect(composer(page)).toBeEnabled();
    expect(bodies(proxy, "global", "create")).toEqual([{ expect: expect.any(String), kind: "create", args: { cwd } }]);
    await expect.poll(() => proxy.streams.some((tap) => tap.scopes.includes(`session/${created}`))).toBe(true);
    expect(proxy.streams.some((tap) => tap.scopes.includes(`session/${other}`))).toBe(false);
    // A created session is held from creation: subscribing does not load it.
    expect(await fixture.gates.calls("session/load")).toEqual([]);
    expect(proxy.v1()).toEqual([]);
  });

  test("navigates to a fork from its own fork result after a successor operation on the source", async ({ page, fixture, pageProxy: proxy }) => {
    const tap = await openAlpha(page, proxy);
    const source = page.url();
    const held = proxy.holdResponse(isOp(ALPHA, "fork"));
    await page.getByRole("button", { name: "Thread actions" }).click();
    await page.getByRole("button", { name: "Fork thread" }).click();
    await held.reached;
    const forked = (JSON.parse(held.exchange!.responseBody!) as { result: { sessionId: string } }).result.sessionId;
    expect(page.url()).toBe(source);
    const successor = await otherClient(fixture.server.port, ALPHA, {
      expect: await currentOp(fixture.server.port, ALPHA), kind: "prompt", args: { prompt: [{ type: "text", text: "echo successor" }] },
    });
    expect(successor.status).toBe(202);
    await expect(page.getByText("Echo: echo successor", { exact: true })).toBeVisible();
    expect(sessionLive(received(tap, ALPHA)).op.kind).toBe("prompt");
    expect(page.url()).toBe(source);
    held.open();
    await expect(page).toHaveURL(new RegExp(`/sessions/${forked}$`, "u"));
    await expect(composer(page)).toBeEnabled();
    expect(bodies(proxy, ALPHA, "fork")).toHaveLength(1);
    expect(proxy.v1()).toEqual([]);
  });

  test("does not resend a prompt whose response was lost", async ({ page, fixture, pageProxy: proxy }) => {
    const tap = await openAlpha(page, proxy);
    tap.pause();
    proxy.loseResponse(isOp(ALPHA, "prompt"));
    await send(page, "wait lost");
    await expect.poll(() => proxy.ops(ALPHA, "prompt")[0]?.exchange.lost).toBe(true);
    await fixture.gates.reached("lost");
    // The user is told the outcome is unknown; nothing is sent again.
    await expect(uncertainNotice(page)).toBeVisible();
    await fixture.gates.open("lost");
    tap.resume();
    await expect(page.getByText("Working on lost. Finished lost.", { exact: true })).toBeVisible();
    await expect(page.locator(".message-user").filter({ hasText: "wait lost" })).toHaveCount(1);
    expect(bodies(proxy, ALPHA, "prompt").map(promptText)).toEqual(["wait lost"]);
    expect((await fixture.gates.calls("session/prompt")).map((call) => call.text)).toEqual(["wait lost"]);
  });

  test("does not resend an answer whose response was lost", async ({ page, fixture, pageProxy: proxy }) => {
    const tap = await openAlpha(page, proxy);
    await send(page, "permission once");
    const card = permissionCard(page, "Permission once");
    await expect(card).toBeVisible();
    // Until the stream resumes, the interaction stays pending on the page: an
    // answer it resent now would be a second write.
    tap.pause();
    proxy.loseResponse(isOp(ALPHA, "answer"));
    await card.getByRole("button", { name: "Allow once", exact: true }).click();
    await expect.poll(() => proxy.ops(ALPHA, "answer")[0]?.exchange.lost).toBe(true);
    await expect(uncertainNotice(page)).toBeVisible();
    expect(proxy.ops(ALPHA, "answer")).toHaveLength(1);
    tap.resume();
    await expect(page.getByText("Permission once: allow.", { exact: true })).toBeVisible();
    await expect(card).toBeHidden();
    await send(page, "echo afterwards");
    await expect(page.getByText("Echo: echo afterwards", { exact: true })).toBeVisible();
    expect(proxy.ops(ALPHA, "answer").map((post) => post.exchange.lost ?? false)).toEqual([true]);
    expect(bodies(proxy, ALPHA, "prompt").map(promptText)).toEqual(["permission once", "echo afterwards"]);
    expect(await fixture.gates.calls("session/prompt")).toHaveLength(2);
  });

  test("shows a failed prompt's error without a history entry and retries the exact prompt on request", async ({ page, pageProxy: proxy }) => {
    const tap = await openAlpha(page, proxy);
    await send(page, "fail now");
    await expect(page.getByText("Synthetic prompt failure", { exact: true })).toBeVisible();
    await expect.poll(() => sessionLive(received(tap, ALPHA)).op.status).toBe("failed");
    expect(logOf(tap, ALPHA)).toHaveLength(1);
    expect(bodies(proxy, ALPHA, "prompt").map(promptText)).toEqual(["fail now"]);
    await page.getByRole("button", { name: /Retry/u }).click();
    await expect.poll(() => bodies(proxy, ALPHA, "prompt").map(promptText)).toEqual(["fail now", "fail now"]);
    expect(bodies(proxy, ALPHA, "prompt")[1]!.expect).not.toBe(bodies(proxy, ALPHA, "prompt")[0]!.expect);
  });

  test("stops a running turn with a cancel write and keeps the queue paused until the turn ends", async ({ page, fixture, pageProxy: proxy }) => {
    const tap = await openAlpha(page, proxy);
    await send(page, "wait stop");
    await fixture.gates.reached("stop");
    await send(page, "echo queued");
    const queue = page.getByRole("region", { name: "Queued messages" });
    await expect(queue).toContainText("1 queued");
    await page.getByRole("button", { name: "Stop current turn", exact: true }).click();
    await fixture.gates.reached("cancel-alpha");
    const running = sessionLive(received(tap, ALPHA)).op;
    expect(bodies(proxy, ALPHA, "cancel")).toEqual([{ expect: running.id, kind: "cancel", args: {} }]);
    await expect(queue).toContainText("1 queued · paused");
    // A requested cancel does not end the operation.
    const cancelling = page.getByRole("button", { name: "Cancelling the current turn…", exact: true });
    await expect(cancelling).toBeVisible();
    await expect(cancelling).toBeDisabled();
    await fixture.gates.open("stop");
    await expect.poll(() => sessionLive(received(tap, ALPHA)).op.result).toMatchObject({ stopReason: "cancelled" });
    await expect(cancelling).toBeHidden();
    await expect(queue).toContainText("1 queued · paused");
    expect(bodies(proxy, ALPHA, "prompt").map(promptText)).toEqual(["wait stop"]);
  });

  test("sends a queued message now by cancelling and submitting it once the turn actually ends", async ({ page, fixture, pageProxy: proxy }) => {
    const tap = await openAlpha(page, proxy);
    await send(page, "wait now");
    await fixture.gates.reached("now");
    await send(page, "echo sent now");
    await page.getByRole("button", { name: "Send queued message 1 now", exact: true }).click();
    await fixture.gates.reached("cancel-alpha");
    const running = sessionLive(received(tap, ALPHA)).op.id;
    expect(bodies(proxy, ALPHA, "cancel").map((request) => request.expect)).toEqual([running]);
    expect(bodies(proxy, ALPHA, "prompt")).toHaveLength(1);
    await fixture.gates.open("now");
    await expect(page.getByText("Echo: echo sent now", { exact: true })).toBeVisible();
    const prompts = bodies(proxy, ALPHA, "prompt");
    expect(prompts.map(promptText)).toEqual(["wait now", "echo sent now"]);
    expect(prompts[1]!.expect).toBe(running);
  });
});

// ---------------------------------------------------------------------------
// 3. #16 and #17: everything updates from the same projection
// ---------------------------------------------------------------------------

test.describe("controls and summaries", () => {
  test("shows mode, usage, commands and session info the Agent changes without a refresh (#16)", async ({ page, fixture, pageProxy: proxy }) => {
    const tap = await openAlpha(page, proxy);
    const navigations: string[] = [];
    page.on("framenavigated", (frame) => { if (frame === page.mainFrame()) navigations.push(frame.url()); });
    expect(await fixture.gates.run({ type: "notify", sessionId: "alpha", updates: [
      { sessionUpdate: "current_mode_update", currentModeId: "code" },
      { sessionUpdate: "usage_update", used: 50_000, size: 100_000 },
      { sessionUpdate: "available_commands_update", availableCommands: [{ name: "deploy", description: "Deploy the app" }] },
      { sessionUpdate: "session_info_update", title: "Renamed alpha" },
    ] })).toEqual({});
    await expect.poll(() => sessionLive(received(tap, ALPHA)).controls.info.title).toBe("Renamed alpha");
    await expect(page.locator(".config-trigger").filter({ hasText: "Mode" })).toContainText("Code");
    await expect(page.getByRole("button", { name: /Context usage: 50%/u })).toBeVisible();
    await expect(page.getByRole("heading", { name: "Renamed alpha" })).toBeVisible();
    await composer(page).fill("/");
    await expect(page.getByRole("option", { name: /deploy/u })).toBeVisible();
    expect(navigations).toEqual([]);
    expect(proxy.v1()).toEqual([]);
    expect(proxy.scopeReads()).toEqual([]);
  });

  test("changes config with a set_config write and shows the Agent's own config updates (#16)", async ({ page, fixture, pageProxy: proxy }) => {
    const stream = proxy.stream(follows(BETA));
    await page.goto(sessionPath("beta", cwd));
    await expect(page.getByText("Beta history answer.", { exact: true })).toBeVisible();
    const tap = await stream;
    const model = page.locator(".config-trigger").filter({ hasText: "Model" });
    await expect(model).toContainText("Fast");
    expect(await fixture.gates.run({ type: "notify", sessionId: "beta", updates: [{
      sessionUpdate: "config_option_update",
      configOptions: [{ type: "select", id: "model", name: "Model", currentValue: "deep",
        options: [{ value: "fast", name: "Fast" }, { value: "deep", name: "Deep" }] }],
    }] })).toEqual({});
    await expect(model).toContainText("Deep");
    const expected = opId(tap, BETA);
    await model.click();
    await page.getByRole("option", { name: /Fast/u }).click();
    await expect(model).toContainText("Fast");
    expect(bodies(proxy, BETA, "set_config")).toEqual([{ expect: expected, kind: "set_config", args: { configId: "model", value: "fast" } }]);
    expect(await fixture.gates.calls("session/set_config_option")).toMatchObject([{ configId: "model", value: "fast" }]);
  });

  test("disables new control writes while a prompt runs but shows the Agent's mode change at once", async ({ page, fixture, pageProxy: proxy }) => {
    const tap = await openAlpha(page, proxy);
    const mode = page.locator(".config-trigger").filter({ hasText: "Mode" });
    await expect(mode).toBeEnabled();
    await send(page, "wait controls");
    await fixture.gates.reached("controls");
    await expect(mode).toBeDisabled();
    expect(await fixture.gates.run({ type: "notify", sessionId: "alpha", updates: [
      { sessionUpdate: "current_mode_update", currentModeId: "code" },
    ] })).toEqual({});
    await expect.poll(() => sessionLive(received(tap, ALPHA)).controls.modes?.currentModeId).toBe("code");
    await expect(mode).toContainText("Code");
    await expect(mode).toBeDisabled();
    await fixture.gates.open("controls");
    await expect(page.getByText("Working on controls. Finished controls.", { exact: true })).toBeVisible();
    await expect(mode).toBeEnabled();
    expect(proxy.ops(ALPHA, "set_mode")).toEqual([]);
  });

  test("removes an accepted URL flow when the Agent completes it or the user dismisses it", async ({ page, context, fixture, pageProxy: proxy }) => {
    context.on("page", (popup) => void popup.close());
    const tap = await openAlpha(page, proxy);
    for (const [flow, ending] of [["connect-one", "complete"], ["connect-two", "dismiss"]] as const) {
      await send(page, `url ${flow}`);
      const request = page.getByRole("dialog", { name: "Agent input request" }).filter({ hasText: `Connect ${flow}` });
      await expect(request).toBeVisible();
      await request.getByRole("button", { name: "Open external flow", exact: true }).click();
      await expect(page.getByText(`URL ${flow}: accept.`, { exact: true })).toBeVisible();
      const waiting = page.getByText("Waiting for external flow").first();
      await expect(waiting).toBeVisible();
      await expect.poll(() => Object.values(sessionLive(received(tap, ALPHA)).urlFlows).map((item) => item.elicitationId))
        .toEqual([flow]);
      if (ending === "complete") {
        expect(await fixture.gates.run({ type: "complete", elicitationId: flow })).toEqual({});
      } else {
        await page.getByRole("button", { name: "Dismiss external flow", exact: true }).click();
        await expect.poll(() => proxy.actions(ALPHA, "dismiss").map((exchange) => exchange.status)).toEqual([204]);
      }
      await expect.poll(() => Object.keys(sessionLive(received(tap, ALPHA)).urlFlows)).toEqual([]);
      await expect(page.getByText("Waiting for external flow")).toHaveCount(0);
    }
    expect(proxy.v1()).toEqual([]);
  });

  test("shows Input needed for a background session from the global summary without following it (#17)", async ({ browser, page, fixture, pageProxy: proxy }) => {
    await openAlpha(page, proxy);
    const other = await secondPage(browser, fixture);
    try {
      const betaStream = other.proxy.stream(follows(BETA));
      await other.page.goto(sessionPath("beta", cwd));
      await expect(other.page.getByText("Beta history answer.", { exact: true })).toBeVisible();
      await betaStream;
      const answered = fixture.gates.send({ type: "permission", sessionId: "beta", title: "Background check" });
      const global = proxy.latestStream()!;
      await expect.poll(() => globalLive(received(global, "global")).sessions?.beta?.pending.interactions).toBe(1);
      const picker = await openSessionPicker(page);
      const reminder = sessionRow(picker, "Beta session").getByLabel("Agent input required");
      await expect(reminder).toBeVisible();
      // The reminder comes from `global.sessions`: this page never follows beta.
      expect(proxy.streams.flatMap((tap) => tap.scopes)).not.toContain(BETA);

      await permissionCard(other.page, "Background check").getByRole("button", { name: "Allow once", exact: true }).click();
      expect(await answered()).toEqual({ outcome: "allow" });
      await expect.poll(() => globalLive(received(global, "global")).sessions?.beta?.pending.interactions).toBe(0);
      await expect(reminder).toHaveCount(0);
      expect(proxy.streams.flatMap((tap) => tap.scopes)).not.toContain(BETA);
      expect(proxy.v1()).toEqual([]);
    } finally {
      await other.page.close();
    }
  });
});

// ---------------------------------------------------------------------------
// 4. Resources and media
// ---------------------------------------------------------------------------

test.describe("resources and media", () => {
  test("keeps a session terminal after its operation ends and drops its output once released", async ({ page, fixture, pageProxy: proxy }) => {
    const tap = await openAlpha(page, proxy);
    await send(page, "terminal t1");
    await expect(page.getByText("Terminal t1 started.", { exact: true })).toBeVisible();
    await expect.poll(() => sessionLive(received(tap, ALPHA)).op.status).toBe("completed");
    // The operation ended; the terminal lives by its own lifecycle, in the
    // tool card its settled turn keeps folded (protocol S8).
    await expandSettledProcess(page);
    await expect(page.getByText(/TERM t1 ready/u)).toBeVisible();
    await fixture.gates.finishTerminal("t1");
    await expect(page.getByText(/TERM t1 done/u)).toBeVisible();
    await fixture.gates.open("release-t1");
    await expect.poll(() => Object.keys(sessionLive(received(tap, ALPHA)).terminals)).toEqual([]);
    await expect(page.getByText(/TERM t1 ready/u)).toHaveCount(0);
    expect(proxy.v1()).toEqual([]);
  });

  test("renders an attachment from the log through the v2 attachment read", async ({ page, pageProxy: proxy }) => {
    const tap = await openAlpha(page, proxy);
    await send(page, "image");
    await expect(page.getByText("Image reply.", { exact: true })).toBeVisible();
    await expect.poll(() => logOf(tap, ALPHA).length).toBe(2);
    const reply = entryBlocks(logOf(tap, ALPHA)[1]!, "reply");
    const attachment = reply.find((block) => block.type === "attachment");
    expect(attachment).toMatchObject({ type: "attachment", block: "image", mimeType: "image/png" });
    const attachmentId = attachment?.type === "attachment" ? attachment.id : "";
    // The image is an earlier Agent message of the settled turn: it is in the
    // turn's folded process, under the final answer.
    await expandSettledProcess(page);
    const image = page.locator(".message-agent img, .conversation-turn img").last();
    await expect(image).toBeVisible();
    await expect.poll(() => image.evaluate((element) => (element as HTMLImageElement).naturalWidth)).toBeGreaterThan(0);
    const reads = proxy.exchanges.filter((exchange) =>
      exchange.path === `/api/v2/sessions/alpha/attachments/${attachmentId}`);
    expect(reads.map((exchange) => exchange.status)).toEqual([200]);
    expect(proxy.v1()).toEqual([]);
  });

  test.describe("terminal login", () => {
    test.use({ serverEnv: { ATTYD_UI_REQUIRE_AUTH: "1" } });

    function currentLogins(proxy: BarrierProxy) {
      // Listing and then loading the unauthenticated route change the scopes
      // followed by the page. Read its current stream, not the initial global
      // stream that was closed when the page tried to load alpha.
      const tap = proxy.latestStream();
      if (tap == null || tap.isClosed || !tap.scopes.includes("global") ||
        !tap.delivered.some((event) => event.scope === "global" && "live" in event &&
          event.live != null && "replace" in event.live)) return [];
      return Object.values(globalLive(received(tap, "global")).terminals);
    }

    test("keeps each login terminal as its own resource with its methodId and sends its input serially", async ({ browser, page, fixture, pageProxy: proxy }) => {
      await page.goto(sessionPath("alpha", cwd));
      await expect(page.getByRole("heading", { name: "Sign in to continue" })).toBeVisible();
      const logins = () => currentLogins(proxy);
      const card = (viewer: Page, method: string) => viewer.locator(".auth-terminal-card")
        .filter({ has: viewer.getByRole("heading", { name: `Sign in with ${method}`, exact: true }) });

      await page.getByRole("button", { name: "Authenticate with Sign in with login-a" }).click();
      const inputA = card(page, "login-a").getByLabel("Agent terminal authentication input");
      await expect(inputA).toBeFocused();
      await inputA.pressSequentially("wrong");
      await inputA.press("Enter");
      await expect.poll(() => logins().map((terminal) => [terminal.methodId, terminal.exitStatus?.exitCode ?? null]))
        .toEqual([["login-a", 1]]);
      expect(logins()[0]!.output).toContain("Rejected login-a.");
      // An exited login stays until it is dismissed.
      await expect(card(page, "login-a")).toBeVisible();

      await page.getByRole("button", { name: "Authenticate with Sign in with login-b" }).click();
      await expect.poll(() => logins().map((terminal) => terminal.methodId).sort()).toEqual(["login-a", "login-b"]);
      const terminalB = logins().find((terminal) => terminal.methodId === "login-b")!;
      expect(terminalB.exitStatus).toBeNull();
      await expect(card(page, "login-a")).toBeVisible();
      await expect(card(page, "login-b")).toBeVisible();
      expect(bodies(proxy, "global", "terminal_login").map((request) =>
        request.kind === "terminal_login" ? request.args.methodId : "")).toEqual(["login-a", "login-b"]);

      // A page opened now gets both resources from its first cut.
      const fresh = await secondPage(browser, fixture);
      try {
        const freshGlobal = fresh.proxy.stream(follows("global"));
        await fresh.page.goto(sessionPath("alpha", cwd));
        const freshTap = await freshGlobal;
        const firstGlobalCut = (): GlobalLive | undefined => {
          const event = freshTap.delivered.find((event) => event.scope === "global");
          return event != null && "live" in event && event.live != null && "replace" in event.live
            ? event.live.replace as GlobalLive : undefined;
        };
        await expect.poll(firstGlobalCut).toBeDefined();
        expect(Object.values(firstGlobalCut()!.terminals)
          .map((terminal) => [terminal.methodId, terminal.exitStatus?.exitCode ?? null]).sort())
          .toEqual([["login-a", 1], ["login-b", null]]);
        await expect(card(fresh.page, "login-a")).toBeVisible();
        await expect(card(fresh.page, "login-b")).toBeVisible();
      } finally {
        await fresh.page.close();
      }

      const pathB = `/api/v2/scopes/global/resources/${encodeURIComponent(terminalB.terminalId)}/input`;
      const inputsB = () => proxy.actions("global", "input").filter((exchange) => exchange.path === pathB);
      const first = proxy.holdRequest((exchange) => exchange.method === "POST" && exchange.path === pathB);
      const inputB = card(page, "login-b").getByLabel("Agent terminal authentication input");
      await inputB.focus();
      await inputB.pressSequentially("c");
      await first.reached;
      await inputB.pressSequentially("ode-login-b");
      await inputB.press("Enter");
      // Input to one terminal waits for the request before it.
      expect(inputsB()).toHaveLength(1);
      first.open();
      await expect(page.getByText("Alpha history answer.", { exact: true })).toBeVisible();
      expect(inputsB().map((exchange) => (JSON.parse(exchange.body) as { data: string }).data).join("")).toBe("code-login-b\r");
      expect(inputsB().every((exchange) => exchange.status === 204)).toBe(true);
      expect(proxy.v1()).toEqual([]);
    });

    test("does not resend terminal input whose response was lost", async ({ page, pageProxy: proxy }) => {
      await page.goto(sessionPath("alpha", cwd));
      await page.getByRole("button", { name: "Authenticate with Sign in with login-a" }).click();
      const input = page.getByLabel("Agent terminal authentication input");
      await expect(input).toBeFocused();
      proxy.loseResponse(isAction("global", "input"));
      await input.pressSequentially("x");
      await expect.poll(() => proxy.actions("global", "input").map((exchange) => exchange.lost ?? false)).toEqual([true]);
      await expect.poll(() => currentLogins(proxy)[0]?.output ?? "").toContain("x");
      await input.pressSequentially("y");
      await expect.poll(() => proxy.actions("global", "input").map((exchange) => (JSON.parse(exchange.body) as { data: string }).data))
        .toEqual(["x", "y"]);
    });
  });
});
