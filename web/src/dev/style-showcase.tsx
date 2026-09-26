import { useEffect, useMemo, useRef, useState } from "react";
import { createRoot } from "react-dom/client";
import type { CreateElicitationRequest, RequestPermissionRequest, SessionConfigOption, SessionModeState, SessionUpdate, ToolCall } from "@agentclientprotocol/sdk";
import type { TerminalSnapshot } from "../../../shared/bridge";
import { Conversation } from "../components/acp/conversation";
import { PermissionCard } from "../components/acp/permission";
import { ElicitationCard } from "../components/acp/elicitation";
import { PromptComposer } from "../components/acp/prompt-composer";
import { SessionControls } from "../components/acp/session-controls";
import { InterfaceSettings } from "../components/interface-settings";
import { appReducer, initialState, type AppState } from "../lib/state";
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
  const projection = useMemo(() => {
    if (!session) return initialState;
    let state: AppState = { ...initialState, session: { sessionId: session.sessionId } };
    for (const update of session.updates) {
      state = appReducer(state, { type: "server/event", event: {
        type: "acp/session_update", notification: { sessionId: session.sessionId, update },
      } });
    }
    return state;
  }, [session]);
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
          {error ? <p role="alert">{error}</p> : session ? (
            panel === "conversation"
              ? <Conversation timeline={projection.timeline} terminalSnapshots={session.terminals} settled />
              : <InteractionExamples session={session} projection={projection} />
          ) : <p role="status">正在加载样例…</p>}
        </div>
      </div></div>
    </main>
  );
}

function InteractionExamples({ session, projection }: { session: MockSession; projection: AppState }) {
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

function InteractionExample({ example, session, projection }: { example: string; session: MockSession; projection: AppState }) {
  const [result, setResult] = useState("");
  const [options, setOptions] = useState(session.examples.configOptions);
  const [mode, setMode] = useState(session.examples.modes.currentModeId);
  const [stage, setStage] = useState<"running" | "cancelling" | "late">("running");
  const running = useMemo(() => appReducer(initialState, { type: "bridge/session_hydrate", view: {
    bridgeEpoch: "showcase", sessionId: session.sessionId, sessionIncarnation: 1, viewRevision: 1,
    historyRevision: "showcase", phase: "running", syncError: null, timeline: [], controls: {},
    workspace: { cwd: "/workspace", session: {} }, operation: null, terminals: {},
    interactions: { permissions: {}, elicitations: {}, urlFlows: {} },
    activeTurn: { operationId: "showcase-turn", clientIntentId: "showcase-intent", terminal: null,
      prompt: [{ type: "text", text: "检查运行中工具，点击停止后再模拟迟到结果。" }],
      cancelRequested: stage !== "running",
      updates: [...session.examples.runningUpdates, ...(stage === "late" ? [{
        sessionUpdate: "tool_call_update" as const, toolCallId: "running-tool", status: "completed" as const,
        content: [{ type: "content" as const, content: { type: "text" as const, text: "工具返回了最终结果。" } }],
      }] : [])],
    },
  } }), [session, stage]);
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
      <Conversation timeline={running.timeline} />
      <PromptComposer disabled={false} running cancelling={running.pendingPrompt?.cancelRequested} commands={[]}
        onSubmit={() => false} onCancel={() => setStage("cancelling")} />
    </> : null}
    {result ? <p className="showcase-result" role="status">示例响应：<code>{result}</code></p> : null}
  </>;
}

const disposeTheme = initializeTheme();
if (import.meta.hot) import.meta.hot.dispose(disposeTheme);
createRoot(document.getElementById("root")!).render(<StyleShowcase />);
