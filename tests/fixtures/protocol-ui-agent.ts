// A scripted ACP Agent for the protocol v2 browser gates.
//
// The test controls every ordering through files in `ATTYD_UI_GATES`:
// - `<name>.reached` is written when the Agent arrives at barrier `<name>`, which
//   then waits for `<name>.open`;
// - `<name>.arm` makes an optional barrier (a load, a control write) active;
// - `cmd-<n>.json` asks the Agent to act outside any request (send session
//   updates, ask for permission, complete a URL flow, release a terminal); the
//   Agent answers with `cmd-<n>.done`;
// - `calls.jsonl` records every ACP call the Agent received, in order;
// - `cancel-<sessionId>.reached` is written when `session/cancel` arrives. The
//   running turn still ends only when its own barrier or interactions let it,
//   and then answers `cancelled`; the next prompt clears the mark.
import { appendFileSync, existsSync, readdirSync, readFileSync, renameSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { Readable, Writable } from "node:stream";
import * as acp from "@agentclientprotocol/sdk";

const gates = process.env.ATTYD_UI_GATES;
if (gates == null) throw new Error("ATTYD_UI_GATES is required");
const requireAuth = process.env.ATTYD_UI_REQUIRE_AUTH === "1";
const authFile = join(gates, "authenticated");

const loginIndex = process.argv.indexOf("--login");
if (loginIndex >= 0) {
  const methodId = process.argv[loginIndex + 1] ?? "";
  process.stdout.write(`Login ${methodId}\r\nCode: `);
  process.stdin.setEncoding("utf8");
  let input = "";
  for await (const chunk of process.stdin) {
    input += chunk;
    if (!input.includes("\n") && !input.includes("\r")) continue;
    if (input.trim() === `code-${methodId}`) {
      writeFileSync(authFile, `${methodId}\n`);
      process.stdout.write(`\r\nSigned in with ${methodId}.\r\n`);
      process.exit(0);
    }
    process.stdout.write(`\r\nRejected ${methodId}.\r\n`);
    process.exit(1);
  }
  process.exit(1);
}

function record(method: string, detail: Record<string, unknown> = {}): void {
  appendFileSync(join(gates!, "calls.jsonl"), `${JSON.stringify({ method, ...detail })}\n`);
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

async function barrier(name: string): Promise<void> {
  writeFileSync(join(gates!, `${name}.reached`), "");
  while (!existsSync(join(gates!, `${name}.open`))) await sleep(20);
}

function armed(name: string): boolean {
  return existsSync(join(gates!, `${name}.arm`));
}

function text(value: string): acp.ContentBlock {
  return { type: "text", text: value };
}

const cwd = process.cwd();
const titles = new Map<string, string>([["alpha", "Alpha session"], ["beta", "Beta session"]]);
const histories = new Map<string, acp.SessionUpdate[]>([
  ["alpha", [
    { sessionUpdate: "user_message_chunk", messageId: "alpha-q", content: text("Alpha question") },
    { sessionUpdate: "agent_message_chunk", messageId: "alpha-a", content: text("Alpha history answer.") },
  ]],
  ["beta", [
    { sessionUpdate: "user_message_chunk", messageId: "beta-q", content: text("Beta question") },
    { sessionUpdate: "agent_message_chunk", messageId: "beta-a", content: text("Beta history answer.") },
  ]],
]);
const deleted = new Set<string>();
const terminals = new Map<string, { sessionId: string; terminalId: string }>();
const cancelled = new Set<string>();
let created = 0;
let messages = 0;

const modes = {
  currentModeId: "ask",
  availableModes: [{ id: "ask", name: "Ask" }, { id: "code", name: "Code" }],
};
function configOptions(model: string): acp.SessionConfigOption[] {
  return [{
    type: "select",
    id: "model",
    name: "Model",
    currentValue: model,
    options: [{ value: "fast", name: "Fast" }, { value: "deep", name: "Deep" }],
  }];
}

// The UI prefers config options over legacy modes, so `alpha` advertises modes
// only and every other session config options only.
function controls(sessionId: string): { modes?: typeof modes; configOptions?: acp.SessionConfigOption[] } {
  return sessionId === "alpha" ? { modes } : { configOptions: configOptions("fast") };
}

function authenticated(): boolean {
  return !requireAuth || existsSync(authFile);
}

function requireAuthentication(): void {
  if (!authenticated()) throw acp.RequestError.authRequired({ hint: "Sign in with a terminal method" });
}

let connection: acp.AgentConnection | undefined;

async function say(sessionId: string, value: string, messageId = `m-${++messages}`): Promise<void> {
  const update: acp.SessionUpdate = { sessionUpdate: "agent_message_chunk", messageId, content: text(value) };
  histories.get(sessionId)?.push(update);
  await connection!.client.notify(acp.methods.client.session.update, { sessionId, update });
}

async function notify(sessionId: string, update: acp.SessionUpdate): Promise<void> {
  await connection!.client.notify(acp.methods.client.session.update, { sessionId, update });
}

async function permission(sessionId: string, title: string, signal?: AbortSignal): Promise<string> {
  const response = await connection!.client.request(acp.methods.client.session.requestPermission, {
    sessionId,
    toolCall: { toolCallId: `tool-${title.replace(/\W+/gu, "-")}`, title },
    options: [
      { optionId: "allow", name: "Allow once", kind: "allow_once" },
      { optionId: "reject", name: "Reject", kind: "reject_once" },
    ],
  }, signal == null ? undefined : { cancellationSignal: signal });
  return response.outcome.outcome === "selected" ? response.outcome.optionId : response.outcome.outcome;
}

async function startTerminal(sessionId: string, name: string): Promise<void> {
  const finish = join(gates!, `${name}.finish`);
  const terminal = await connection!.client.request(acp.methods.client.terminal.create, {
    sessionId,
    command: process.execPath,
    args: [
      "-e",
      `const fs = require("node:fs"); process.stdout.write("TERM ${name} ready\\n"); const timer = setInterval(() => { if (fs.existsSync(${JSON.stringify(finish)})) { process.stdout.write("TERM ${name} done\\n"); clearInterval(timer); } }, 25);`,
    ],
  });
  terminals.set(name, { sessionId, terminalId: terminal.terminalId });
  await notify(sessionId, {
    sessionUpdate: "tool_call",
    toolCallId: `terminal-${name}`,
    title: `Run terminal ${name}`,
    kind: "execute",
    status: "in_progress",
    content: [{ type: "terminal", terminalId: terminal.terminalId }],
  });
}

async function prompt(sessionId: string, promptText: string): Promise<acp.StopReason> {
  const [word, ...rest] = promptText.trim().split(/\s+/u);
  const name = rest.join(" ");
  switch (word) {
    case "wait": {
      await notify(sessionId, { sessionUpdate: "agent_thought_chunk", content: text(`Thinking about ${name}.`) });
      await notify(sessionId, {
        sessionUpdate: "plan",
        entries: [{ content: `Plan for ${name}`, priority: "high", status: "in_progress" }],
      });
      await notify(sessionId, {
        sessionUpdate: "tool_call",
        toolCallId: `step-${name}`,
        title: `Step ${name}`,
        kind: "read",
        status: "in_progress",
      });
      const messageId = `m-${++messages}`;
      await say(sessionId, `Working on ${name}.`, messageId);
      await barrier(name);
      await notify(sessionId, { sessionUpdate: "tool_call_update", toolCallId: `step-${name}`, status: "completed" });
      await say(sessionId, ` Finished ${name}.`, messageId);
      return "end_turn";
    }
    case "permission": {
      const outcome = await permission(sessionId, `Permission ${name}`);
      await say(sessionId, `Permission ${name}: ${outcome}.`);
      return "end_turn";
    }
    case "two-permissions": {
      const [first, second] = await Promise.all([
        permission(sessionId, "Tool one"),
        permission(sessionId, "Tool two"),
      ]);
      await say(sessionId, `Answers: ${first}, ${second}.`);
      return "end_turn";
    }
    case "form-join": {
      const form = connection!.client.request(acp.methods.client.elicitation.create, {
        sessionId,
        mode: "form",
        message: "Describe the change",
        requestedSchema: {
          type: "object",
          required: ["note"],
          properties: { note: { type: "string", title: "Note" } },
        },
      });
      const joined = (async () => {
        await barrier("join");
        return permission(sessionId, "Joined check");
      })();
      const [response, check] = await Promise.all([form, joined]);
      const content = (response as { content?: Record<string, unknown> }).content;
      const note = response.action === "accept" ? String(content?.note ?? "") : response.action;
      await say(sessionId, `Note: ${note}; joined: ${check}.`);
      return "end_turn";
    }
    case "form-shrink": {
      // A form and a permission; at barrier `shrink` the Agent withdraws the
      // permission, so the pending set shrinks to the form alone.
      const withdrawn = new AbortController();
      const check = permission(sessionId, "Shrinking check", withdrawn.signal).catch(() => "withdrawn");
      const form = connection!.client.request(acp.methods.client.elicitation.create, {
        sessionId,
        mode: "form",
        message: "Describe the change",
        requestedSchema: {
          type: "object",
          required: ["note"],
          properties: { note: { type: "string", title: "Note" } },
        },
      });
      void (async () => {
        await barrier("shrink");
        withdrawn.abort();
      })();
      const [response, outcome] = await Promise.all([form, check]);
      const content = (response as { content?: Record<string, unknown> }).content;
      const note = response.action === "accept" ? String(content?.note ?? "") : response.action;
      await say(sessionId, `Note: ${note}; check: ${outcome}.`);
      return "end_turn";
    }
    case "url": {
      const response = await connection!.client.request(acp.methods.client.elicitation.create, {
        sessionId,
        mode: "url",
        message: `Connect ${name}`,
        elicitationId: name,
        url: `https://example.test/${name}`,
      });
      await say(sessionId, `URL ${name}: ${response.action}.`);
      return "end_turn";
    }
    case "terminal": {
      await startTerminal(sessionId, name);
      await say(sessionId, `Terminal ${name} started.`);
      void (async () => {
        await barrier(`release-${name}`);
        const terminal = terminals.get(name)!;
        await connection!.client.request(acp.methods.client.terminal.release, terminal);
        terminals.delete(name);
        record("terminal/release", { sessionId, name });
      })();
      return "end_turn";
    }
    case "image": {
      const update: acp.SessionUpdate = {
        sessionUpdate: "agent_message_chunk",
        messageId: `m-${++messages}`,
        content: {
          type: "image",
          mimeType: "image/png",
          data: "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYAAAAAYAAjCB0C8AAAAASUVORK5CYII=",
        },
      };
      histories.get(sessionId)?.push(update);
      await notify(sessionId, update);
      await say(sessionId, "Image reply.");
      return "end_turn";
    }
    case "fail":
      throw new acp.RequestError(-32603, "Synthetic prompt failure");
    default:
      await say(sessionId, `Echo: ${promptText}`);
      return "end_turn";
  }
}

async function runCommand(command: Record<string, unknown>): Promise<unknown> {
  const sessionId = String(command.sessionId ?? "");
  switch (command.type) {
    case "notify":
      for (const update of command.updates as acp.SessionUpdate[]) await notify(sessionId, update);
      return {};
    case "permission":
      return { outcome: await permission(sessionId, String(command.title)) };
    case "complete":
      await connection!.client.notify(acp.methods.client.elicitation.complete, {
        elicitationId: String(command.elicitationId),
      });
      return {};
    case "say":
      await say(sessionId, String(command.text));
      return {};
    default:
      throw new Error(`Unknown command ${String(command.type)}`);
  }
}

async function pollCommands(): Promise<never> {
  for (;;) {
    const pending = readdirSync(gates!)
      .filter((file) => /^cmd-\d+\.json$/u.test(file))
      .sort((a, b) => Number(a.slice(4, -5)) - Number(b.slice(4, -5)));
    for (const file of pending) {
      const path = join(gates!, file);
      const running = `${path}.running`;
      renameSync(path, running);
      const command = JSON.parse(readFileSync(running, "utf8")) as Record<string, unknown>;
      // Commands run concurrently: a permission waits for its answer while the
      // test sends further commands.
      void runCommand(command).then(
        (result) => writeFileSync(path.replace(/\.json$/u, ".done"), JSON.stringify({ result })),
        (error: unknown) => writeFileSync(path.replace(/\.json$/u, ".done"), JSON.stringify({ error: String(error) })),
      );
    }
    await sleep(20);
  }
}

const agent = acp.agent({ name: "protocol-ui-fixture" })
  .onRequest(acp.methods.agent.initialize, () => {
    record("initialize");
    return {
      protocolVersion: acp.PROTOCOL_VERSION,
      agentCapabilities: {
        loadSession: true,
        promptCapabilities: { image: true, embeddedContext: true },
        sessionCapabilities: { list: {}, fork: {}, close: {}, delete: {} },
      },
      agentInfo: { name: "protocol-ui-fixture", version: "1.0.0" },
      authMethods: requireAuth
        ? (["login-a", "login-b"] as const).map((id) => ({
            id,
            name: `Sign in with ${id}`,
            type: "terminal" as const,
            args: ["--login", id],
            env: { ATTYD_UI_GATES: gates },
          }))
        : [],
    };
  })
  .onRequest(acp.methods.agent.session.list, () => {
    record("session/list");
    requireAuthentication();
    return {
      sessions: [...histories.keys()].filter((id) => !deleted.has(id))
        .map((sessionId) => ({ sessionId, cwd, title: titles.get(sessionId) ?? null })),
    };
  })
  .onRequest(acp.methods.agent.session.new, () => {
    requireAuthentication();
    const sessionId = `new-${++created}`;
    record("session/new", { sessionId });
    histories.set(sessionId, []);
    return { sessionId, ...controls(sessionId) };
  })
  .onRequest(acp.methods.agent.session.load, async ({ params }) => {
    requireAuthentication();
    record("session/load", { sessionId: params.sessionId });
    const history = histories.get(params.sessionId);
    if (history == null || deleted.has(params.sessionId)) {
      throw new acp.RequestError(-32002, "Session not found");
    }
    if (armed(`load-permission-${params.sessionId}`)) {
      const outcome = await permission(params.sessionId, `Load check ${params.sessionId}`);
      record("load/permission", { sessionId: params.sessionId, outcome });
    }
    if (armed(`load-${params.sessionId}`)) await barrier(`load-${params.sessionId}`);
    for (const update of history) await notify(params.sessionId, update);
    return controls(params.sessionId);
  })
  .onRequest(acp.methods.agent.session.fork, ({ params }) => {
    const sessionId = `fork-${++created}`;
    record("session/fork", { sessionId: params.sessionId, forked: sessionId });
    histories.set(sessionId, [...(histories.get(params.sessionId) ?? [])]);
    return { sessionId, ...controls(sessionId) };
  })
  .onRequest(acp.methods.agent.session.close, ({ params }) => {
    record("session/close", { sessionId: params.sessionId });
    return {};
  })
  .onRequest(acp.methods.agent.session.delete, ({ params }) => {
    record("session/delete", { sessionId: params.sessionId });
    deleted.add(params.sessionId);
    return {};
  })
  .onRequest(acp.methods.agent.session.setMode, async ({ params }) => {
    record("session/set_mode", { sessionId: params.sessionId, modeId: params.modeId });
    if (armed("control")) await barrier("control");
    return {};
  })
  .onRequest(acp.methods.agent.session.setConfigOption, async ({ params }) => {
    record("session/set_config_option", { sessionId: params.sessionId, configId: params.configId, value: params.value });
    if (armed("control")) await barrier("control");
    return { configOptions: configOptions(String(params.value)) };
  })
  .onRequest(acp.methods.agent.session.prompt, async ({ params }) => {
    const promptText = params.prompt.map((block) => block.type === "text" ? block.text : `[${block.type}]`).join("");
    record("session/prompt", { sessionId: params.sessionId, text: promptText, blocks: params.prompt.map((block) => block.type) });
    const history = histories.get(params.sessionId);
    const messageId = `u-${++messages}`;
    for (const content of params.prompt) {
      history?.push({ sessionUpdate: "user_message_chunk", messageId, content });
    }
    cancelled.delete(params.sessionId);
    rmSync(join(gates, `cancel-${params.sessionId}.reached`), { force: true });
    const stopReason = await prompt(params.sessionId, promptText);
    return { stopReason: cancelled.delete(params.sessionId) ? "cancelled" : stopReason };
  })
  .onNotification(acp.methods.agent.session.cancel, ({ params }) => {
    record("session/cancel", { sessionId: params.sessionId });
    cancelled.add(params.sessionId);
    writeFileSync(join(gates, `cancel-${params.sessionId}.reached`), "");
  });

connection = agent.connect(acp.ndJsonStream(
  Writable.toWeb(process.stdout) as WritableStream<Uint8Array>,
  Readable.toWeb(process.stdin) as ReadableStream<Uint8Array>,
));
void pollCommands();
await connection.closed;
