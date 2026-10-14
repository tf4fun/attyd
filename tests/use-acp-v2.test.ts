// @vitest-environment happy-dom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { useAcp } from "../web/src/lib/use-acp";
import { FakeBridge, json, pointer } from "./v2-fake-bridge";

describe("useAcp on protocol v2", () => {
  let root: Root;
  let container: HTMLDivElement;
  let acp: ReturnType<typeof useAcp>;
  let bridge: FakeBridge;

  function Harness() {
    acp = useAcp();
    return null;
  }

  async function settle() {
    for (let index = 0; index < 8; index += 1) {
      await act(async () => { await new Promise((resolve) => setTimeout(resolve, 0)); });
    }
  }

  async function mount(path: string) {
    window.history.replaceState(null, "", path);
    await act(async () => root.render(createElement(Harness)));
    await settle();
  }

  beforeEach(() => {
    (globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    container = document.createElement("div");
    document.body.append(container);
    root = createRoot(container);
    bridge = new FakeBridge();
    bridge.catalog = [{ sessionId: "alpha", cwd: "/work/alpha", title: "Alpha" }];
    vi.stubGlobal("EventSource", class {
      static CLOSED = 2;
      constructor(url: string) { return bridge.eventSource(url); }
    });
    vi.stubGlobal("fetch", bridge.fetch);
  });

  afterEach(async () => {
    await act(async () => root.unmount());
    container.remove();
    window.history.replaceState(null, "", "/");
    vi.unstubAllGlobals();
  });

  async function openAlpha() {
    await mount("/projects/%2Fwork%2Falpha/sessions/alpha");
    expect(acp.state.session?.sessionId).toBe("alpha");
  }

  it("opens a routed session, then subscribes to the owner the open returned", async () => {
    await openAlpha();
    const [open] = bridge.commands("POST", "/api/v2/sessions/alpha/open");
    expect(open.body).toEqual({ cwd: "/work/alpha" });
    expect(open.headers["idempotency-key"]).toMatch(/.+/u);
    expect(open.headers["if-match"]).toBeUndefined();
    expect(bridge.stream.subscriptions).toEqual(["global", "session/alpha@e1:1"]);
    expect(acp.state.phase).toBe("ready");
    expect(acp.state.sessionView).toMatchObject({ bridgeEpoch: "e1", sessionId: "alpha", sessionIncarnation: 1 });
    expect(acp.sessionOpening).toBeUndefined();
  });

  it("starts a turn with its key and the history CAS, and streams its updates without refetching", async () => {
    await openAlpha();
    await act(async () => { acp.prompt([{ type: "text", text: "hello" }]); });
    await settle();
    const [turn] = bridge.commands("POST", "/api/v2/sessions/alpha/turns");
    expect(turn.headers["if-match"]).toBe('"e1:1:r1"');
    expect(turn.headers["idempotency-key"]).toMatch(/.+/u);
    expect(acp.state.running).toBe(true);
    const reads = bridge.commands("GET", "/api/v2/sessions/alpha").length;

    await act(async () => bridge.appendTurnUpdate("alpha", { sessionUpdate: "agent_message_chunk", content: { type: "text", text: "Hel" } }));
    await act(async () => bridge.commit("session/alpha", [{ op: "append", path: "/turn/updates/0/content/text", text: "lo" }]));
    expect(JSON.stringify(acp.state.timeline)).toContain("Hello");
    expect(bridge.commands("GET", "/api/v2/sessions/alpha")).toHaveLength(reads);
  });

  it("updates usage, title and Agent-initiated modes live (#16)", async () => {
    await openAlpha();
    await act(async () => bridge.commit("session/alpha", [
      { op: "set", path: "/controls/usage", value: { used: 3, size: 10, cost: null } },
      { op: "set", path: "/controls/info/title", value: "Renamed" },
      { op: "set", path: "/controls/commands", value: [{ name: "init", description: "Init" }] },
      { op: "set", path: "/controls/modes", value: { currentModeId: "code", availableModes: [{ id: "code", name: "Code" }] } },
    ]));
    expect(acp.state.usage).toMatchObject({ used: 3, size: 10 });
    expect(acp.state.title).toBe("Renamed");
    expect(acp.state.availableCommands).toEqual([{ name: "init", description: "Init" }]);
    expect(acp.state.modeId).toBe("code");
  });

  it("sets a mode with the owner tag and settles from the request record", async () => {
    await openAlpha();
    await act(async () => bridge.commit("session/alpha", [
      { op: "set", path: "/controls/modes", value: { currentModeId: "ask", availableModes: [{ id: "ask", name: "Ask" }, { id: "code", name: "Code" }] } },
    ]));
    await act(async () => acp.setMode("code"));
    await settle();
    const [mode] = bridge.commands("PUT", "/api/v2/sessions/alpha/mode");
    expect(mode.headers["if-match"]).toBe('"e1:1"');
    expect(acp.state.modeId).toBe("code");
    expect(acp.state.pendingSessionControl).toBeUndefined();
  });

  it("resends the same key after a lost response, and the turn runs once", async () => {
    await openAlpha();
    let calls = 0;
    bridge.overrides.push((request) => {
      if (!request.path.endsWith("/turns")) return undefined;
      calls += 1;
      if (calls === 1) {
        // The bridge accepts the turn, but its response never arrives.
        bridge.serve(request);
        throw new TypeError("network down");
      }
      const turn = bridge.sessions.get("alpha")!.turn!;
      return json({ at: { epoch: bridge.epoch, seq: bridge.seq }, scopes: ["session/alpha"],
        result: { operationId: turn.operationId, disposition: "duplicate" } });
    });
    await act(async () => { acp.prompt([{ type: "text", text: "once" }]); });
    await settle();
    const sent = bridge.requests.filter((request) => request.path.endsWith("/turns"));
    expect(sent).toHaveLength(2);
    expect(sent[0].headers["idempotency-key"]).toBe(sent[1].headers["idempotency-key"]);
    expect(acp.state.running).toBe(true);
    expect(acp.state.timeline.filter((item) => item.type === "error")).toEqual([]);
  });

  it("shows URL flow completion and cancellation live (#16)", async () => {
    await openAlpha();
    const flow = (status: string) => ({
      interactionId: "I1", elicitationId: "el", status,
      request: { sessionId: "alpha", mode: "url", message: "Sign in", url: "https://example.test", elicitationId: "el" },
    });
    await act(async () => bridge.commit("session/alpha", [{ op: "set", path: "/urlFlows/I1", value: flow("waiting") }]));
    expect(acp.state.externalFlows).toMatchObject([{ interactionId: "I1", status: "waiting" }]);
    await act(async () => bridge.commit("session/alpha", [{ op: "set", path: "/urlFlows/I1/status", value: "completed" }]));
    expect(acp.state.externalFlows).toMatchObject([{ interactionId: "I1", status: "completed" }]);
    await act(async () => acp.dismissExternalFlow("I1"));
    await settle();
    expect(bridge.commands("DELETE", "/api/v2/sessions/alpha/url-flows/I1")[0].headers["if-match"]).toBe('"e1:1"');
  });

  it("responds to a permission with the session owner and to a global elicitation with the epoch", async () => {
    await openAlpha();
    bridge.overrides.push((request) => request.path.startsWith("/api/v2/interactions/")
      ? bridge.accept({
        requestId: `answer-${request.path.split("/").pop()}`, operation: "interaction_response", method: "session/request_permission",
        clientIntentId: request.headers["idempotency-key"], target: null, interactionId: null, methodId: null,
        params: {}, status: "succeeded", result: { kind: "done" },
      })
      : undefined);
    await act(async () => bridge.commit("session/alpha", [{ op: "set", path: "/interactions/P1", value: {
      interactionId: "P1", kind: "permission", operationId: null,
      request: { sessionId: "alpha", toolCall: { toolCallId: "t" }, options: [{ optionId: "ok", name: "OK", kind: "allow_once" }] },
    } }]));
    expect(acp.state.permissions.map(({ permissionId }) => permissionId)).toEqual(["P1"]);
    await act(async () => bridge.commit("global", [{ op: "set", path: "/interactions/G1", value: {
      interactionId: "G1", kind: "elicitation", operationId: null, requestId: "R0",
      request: { mode: "form", message: "Token?", requestedSchema: { type: "object", properties: {} } },
    } }]));
    expect(acp.state.globalElicitations.map(({ elicitationId }) => elicitationId)).toEqual(["G1"]);

    await act(async () => acp.respondPermission("P1", { outcome: "selected", optionId: "ok" }));
    await act(async () => acp.respondElicitation("G1", { action: "cancel" }));
    await settle();
    expect(bridge.commands("POST", "/api/v2/interactions/P1")[0]).toMatchObject({
      headers: { "if-match": '"e1:1"' }, body: { outcome: { outcome: "selected", optionId: "ok" } },
    });
    expect(bridge.commands("POST", "/api/v2/interactions/G1")[0]).toMatchObject({
      headers: { "if-match": '"e1"' }, body: { response: { action: "cancel" } },
    });
  });

  it("projects held sessions for the sidebar, including background work and input (#17)", async () => {
    await mount("/");
    bridge.hold("beta", "/work/beta");
    await act(async () => bridge.commit("global", [
      { op: "set", path: `/sessions/${pointer("beta")}/work`, value: "awaiting_input" },
      { op: "set", path: `/sessions/${pointer("beta")}/pending/permissions`, value: 1 },
    ]));
    expect(acp.state.globalSessions.beta).toMatchObject({ work: "awaiting_input", pending: { permissions: 1 } });
  });

  it("creates a session from its request record and subscribes to the new owner", async () => {
    await mount("/projects/%2Fwork%2Fnew");
    await act(async () => { acp.newSession("/work/new"); });
    await settle();
    const [create] = bridge.commands("POST", "/api/v2/sessions");
    expect(create.body).toEqual({ cwd: "/work/new" });
    const created = acp.state.session?.sessionId;
    expect(created).toMatch(/^created-/u);
    expect(bridge.stream.subscriptions).toEqual(["global", `session/${created}@e1:1`]);
    expect(window.location.pathname).toBe(`/projects/%2Fwork%2Fnew/sessions/${created}`);
  });

  it("returns to the project when the session scope ends", async () => {
    await openAlpha();
    await act(async () => acp.closeSession());
    await settle();
    expect(bridge.commands("POST", "/api/v2/sessions/alpha/close")[0].headers["if-match"]).toBe('"e1:1"');
    expect(acp.state.session).toBeUndefined();
    expect(window.location.pathname).toBe("/projects/%2Fwork%2Falpha");
    expect(bridge.stream.subscriptions).toEqual(["global"]);
  });

  it("deletes a cold session with the no-incarnation tag", async () => {
    await mount("/projects/%2Fwork%2Falpha");
    bridge.overrides.push((request) => request.method === "DELETE" && request.path === "/api/v2/sessions/cold"
      ? bridge.accept({
        requestId: "D1", operation: "delete", method: "session/delete", clientIntentId: request.headers["idempotency-key"],
        target: { sessionId: "cold", incarnation: null }, interactionId: null, methodId: null, params: {},
        status: "succeeded", result: { kind: "done" },
      })
      : undefined);
    await act(async () => acp.deleteSession("cold"));
    await settle();
    expect(bridge.commands("DELETE", "/api/v2/sessions/cold")[0].headers["if-match"]).toBe('"e1:0"');
    expect(acp.state.pendingSessionDeletions).toEqual([]);
  });

  it("re-reads the catalog when its revision changes", async () => {
    await mount("/");
    const reads = bridge.commands("GET", "/api/v2/catalog").length;
    expect(acp.state.sessions.map(({ sessionId }) => sessionId)).toEqual(["alpha"]);
    bridge.catalog = [...bridge.catalog, { sessionId: "gamma", cwd: "/work/gamma" }];
    await act(async () => bridge.commit("global", [{ op: "set", path: "/catalog/revision", value: "2" }]));
    await settle();
    expect(bridge.commands("GET", "/api/v2/catalog").length).toBeGreaterThan(reads);
    expect(acp.state.sessions.map(({ sessionId }) => sessionId)).toContain("gamma");
  });

  async function runTurn(text: string) {
    await act(async () => { acp.prompt([{ type: "text", text }]); });
    await settle();
    await act(async () => bridge.appendTurnUpdate("alpha", { sessionUpdate: "tool_call", toolCallId: "read-1", title: "Read file", status: "completed" }));
    await act(async () => bridge.appendTurnUpdate("alpha", { sessionUpdate: "agent_message_chunk", content: { type: "text", text: `answer to ${text}` } }));
  }

  it("keeps a watched turn's process inline after it completes, without fetching its page", async () => {
    await openAlpha();
    await runTurn("watched");
    await act(async () => bridge.completeTurn("alpha", "r2"));
    expect(acp.state.running).toBe(false);
    expect(acp.state.timeline.some((item) => item.type === "tool" && item.call.toolCallId === "read-1")).toBe(true);
    expect(acp.state.timeline.find((item) => item.retainedProcess != null)?.retainedProcess)
      .toMatchObject({ historyRevision: "r2", processCount: 1 });
    expect(bridge.requests.filter(({ path }) => path.includes("/process"))).toEqual([]);
  });

  it("loads the process page of a turn it did not watch by (historyRevision, turnId)", async () => {
    await openAlpha();
    await runTurn("elsewhere");
    await act(async () => bridge.completeTurn("alpha", "r2"));
    // A fresh page load has not watched the turn.
    await act(async () => root.unmount());
    root = createRoot(container);
    await mount("/projects/%2Fwork%2Falpha/sessions/alpha");
    const deferred = acp.state.timeline.find((item) => item.deferredProcess != null)?.deferredProcess;
    expect(deferred).toMatchObject({ historyRevision: "r2", processCount: 1 });
    expect(acp.state.timeline.some((item) => item.type === "tool")).toBe(false);
    const page = await acp.loadTurnProcess(deferred!, 0, new AbortController().signal);
    expect(bridge.requests.at(-1)!.path).toBe(`/api/v2/sessions/alpha/history/r2/turns/${deferred!.turnId}/process?offset=0`);
    expect(page.items).toEqual([[{ sessionUpdate: "tool_call", toolCallId: "read-1", title: "Read file", status: "completed" }]]);
  });

  it("shows a failed turn once, with its prompt kept for a retry", async () => {
    await openAlpha();
    await act(async () => { acp.prompt([{ type: "text", text: "fail me" }]); });
    await settle();
    const turn = bridge.sessions.get("alpha")!.turn!;
    await act(async () => bridge.commit("session/alpha", [
      { op: "set", path: `/outcomes/${turn.operationId}`, value: {
        operationId: turn.operationId, clientIntentId: turn.clientIntentId, status: "failed",
        error: { code: "agent_error", message: "Agent failed", data: { code: -32603, message: "Internal" } },
        prompt: turn.prompt, afterUpdate: 0,
      } },
      { op: "set", path: "/turn", value: null },
      { op: "set", path: "/history/phase", value: "ready" },
    ]));
    await act(async () => bridge.commit("session/alpha", [{ op: "set", path: "/controls/info/title", value: "later" }]));
    const errors = acp.state.timeline.filter((item) => item.type === "error");
    expect(errors).toHaveLength(1);
    expect(errors[0]).toMatchObject({ message: "Internal", code: -32603, retryBlocks: [{ type: "text", text: "fail me" }] });
    expect(acp.state.running).toBe(false);
  });

  it("M07 coalesces a reset burst into one in-flight snapshot read", async () => {
    await openAlpha();
    let release!: () => void;
    const held = new Promise<void>((resolve) => { release = resolve; });
    bridge.overrides.push(async (request) => {
      if (request.method !== "GET" || request.path !== "/api/v2/sessions/alpha") return undefined;
      await held;
      return bridge.serve(request);
    });
    const reads = () => bridge.commands("GET", "/api/v2/sessions/alpha").length;
    const before = reads();
    await act(async () => {
      for (let burst = 0; burst < 32; burst += 1) {
        bridge.seq += 1;
        bridge.stream.deliver({ type: "reset", scope: "session/alpha", at: { epoch: "e1", seq: bridge.seq } }, `e1:${bridge.seq}`);
      }
    });
    expect(reads() - before).toBe(1);
    await act(async () => release());
    await settle();
    expect(acp.state.sessionView?.viewRevision).toBe(bridge.seq);
  });

  it("sends terminal sign-in keystrokes in order, one write at a time, to the bridge's request", async () => {
    bridge.global.agent.initialize!.authMethods = [{ id: "login", name: "Login", type: "terminal", args: [] } as never];
    await mount("/");
    await act(async () => acp.authenticate("login"));
    await settle();
    const [start] = bridge.commands("POST", "/api/v2/agent/auth-terminals");
    expect(start.body).toEqual({ methodId: "login", cols: 80, rows: 24 });
    const terminal = acp.state.authTerminal!;
    expect(terminal.status).toBe("running");
    let release!: () => void;
    const held = new Promise<void>((resolve) => { release = resolve; });
    let first = true;
    bridge.overrides.push(async (request) => {
      if (!request.path.endsWith("/input") || !first) return undefined;
      first = false;
      await held;
      return undefined;
    });
    await act(async () => {
      for (const data of ["a", "b", "\r"]) acp.writeAuthTerminal(terminal.requestId, data);
    });
    await settle();
    expect(bridge.commands("POST", "/api/v2/agent/auth-terminals/").filter(({ path }) => path.endsWith("/input"))).toHaveLength(1);
    await act(async () => release());
    await settle();
    const inputs = bridge.commands("POST", "/api/v2/agent/auth-terminals/").filter(({ path }) => path.endsWith("/input"));
    expect(inputs.map(({ body }) => (body as { data: string }).data)).toEqual(["a", "b", "\r"]);
    expect(new Set(inputs.map(({ path }) => path))).toEqual(new Set([`/api/v2/agent/auth-terminals/${Object.keys(bridge.global.requests)[0]}/input`]));
  });

  it("shows terminal sign-in output and its exit from the request record", async () => {
    bridge.global.agent.initialize!.authMethods = [{ id: "login", name: "Login", type: "terminal", args: [] } as never];
    await mount("/");
    await act(async () => acp.authenticate("login"));
    await settle();
    const requestId = Object.keys(bridge.global.requests)[0];
    await act(async () => bridge.commit("global", [{ op: "append", path: `/requests/${requestId}/terminal/output`, text: "Code: 1234" }]));
    expect(acp.state.authTerminal?.output).toBe("Code: 1234");
    await act(async () => bridge.finish(requestId, {
      status: "failed", terminal: { output: "Code: 1234", truncated: false, exitCode: 2, signal: null, message: "bad code" },
    }));
    expect(acp.state.authTerminal).toMatchObject({ status: "failed", exitCode: 2, message: "bad code" });
    expect(acp.state.pendingAuth).toBeUndefined();
  });

  it("reconnects by resyncing its stream only, after the Agent connection ended", async () => {
    await mount("/");
    await act(async () => bridge.commit("global", [{ op: "set", path: "/agent/phase", value: "stopped" }]));
    expect(acp.state.phase).toBe("stopped");
    const streams = bridge.streams.length;
    await act(async () => acp.reconnect());
    await settle();
    expect(bridge.streams.length).toBe(streams + 1);
    expect(bridge.requests.filter(({ path }) => path.startsWith("/api/v2/agent/"))).toEqual([]);
  });

  it("reopens the routed session in the next epoch after the Agent connection was replaced", async () => {
    await openAlpha();
    await act(async () => bridge.restart("e2"));
    await settle();
    await act(async () => acp.reconnect());
    await settle();
    expect(bridge.commands("POST", "/api/v2/sessions/alpha/open")).toHaveLength(2);
    expect(acp.state.phase).toBe("ready");
    expect(acp.state.session?.sessionId).toBe("alpha");
    expect(acp.state.sessionView).toMatchObject({ bridgeEpoch: "e2", sessionIncarnation: 1 });
    expect(bridge.stream.subscriptions).toEqual(["global", "session/alpha@e2:1"]);
  });
});

