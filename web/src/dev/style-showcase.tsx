import { useEffect, useMemo, useRef, useState } from "react";
import { createRoot } from "react-dom/client";
import type { AvailableCommand, CreateElicitationRequest, RequestPermissionRequest, SessionConfigOption, SessionModeState, SessionUpdate, ToolCall } from "@agentclientprotocol/sdk";
import type { TerminalSnapshot } from "../../../shared/bridge";
import type { Operation, ProcessUpdate } from "../../../shared/protocol";
import { Conversation } from "../components/acp/conversation";
import { PermissionCard } from "../components/acp/permission";
import { ElicitationCard } from "../components/acp/elicitation";
import { PromptComposer } from "../components/acp/prompt-composer";
import { SessionControls } from "../components/acp/session-controls";
import { InterfaceSettings } from "../components/interface-settings";
import type { AppState, AssistantMessageChunk, TimelineItem } from "../lib/state";
import { projectProcess, projectTimeline } from "../lib/workspace-projection";
import { initializeTheme } from "../lib/theme";
import "../styles.css";
import "./style-showcase.css";

interface MockSession {
  sessionId: string;
  title: string;
  updates: SessionUpdate[];
  terminals: TerminalSnapshot[];
  examples: {
    permissions: Array<{ name: string; request: RequestPermissionRequest; toolCall?: ToolCall }>;
    elicitations: Array<{ name: string; request: CreateElicitationRequest }>;
    configOptions: SessionConfigOption[];
    modes: SessionModeState;
    runningUpdates: SessionUpdate[];
  };
}

function StyleShowcase() {
  const [session, setSession] = useState<MockSession>();
  const [error, setError] = useState<string>();
  const [panel, setPanel] = useState("conversation");
  const settings = useRef<HTMLDetailsElement>(null);
  useEffect(() => {
    const controller = new AbortController();
    void fetch("/dev/style-session.json", { signal: controller.signal, cache: "no-store" })
      .then(async (response) => {
        if (!response.ok) throw new Error(`样例加载失败：${response.status}`);
        setSession(await response.json() as MockSession);
      })
      .catch((error: unknown) => {
        if (!controller.signal.aborted) setError(String(error));
      });
    return () => controller.abort();
  }, []);
  const projection = useMemo(() => session ? showcaseProjection(session) : undefined, [session]);
  return (
    <main className="main-panel">
      <header className="session-header">
        <div><h1>{session?.title ?? "样式检查"}</h1><p>静态会话样例 · 展开内容检查排版</p></div>
        <InterfaceSettings menuRef={settings} />
      </header>
      <nav className="showcase-tabs" aria-label="样例分类">
        <button type="button" aria-pressed={panel === "conversation"} onClick={() => setPanel("conversation")}>会话内容</button>
        <button type="button" aria-pressed={panel === "interactions"} onClick={() => setPanel("interactions")}>审批、设置与状态</button>
      </nav>
      <div className="thread-body"><div className="scroll-region" role="region" aria-label="样式检查会话">
        <div className="conversation-wrap">
          {error ? <p role="alert">{error}</p> : session && projection ? (
            panel === "conversation"
              ? <Conversation timeline={projection.timeline} terminalSnapshots={session.terminals} settled />
              : <InteractionExamples session={session} projection={projection} />
          ) : <p role="status">正在加载样例…</p>}
        </div>
      </div></div>
    </main>
  );
}

/**
 * The sample as the thread shows it: each prompt and what follows it is one
 * turn's process (§4.4), and the controls updates are what the session shows
 * beside the thread.
 */
function showcaseProjection(session: MockSession): ShowcaseProjection {
  const turns: ProcessUpdate[][] = [];
  let availableCommands: AvailableCommand[] = [];
  let usage: AppState["usage"];
  let previous: SessionUpdate | undefined;
  for (const update of session.updates) {
    if (update.sessionUpdate === "usage_update") usage = { used: update.used, size: update.size, cost: update.cost };
    else if (update.sessionUpdate === "available_commands_update") availableCommands = update.availableCommands;
    else {
      if (turns.length === 0 || (update.sessionUpdate === "user_message_chunk" && previous?.sessionUpdate !== "user_message_chunk")) {
        turns.push([]);
      }
      turns.at(-1)!.push(update as ProcessUpdate);
    }
    previous = update;
  }
  const timeline = turns.flatMap((process, index) =>
    joinMessageBlocks(projectProcess(session.sessionId, `showcase-${index}`, process).items));
  return { timeline, availableCommands, usage };
}

/** One entry per message: the sample spreads a message's blocks over chunks. */
function joinMessageBlocks(items: TimelineItem[]): TimelineItem[] {
  const joined: TimelineItem[] = [];
  for (const item of items) {
    const last = joined.at(-1);
    if (item.type === "message" && last?.type === "message" && item.messageId != null && item.messageId === last.messageId) {
      joined[joined.length - 1] = { ...last, blocks: [...last.blocks, ...item.blocks] };
    } else if (item.type === "assistant") {
      const chunks: AssistantMessageChunk[] = [];
      for (const chunk of item.chunks) {
        const previous = chunks.at(-1);
        if (previous != null && chunk.messageId != null && chunk.messageId === previous.messageId && chunk.role === previous.role) {
          chunks[chunks.length - 1] = { ...previous, blocks: [...previous.blocks, ...chunk.blocks] };
        } else chunks.push(chunk);
      }
      joined.push({ ...item, chunks });
    } else joined.push(item);
  }
  return joined;
}

type ShowcaseProjection = Pick<AppState, "availableCommands" | "usage"> & { timeline: TimelineItem[] };

function InteractionExamples({ session, projection }: { session: MockSession; projection: ShowcaseProjection }) {
  const [example, setExample] = useState("permission-0");
  return <section className="showcase-interactions">
    <label className="showcase-selector">检查场景
      <select value={example} onChange={(event) => setExample(event.target.value)}>
        {session.examples.permissions.map((item, index) => <option key={`permission-${index}`} value={`permission-${index}`}>{item.name}</option>)}
        {session.examples.elicitations.map((item, index) => <option key={`form-${index}`} value={`form-${index}`}>{item.name}</option>)}
        <option value="settings">分组设置、模式与上下文用量</option>
        <option value="cancellation">运行、取消与迟到结果</option>
      </select>
    </label>
    <InteractionExample key={example} example={example} session={session} projection={projection} />
  </section>;
}

function InteractionExample({ example, session, projection }: { example: string; session: MockSession; projection: ShowcaseProjection }) {
  const [result, setResult] = useState("");
  const [options, setOptions] = useState(session.examples.configOptions);
  const [mode, setMode] = useState(session.examples.modes.currentModeId);
  const [stage, setStage] = useState<"running" | "cancelling" | "late">("running");
  // A cancel request does not end the turn or its tools (§4.4): only the
  // Agent's later result does, folded into its call like the bridge folds it.
  const running = useMemo((): Operation => ({
    id: "showcase-turn", kind: "prompt", status: "running", cancelRequested: stage !== "running", interactions: {},
    input: { prompt: [{ type: "text", text: "检查运行中工具，点击停止后再模拟迟到结果。" }] },
    process: session.examples.runningUpdates.map((update) => (stage === "late" &&
      update.sessionUpdate === "tool_call" && update.toolCallId === "running-tool" ? {
        ...update, status: "completed" as const,
        content: [{ type: "content" as const, content: { type: "text" as const, text: "工具返回了最终结果。" } }],
      } : update) as ProcessUpdate),
  }), [session, stage]);
  const permission = example.startsWith("permission-") ? session.examples.permissions[Number(example.slice(11))] : undefined;
  const elicitation = example.startsWith("form-") ? session.examples.elicitations[Number(example.slice(5))] : undefined;
  const settings = <SessionControls options={options} modes={null} disabled={false} onMode={() => {}}
    onConfig={(id, value) => setOptions((current) => current.map((option) => option.id !== id ? option
      : option.type === "boolean" ? { ...option, currentValue: Boolean(value) } : { ...option, currentValue: String(value) }))} />;
  return <>
    {permission ? <PermissionCard pending={{ permissionId: example, request: permission.request }}
      toolCall={permission.toolCall} terminalSnapshots={session.terminals} onRespond={(outcome) => setResult(JSON.stringify(outcome))} /> : null}
    {elicitation ? <ElicitationCard agentName="示例 Agent" pending={{ elicitationId: example, request: elicitation.request }}
      onRespond={(response) => setResult(JSON.stringify(response))} /> : null}
    {example === "settings" ? <>
      <h2>分组设置与上下文用量</h2>
      <PromptComposer disabled={false} running={false} commands={projection.availableCommands} usage={projection.usage}
        sessionControls={settings} onSubmit={() => false} onCancel={() => {}} />
      <h2>旧版模式选择器</h2>
      <div className="showcase-settings">
        <SessionControls options={null} modes={session.examples.modes} currentMode={mode} disabled={false} onMode={setMode} onConfig={() => {}} />
      </div>
    </> : null}
    {example === "cancellation" ? <>
      <div className="showcase-actions">
        <button type="button" onClick={() => setStage("running")}>恢复运行</button>
        <button type="button" disabled={stage !== "cancelling"} onClick={() => setStage("late")}>模拟工具返回结果</button>
      </div>
      <Conversation timeline={projectTimeline(session.sessionId, [], running)} />
      <PromptComposer disabled={false} running cancelling={running.cancelRequested} commands={[]}
        onSubmit={() => false} onCancel={() => setStage("cancelling")} />
    </> : null}
    {result ? <p className="showcase-result" role="status">示例响应：<code>{result}</code></p> : null}
  </>;
}

const disposeTheme = initializeTheme();
if (import.meta.hot) import.meta.hot.dispose(disposeTheme);
createRoot(document.getElementById("root")!).render(<StyleShowcase />);
