// @vitest-environment happy-dom

// The real v2 workspace receives protocol events through EventSource. Only
// HTTP/SSE transport is faked; no replica or workspace internals are injected.
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { CatalogLive, Event, GlobalLive, Interaction, Operation, SessionLive } from "../shared/protocol";
import { H0 } from "../shared/replica";
import { ElicitationCard } from "../web/src/components/acp/elicitation";
import { PermissionCard } from "../web/src/components/acp/permission";
import { sessionPath } from "../web/src/lib/session-route";
import { useAcp } from "../web/src/lib/use-acp";

(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT: boolean })
  .IS_REACT_ACT_ENVIRONMENT = true;

const SESSION = "s";
const SCOPE = `session/${SESSION}`;
const CWD = "/work";
const selected = { outcome: "selected" as const, optionId: "allow" };

class Source {
  static instances: Source[] = [];
  onmessage: ((event: MessageEvent<string>) => void) | null = null;
  onerror: (() => void) | null = null;
  closed = false;

  constructor(readonly url: string) {
    Source.instances.push(this);
  }

  close() { this.closed = true; }

  emit(event: Event) {
    expect(this.closed).toBe(false);
    expect(this.onmessage).not.toBeNull();
    this.onmessage!(new MessageEvent("message", { data: JSON.stringify(event) }));
  }
}

interface Call {
  path: string;
  method: string | undefined;
  body: unknown;
  respond(status: number, body: unknown): void;
}

function operation(fields: Partial<Operation> = {}): Operation {
  return {
    id: "idle", kind: "none", status: "completed", input: null,
    cancelRequested: false, interactions: {}, ...fields,
  };
}

function permission(id: string): Interaction {
  return {
    kind: "permission", interactionId: id,
    request: {
      sessionId: SESSION,
      toolCall: { toolCallId: id, title: `Tool ${id}`, status: "pending" },
      options: [{ optionId: "allow", kind: "allow_once", name: "Allow once" }],
    },
  };
}

function global(sessionOp: Operation, globalOp = operation({ id: "global-idle" })): GlobalLive {
  return {
    op: globalOp, urlFlows: {}, terminals: {},
    agent: {
      phase: "ready", error: null, authenticatedMethodId: null,
      initialize: { protocolVersion: 1, agentCapabilities: { sessionCapabilities: { close: {}, delete: {} } } },
      launch: { transport: "stdio", command: [], cwd: CWD, readOnly: false, additionalDirectories: [], mcpServers: [] },
    },
    sessions: {
      [SESSION]: {
        sessionId: SESSION, cwd: CWD, title: "Session",
        op: { kind: sessionOp.kind, status: sessionOp.status },
        pending: { interactions: Object.keys(sessionOp.interactions).length, urlFlows: 0 },
      },
    },
  };
}

function session(op: Operation): SessionLive {
  return {
    sessionId: SESSION, cwd: CWD, op, urlFlows: {}, terminals: {},
    controls: {
      modes: null, configOptions: null, commands: [], usage: null,
      info: { title: "Session", updatedAt: null, meta: null },
    },
  };
}

const catalog: CatalogLive = {
  revision: "catalog-1", op: operation({ id: "catalog-idle" }), urlFlows: {},
};

function alignment(scope: string, live: unknown): Event {
  return { scope, log: { from: 0, prev: H0, entries: [] }, live: { replace: live } };
}

describe("review: workspace answers follow the pending interaction set", () => {
  let root: Root;
  let container: HTMLDivElement;
  let acp: ReturnType<typeof useAcp>;
  let calls: Call[];

  function Page() {
    acp = useAcp();
    return <>
      {acp.state.permissions.map((pending) => <PermissionCard
        key={pending.permissionId}
        pending={pending}
        onRespond={(outcome) => acp.respondPermission(pending.permissionId, outcome)}
      />)}
      {acp.state.elicitations.map((pending) => <ElicitationCard
        key={pending.elicitationId}
        pending={pending}
        onRespond={(response) => acp.respondElicitation(pending.elicitationId, response)}
      />)}
    </>;
  }

  beforeEach(() => {
    Source.instances = [];
    calls = [];
    vi.stubGlobal("EventSource", Source);
    vi.stubGlobal("fetch", vi.fn<typeof fetch>((input, init) => new Promise((resolve) => {
      calls.push({
        path: String(input), method: init?.method, body: JSON.parse(String(init?.body)),
        respond: (status, body) => resolve(new Response(JSON.stringify(body), { status })),
      });
    })));
    window.history.replaceState(null, "", sessionPath(SESSION, CWD));
    container = document.createElement("div");
    document.body.append(container);
    root = createRoot(container);
  });

  afterEach(async () => {
    await act(async () => root.unmount());
    container.remove();
    vi.unstubAllGlobals();
    window.history.replaceState(null, "", "/");
  });

  async function mount(sessionOp: Operation, globalOp?: Operation): Promise<Source> {
    await act(async () => root.render(<Page />));
    const initial = Source.instances.at(-1)!;
    const globalLive = global(sessionOp, globalOp);
    // Learning that the route's session is held adds it to the subscription.
    await act(async () => initial.emit(alignment("global", globalLive)));
    const source = Source.instances.at(-1)!;
    expect(new URL(source.url, window.location.href).searchParams.getAll("scope")).toContain(SCOPE);
    await act(async () => {
      source.emit(alignment("global", globalLive));
      source.emit(alignment("catalog", catalog));
      source.emit(alignment(SCOPE, session(sessionOp)));
    });
    expect(acp.state.session?.sessionId).toBe(SESSION);
    expect(calls).toEqual([]);
    return source;
  }

  it("sends a previously unsent answer once cancellation makes its draft cover every remaining interaction", async () => {
    const op = operation({ id: "ask-both", kind: "agent_request", status: "running", interactions: {
      A: permission("A"), B: permission("B"),
    } });
    const source = await mount(op);
    expect(container.querySelectorAll(".permission-card")).toHaveLength(2);

    await act(async () => acp.respondPermission("A", selected));
    expect(calls).toEqual([]);
    expect(acp.state.permissions.find(({ permissionId }) => permissionId === "A")?.responseRequestId).toBeTruthy();

    // No POST has been made. The Agent withdraws B, so A is now a complete
    // answer, and the page must not wait for another click on its disabled card.
    await act(async () => source.emit({ scope: SCOPE, live: { patch: [
      { op: "remove", path: "/op/interactions/B" },
    ] } }));
    expect(acp.state.permissions.map(({ permissionId }) => permissionId)).toEqual(["A"]);
    expect(calls).toHaveLength(1);
    expect(calls[0]).toMatchObject({ path: "/api/v2/scopes/session%2Fs/ops", method: "POST" });
    expect(calls[0].body).toEqual({
      expect: op.id, kind: "answer", args: { answers: { A: { outcome: selected } } },
    });

    await act(async () => calls[0].respond(202, { op: op.id }));
    expect(container.querySelectorAll(".permission-card")).toHaveLength(1);
    await act(async () => source.emit({ scope: SCOPE, live: { patch: [
      { op: "remove", path: "/op/interactions/A" },
      { op: "set", path: "/op/status", value: "completed" },
    ] } }));
    expect(acp.state.permissions).toEqual([]);
    expect(container.querySelectorAll(".permission-card")).toHaveLength(0);
    expect(calls).toHaveLength(1);
  });

  it("shows one interaction for a delete mirrored into its held session and answers through a consistent scope", async () => {
    const close: Interaction = {
      kind: "elicitation", interactionId: "close-question",
      request: {
        sessionId: SESSION, mode: "form", message: "Close this session?",
        requestedSchema: { type: "object", properties: { note: { type: "string", title: "Note" } } },
      },
    };
    const deleting = operation({
      id: "delete-s", kind: "delete", status: "running", input: { sessionId: SESSION },
      interactions: { [close.interactionId]: close },
    });
    const source = await mount(deleting, deleting);
    expect(acp.state.elicitations.map(({ elicitationId }) => elicitationId)).toEqual([close.interactionId]);
    expect(container.querySelectorAll(".elicitation-card")).toHaveLength(1);

    const answer = { action: "accept" as const, content: { note: "Close it" } };
    await act(async () => acp.respondElicitation(close.interactionId, answer));
    expect(calls).toHaveLength(1);
    // Either published scope may answer the occupying operation (§6.4).
    expect(["/api/v2/scopes/global/ops", "/api/v2/scopes/session%2Fs/ops"]).toContain(calls[0].path);
    expect(calls[0].method).toBe("POST");
    expect(calls[0].body).toEqual({
      expect: deleting.id, kind: "answer", args: { answers: { [close.interactionId]: { response: answer } } },
    });
    expect(acp.state.elicitations[0].responseRequestId).toBeTruthy();
    expect(container.querySelector(".elicitation-card")?.getAttribute("aria-busy")).toBe("true");

    // HTTP settles the local write, not either replica's pending interaction.
    await act(async () => calls[0].respond(202, { op: deleting.id }));
    expect(container.querySelectorAll(".elicitation-card")).toHaveLength(1);
    expect(container.querySelector(".elicitation-card")?.getAttribute("aria-busy")).toBe("true");
    await act(async () => {
      for (const scope of [SCOPE, "global"]) source.emit({ scope, live: { patch: [
        { op: "remove", path: "/op/interactions/close-question" },
      ] } });
    });
    expect(acp.state.elicitations).toEqual([]);
    expect(container.querySelectorAll(".elicitation-card")).toHaveLength(0);
    expect(calls).toHaveLength(1);
  });

  it.each(["draft", "sending", "uncertain"] as const)(
    "keeps %s answers when the same occupying operation aligns in its session later",
    async (stage) => {
      const deleting = operation({
        id: "delete-late-session", kind: "delete", status: "running", input: { sessionId: SESSION },
        interactions: { A: permission("A"), B: permission("B") },
      });
      const globalLive = global(deleting, deleting);
      await act(async () => root.render(<Page />));
      await act(async () => Source.instances.at(-1)!.emit(alignment("global", globalLive)));
      const source = Source.instances.at(-1)!;
      expect(new URL(source.url, window.location.href).searchParams.getAll("scope")).toContain(SCOPE);
      await act(async () => {
        source.emit(alignment("global", globalLive));
        source.emit(alignment("catalog", catalog));
      });
      // The global operation can already be answered while the subscribed
      // session's first event is still in transit.
      expect(acp.state.session).toBeUndefined();
      expect(acp.state.permissions.map(({ permissionId }) => permissionId)).toEqual(["A", "B"]);
      await act(async () => acp.respondPermission("A", selected));
      expect(calls).toEqual([]);

      if (stage !== "draft") {
        await act(async () => acp.respondPermission("B", selected));
        expect(calls).toHaveLength(1);
        expect(calls[0].path).toBe("/api/v2/scopes/global/ops");
        if (stage === "uncertain") {
          // A proxy response without a protocol error is not a definite
          // refusal: the answer may already have reached the bridge (C3).
          await act(async () => calls[0].respond(502, { message: "Upstream response was lost" }));
        }
      }
      const statusBefore = acp.state.permissions.find(({ permissionId }) => permissionId === "A")!;
      expect(statusBefore.responseRequestId).toBeTruthy();
      if (stage === "uncertain") expect(statusBefore.responseStatus).toBeTruthy();

      // The session shows that exact operation, not a new request. Its
      // alignment must not discard A's draft or the in-flight/uncertain write.
      await act(async () => source.emit(alignment(SCOPE, session(deleting))));
      expect(acp.state.session?.sessionId).toBe(SESSION);
      expect(acp.state.permissions.map(({ permissionId }) => permissionId)).toEqual(["A", "B"]);
      const statusAfter = acp.state.permissions.find(({ permissionId }) => permissionId === "A")!;
      expect(statusAfter.responseRequestId).toBeTruthy();
      expect(statusAfter.responseStatus).toBe(statusBefore.responseStatus);
      const aCard = [...container.querySelectorAll(".permission-card")]
        .find((card) => card.textContent?.includes("Tool A"));
      expect(aCard?.getAttribute("aria-busy")).toBe("true");

      if (stage === "draft") {
        expect(calls).toEqual([]);
        await act(async () => acp.respondPermission("B", selected));
      }
      expect(calls).toHaveLength(1);
      expect(["/api/v2/scopes/global/ops", "/api/v2/scopes/session%2Fs/ops"]).toContain(calls[0].path);
      expect(calls[0].body).toEqual({
        expect: deleting.id, kind: "answer",
        args: { answers: { A: { outcome: selected }, B: { outcome: selected } } },
      });
      if (stage !== "uncertain") {
        await act(async () => calls[0].respond(202, { op: deleting.id }));
      }
      // Another event must not replay an answer, including an uncertain one.
      await act(async () => source.emit({ scope: SCOPE, live: { patch: [
        { op: "set", path: "/cwd", value: CWD },
      ] } }));
      expect(calls).toHaveLength(1);
      await act(async () => {
        for (const scope of [SCOPE, "global"]) source.emit({ scope, live: { patch: [
          { op: "set", path: "/op/interactions", value: {} },
        ] } });
      });
      expect(acp.state.permissions).toEqual([]);
      expect(container.querySelectorAll(".permission-card")).toHaveLength(0);
      expect(calls).toHaveLength(1);
    },
  );
});
