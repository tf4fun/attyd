import type { ContentBlock, ToolCall } from "@agentclientprotocol/sdk";
import {
  Activity,
  ArrowDownToLine,
  ArrowUpToLine,
  Bot,
  ChevronDown,
  Ellipsis,
  FileText,
  FolderGit2,
  GitFork,
  LogOut,
  Menu,
  Plus,
  Search as SearchIcon,
  ScrollText,
  Settings2,
  ShieldCheck,
  X,
} from "lucide-react";
import { lazy, Suspense, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { ChangeReview } from "./components/acp/change-review";
import { AgentAuthCard, AgentAuthControls } from "./components/acp/agent-auth";
import { Conversation } from "./components/acp/conversation";
import { ElicitationCard, ExternalFlowCard } from "./components/acp/elicitation";
import { PermissionCard } from "./components/acp/permission";
import { PlanCard } from "./components/acp/plan";
import { NewSessionDialog } from "./components/acp/new-session-dialog";
import {
  PromptComposer,
  type ComposerDraft,
  type ThreadNavigationTarget,
} from "./components/acp/prompt-composer";
import {
  MAX_QUEUED_PROMPTS,
  QueuedPrompts,
  type QueuedPrompt,
} from "./components/acp/queued-prompts";
import { RawJson } from "./components/acp/raw-json";
import { SessionControls } from "./components/acp/session-controls";
import { SessionHistory } from "./components/acp/session-history";
import { ThreadSearchBar } from "./components/acp/thread-search";
import { randomId } from "./lib/id";
import { collectPromptHistory } from "./lib/prompt-history";
import { timelineToMarkdown } from "./lib/thread-markdown";
import { collectReviewChanges } from "./lib/review-changes";
import type { AgentActivity, TimelineItem } from "./lib/state";
import { useAcp } from "./lib/use-acp";

const AuthTerminalCard = lazy(() => import("./components/acp/auth-terminal").then(
  ({ AuthTerminalCard: component }) => ({ default: component }),
));

export default function App() {
  const {
    state,
    reconnect,
    authenticate,
    writeAuthTerminal,
    resizeAuthTerminal,
    cancelAuthTerminal,
    dismissAuthTerminal,
    logout,
    prompt,
    cancel,
    setMode,
    setConfig,
    respondPermission,
    respondElicitation,
    dismissExternalFlow,
    newSession,
    listSessions,
    attachSession,
    forkSession,
    closeSession,
    deleteSession,
    searchWorkspaceContext,
    readWorkspaceContext,
  } = useAcp();
  const [sidebarOpen, setSidebarOpen] = useState(false);
  const [newThreadOpen, setNewThreadOpen] = useState(false);
  const [composerDraft, setComposerDraft] = useState<ComposerDraft>();
  const [queuedPrompts, setQueuedPrompts] = useState<QueuedPrompt[]>([]);
  const [queueError, setQueueError] = useState<string>();
  const [queuePaused, setQueuePaused] = useState(false);
  const [reviewOpen, setReviewOpen] = useState(false);
  const [threadSearchOpen, setThreadSearchOpen] = useState(false);
  const [threadSearchFocusRequest, setThreadSearchFocusRequest] = useState(0);
  const [threadScroll, setThreadScroll] = useState({
    overflow: false,
    atTop: true,
    atBottom: true,
  });
  const scroll = useRef<HTMLDivElement>(null);
  const threadSearchContainer = useRef<HTMLDivElement>(null);
  const mobileMenu = useRef<HTMLButtonElement>(null);
  const newThreadButton = useRef<HTMLButtonElement>(null);
  const sidebarClose = useRef<HTMLButtonElement>(null);
  const previousRunning = useRef(false);
  const lastPositionedSession = useRef<string | undefined>(undefined);

  const measureThreadScroll = useCallback(() => {
    const element = scroll.current;
    if (!element) return;
    const next = {
      overflow: element.scrollHeight > element.clientHeight + 1,
      atTop: element.scrollTop <= 2,
      atBottom: element.scrollHeight - element.clientHeight - element.scrollTop <= 2,
    };
    setThreadScroll((current) =>
      current.overflow === next.overflow &&
      current.atTop === next.atTop &&
      current.atBottom === next.atBottom
        ? current
        : next
    );
  }, []);

  const navigateThread = useCallback((target: ThreadNavigationTarget) => {
    const element = scroll.current;
    if (!element) return;
    if (target === "top" || target === "bottom") {
      // Boundary controls should be deterministic even while streamed content
      // is changing the scroll height. Smooth scrolling can be interrupted by
      // those layout updates and leave the thread between endpoints.
      element.scrollTop = target === "top"
        ? 0
        : Math.max(0, element.scrollHeight - element.clientHeight);
      requestAnimationFrame(measureThreadScroll);
      return;
    }
    if (target === "page-up" || target === "page-down") {
      element.scrollBy({
        top: element.clientHeight * (target === "page-up" ? -0.85 : 0.85),
        behavior: "smooth",
      });
      return;
    }

    const selector = target === "previous-prompt" || target === "next-prompt" || target === "latest-prompt"
      ? '[data-thread-role="user"]'
      : "[data-thread-entry]";
    const entries = [...element.querySelectorAll<HTMLElement>(selector)];
    if (entries.length === 0) {
      if (target === "latest-prompt") {
        element.scrollTo({ top: element.scrollHeight, behavior: "smooth" });
      }
      return;
    }
    if (target === "latest-prompt") {
      entries.at(-1)?.scrollIntoView({ block: "start", behavior: "smooth" });
      return;
    }
    const rootTop = element.getBoundingClientRect().top;
    const previous = target === "previous-message" || target === "previous-prompt";
    const candidate = previous
      ? [...entries].reverse().find((entry) => entry.getBoundingClientRect().top < rootTop - 4)
      : entries.find((entry) => entry.getBoundingClientRect().top > rootTop + 4);
    candidate?.scrollIntoView({ block: "start", behavior: "smooth" });
  }, [measureThreadScroll]);

  useLayoutEffect(() => {
    const sessionId = state.session?.sessionId;
    if (!sessionId) {
      lastPositionedSession.current = undefined;
      return;
    }
    if (lastPositionedSession.current === sessionId) return;
    lastPositionedSession.current = sessionId;

    const element = scroll.current;
    if (!element) return;
    const moveToLatest = () => {
      element.scrollTop = Math.max(0, element.scrollHeight - element.clientHeight);
    };
    moveToLatest();
    const frame = requestAnimationFrame(() => {
      moveToLatest();
      measureThreadScroll();
    });
    return () => cancelAnimationFrame(frame);
  }, [measureThreadScroll, state.session?.sessionId]);

  useEffect(() => {
    const element = scroll.current;
    if (!element) return;

    let keepLatestVisible = false;
    let frame = 0;
    const composerOwnsFocus = () =>
      document.activeElement instanceof Element &&
      document.activeElement.closest(".composer") != null;
    const moveToLatest = () => {
      if (!keepLatestVisible || !composerOwnsFocus()) return;
      element.scrollTop = Math.max(0, element.scrollHeight - element.clientHeight);
      measureThreadScroll();
    };
    const followViewport = () => {
      if (!keepLatestVisible || !composerOwnsFocus()) return;
      cancelAnimationFrame(frame);
      frame = requestAnimationFrame(() => {
        moveToLatest();
        frame = requestAnimationFrame(moveToLatest);
      });
    };
    const rememberPosition = (event: FocusEvent) => {
      if (!(event.target instanceof Element) || event.target.closest(".composer") == null) return;
      const distance = element.scrollHeight - element.scrollTop - element.clientHeight;
      keepLatestVisible = distance <= 48;
      followViewport();
    };
    const stopFollowing = () => {
      keepLatestVisible = false;
      cancelAnimationFrame(frame);
    };

    document.addEventListener("focusin", rememberPosition);
    element.addEventListener("touchmove", stopFollowing, { passive: true });
    element.addEventListener("wheel", stopFollowing, { passive: true });
    window.addEventListener("resize", followViewport);
    window.visualViewport?.addEventListener("resize", followViewport);
    window.visualViewport?.addEventListener("scroll", followViewport);
    return () => {
      cancelAnimationFrame(frame);
      document.removeEventListener("focusin", rememberPosition);
      element.removeEventListener("touchmove", stopFollowing);
      element.removeEventListener("wheel", stopFollowing);
      window.removeEventListener("resize", followViewport);
      window.visualViewport?.removeEventListener("resize", followViewport);
      window.visualViewport?.removeEventListener("scroll", followViewport);
    };
  }, [measureThreadScroll]);

  const closeMobileSidebar = () => {
    setSidebarOpen(false);
    requestAnimationFrame(() => mobileMenu.current?.focus());
  };

  const closeNewThread = useCallback(() => {
    setNewThreadOpen(false);
    requestAnimationFrame(() => {
      if (window.innerWidth <= 760) mobileMenu.current?.focus();
      else newThreadButton.current?.focus();
    });
  }, []);

  const closeThreadSearch = useCallback(() => {
    setThreadSearchOpen(false);
    requestAnimationFrame(() => {
      document.querySelector<HTMLTextAreaElement>(".composer textarea")?.focus();
    });
  }, []);

  const toggleThreadSearch = useCallback(() => {
    const searchOwnsFocus = threadSearchContainer.current?.contains(document.activeElement);
    if (threadSearchOpen && searchOwnsFocus) {
      closeThreadSearch();
      return;
    }
    setThreadSearchOpen(true);
    setThreadSearchFocusRequest((request) => request + 1);
  }, [closeThreadSearch, threadSearchOpen]);

  useEffect(() => {
    if (!sidebarOpen) return;
    sidebarClose.current?.focus();
    const closeOnEscape = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      event.preventDefault();
      closeMobileSidebar();
    };
    window.addEventListener("keydown", closeOnEscape);
    return () => window.removeEventListener("keydown", closeOnEscape);
  }, [sidebarOpen]);

  useEffect(() => {
    const element = scroll.current;
    if (!element) return;
    const distance = element.scrollHeight - element.scrollTop - element.clientHeight;
    if (distance < 420) element.scrollTo({ top: element.scrollHeight, behavior: "smooth" });
    const frame = requestAnimationFrame(measureThreadScroll);
    return () => cancelAnimationFrame(frame);
  }, [measureThreadScroll, state.timeline, state.permissions, state.elicitations]);

  useEffect(() => {
    const element = scroll.current;
    if (!element) return;
    measureThreadScroll();
    if (typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(measureThreadScroll);
    observer.observe(element);
    const content = element.querySelector(".conversation-wrap");
    if (content) observer.observe(content);
    return () => observer.disconnect();
  }, [measureThreadScroll, state.session?.sessionId]);

  useEffect(() => {
    setComposerDraft(undefined);
    setQueuedPrompts([]);
    setQueueError(undefined);
    setQueuePaused(false);
    setReviewOpen(false);
    setThreadSearchOpen(false);
    previousRunning.current = false;
  }, [state.session?.sessionId]);

  useEffect(() => {
    if (state.phase !== "stopped" && state.phase !== "error") return;
    setQueuedPrompts([]);
    setQueueError(undefined);
    setQueuePaused(false);
    previousRunning.current = false;
  }, [state.phase]);

  const agent = state.initialized?.agentInfo;
  const agentCapabilities = state.initialized?.agentCapabilities;
  const sessionCapabilities = agentCapabilities?.sessionCapabilities;
  const authMethods = state.initialized?.authMethods ?? [];
  const authTerminalMethod = state.authTerminal == null
    ? undefined
    : authMethods.find(({ id }) => id === state.authTerminal?.methodId);
  const transitioning = state.sessionTransition != null;
  const changingControl = state.pendingSessionControl != null;
  const authBlocksCurrent = state.authStatus === "required" || state.pendingAuth != null;
  const authBlocksNewSession = authBlocksCurrent || state.authStatus === "logged_out";
  const ready = state.phase === "ready" && state.session != null && !transitioning && !changingControl && !authBlocksCurrent;
  const terminalAuthOwnsInteraction = state.authTerminal?.status === "starting" ||
    state.authTerminal?.status === "running" ||
    state.authTerminal?.status === "succeeded";
  const showAuthCard = authMethods.length > 0 &&
    (state.authStatus === "required" || state.authStatus === "logged_out") &&
    !terminalAuthOwnsInteraction;
  const reviewChanges = useMemo(() => collectReviewChanges(state.timeline), [state.timeline]);
  const promptHistory = useMemo(() => collectPromptHistory(state.timeline), [state.timeline]);
  const openActiveThreadMarkdown = useCallback(() => {
    openThreadMarkdown(timelineToMarkdown(state.timeline, {
      title: state.title ?? "Agent thread",
      agentName: agent?.title ?? agent?.name,
      sessionId: state.session?.sessionId,
      cwd: state.cwd,
      terminalSnapshots: state.terminalSnapshots,
    }));
  }, [
    agent?.name,
    agent?.title,
    state.cwd,
    state.session?.sessionId,
    state.terminalSnapshots,
    state.timeline,
    state.title,
  ]);

  useEffect(() => {
    if (reviewChanges.fileCount === 0) setReviewOpen(false);
  }, [reviewChanges.fileCount]);

  useEffect(() => {
    const turnFinished = previousRunning.current && !state.running;
    previousRunning.current = state.running;
    if (!turnFinished || !ready || queuePaused || queuedPrompts.length === 0) return;
    const next = queuedPrompts[0];
    if (next.sessionId !== state.session?.sessionId) {
      setQueuedPrompts((current) => current.filter(({ id }) => id !== next.id));
      setQueueError("A queued message was discarded because its ACP session changed.");
      return;
    }
    if (!prompt(next.blocks)) {
      setQueueError("The queued message could not be sent to the Agent.");
      return;
    }
    setQueuedPrompts((current) => current[0]?.id === next.id
      ? current.slice(1)
      : current.filter(({ id }) => id !== next.id));
    setQueueError(undefined);
    setReviewOpen(false);
  }, [prompt, queuePaused, queuedPrompts, ready, state.running, state.session?.sessionId]);

  const submitPrompt = (
    text: string,
    attachments: ContentBlock[],
    restoredBlocks?: ContentBlock[],
  ): boolean => {
    const blocks: ContentBlock[] = restoredBlocks ?? [
      ...(text ? [{ type: "text" as const, text }] : []),
      ...attachments,
    ];
    setComposerDraft(undefined);
    if (!state.running) {
      setQueuePaused(false);
      return prompt(blocks);
    }
    const sessionId = state.session?.sessionId;
    if (!sessionId) return false;
    if (queuedPrompts.length >= MAX_QUEUED_PROMPTS) {
      setQueueError(`Only ${MAX_QUEUED_PROMPTS} messages can be queued for one ACP turn.`);
      return false;
    }
    setQueuedPrompts((current) => [
      ...current,
      { id: randomId(), sessionId, blocks },
    ]);
    setQueuePaused(false);
    setQueueError(undefined);
    return true;
  };

  return (
    <div className="app-shell">
      <button
        ref={mobileMenu}
        className="mobile-menu"
        aria-label="Open sidebar"
        aria-controls="app-sidebar"
        aria-expanded={sidebarOpen}
        onClick={() => setSidebarOpen(true)}
      >
        <Menu size={18} />
      </button>
      <aside
        id="app-sidebar"
        className={`sidebar ${sidebarOpen ? "sidebar-open" : ""}`}
        aria-label="Application sidebar"
      >
        <div className="brand">
          <div className="brand-mark">a<span>&gt;</span></div>
          <div><strong>attyd</strong><small>Agent threads</small></div>
          <button
            ref={sidebarClose}
            className="close-sidebar"
            aria-label="Close sidebar"
            onClick={closeMobileSidebar}
          ><X size={17} /></button>
        </div>
        <button
          ref={newThreadButton}
          className="new-session"
          aria-label="New thread"
          disabled={state.phase !== "ready" || state.running || transitioning || changingControl || authBlocksNewSession || queuedPrompts.length > 0}
          onClick={() => {
            setSidebarOpen(false);
            setNewThreadOpen(true);
          }}
        >
          <Plus size={15} /> New thread
        </button>

        <SessionHistory
          sessions={state.sessions}
          activeSessionId={state.session?.sessionId}
          activeTitle={state.title ?? (state.session ? "New agent session" : undefined)}
          canList={sessionCapabilities?.list != null}
          nextCursor={state.nextSessionCursor}
          canAttach={Boolean(agentCapabilities?.loadSession || sessionCapabilities?.resume != null)}
          canDelete={sessionCapabilities?.delete != null}
          deletingSessionIds={state.pendingSessionDeletions.map(({ sessionId }) => sessionId)}
          openSessionIds={[...state.cachedSessions.keys()]}
          disabled={state.running || transitioning || changingControl || authBlocksCurrent || queuedPrompts.length > 0}
          onAttach={(session) => {
            attachSession(session);
            closeMobileSidebar();
          }}
          onDelete={(sessionId) => {
            if (window.confirm("Delete this session from the agent?")) deleteSession(sessionId);
          }}
          onRefresh={() => listSessions()}
          onMore={(cursor) => listSessions(cursor)}
        />

        <div className="sidebar-spacer" />
        <details className="agent-details">
          <summary>
            <StatusDot phase={state.phase} />
            <span><strong>{agent?.title ?? agent?.name ?? "Starting agent"}</strong><small>{phaseLabel(state.phase)} · ACP v{state.initialized?.protocolVersion ?? "–"}</small></span>
            <ChevronDown size={13} />
          </summary>
          <div className="agent-details-body">
            <div className="workspace-path"><FolderGit2 size={14} /><span title={state.cwd}>{state.cwd || "…"}</span></div>
            {state.additionalDirectories.map((directory) => (
              <div className="workspace-path workspace-extra" key={directory}><Plus size={12} /><span title={directory}>{directory}</span></div>
            ))}
            <div className="safety-row"><ShieldCheck size={14} />{state.readOnly ? "Read only" : "Filesystem confined"}</div>
            <code className="agent-command" title={state.command.join(" ")}>
              {state.transport} · {state.command.join(" ") || "Connecting…"}
            </code>
            {state.mcpServers.length > 0 ? (
              <div className="mcp-summary" title={state.mcpServers.map(({ name, type }) => `${name} (${type})`).join("\n")}>
                {state.mcpServers.length} MCP server{state.mcpServers.length === 1 ? "" : "s"}
                {state.mcpConnections.length > 0 ? ` · ${state.mcpConnections.length} active` : ""}
              </div>
            ) : null}
            {authMethods.length > 0 && state.authStatus ? (
              <AgentAuthControls
                methods={authMethods}
                status={state.authStatus}
                pending={state.pendingAuth}
                error={state.authError}
                canLogout={agentCapabilities?.auth?.logout != null}
                lastResponse={state.lastAuthResponse}
                disabled={state.phase !== "ready"}
                onAuthenticate={authenticate}
                onLogout={() => {
                  if (window.confirm("Sign out of this ACP Agent? Active sessions may behave differently afterward.")) logout();
                }}
              />
            ) : null}
            {state.initialized ? (
              <details className="sidebar-details">
                <summary><Settings2 size={13} /> Capabilities <ChevronDown size={12} /></summary>
                <RawJson label="initialize response" value={state.initialized} />
              </details>
            ) : null}
            {state.stderr ? (
              <details className="sidebar-details logs">
                <summary><ScrollText size={13} /> Agent stderr <ChevronDown size={12} /></summary>
                <pre>{state.stderr}</pre>
              </details>
            ) : null}
            {state.backgroundEvents.length > 0 ? (
              <details className="sidebar-details">
                <summary><ScrollText size={13} /> Background events ({state.backgroundEvents.length}) <ChevronDown size={12} /></summary>
                <RawJson label="non-current session events" value={state.backgroundEvents} />
              </details>
            ) : null}
            {state.mcpActivity.length > 0 ? (
              <details className="sidebar-details">
                <summary><Activity size={13} /> MCP-over-ACP ({state.mcpActivity.length}) <ChevronDown size={12} /></summary>
                <RawJson label="MCP transport activity" value={{ activeConnections: state.mcpConnections, messages: state.mcpActivity }} />
              </details>
            ) : null}
          </div>
        </details>
      </aside>
      {sidebarOpen ? (
        <button
          className="sidebar-backdrop"
          aria-label="Dismiss sidebar overlay"
          onClick={closeMobileSidebar}
        />
      ) : null}

      <main
        className="main-panel"
        onKeyDownCapture={(event) => {
          if (
            event.key.toLowerCase() === "f" &&
            (event.ctrlKey || event.metaKey) &&
            !event.altKey
          ) {
            event.preventDefault();
            toggleThreadSearch();
          }
        }}
      >
        <header className="topbar">
          <div className="thread-heading">
            <div className="thread-agent-icon"><Bot size={16} /></div>
            <div>
              <h1>{state.title ?? (state.session ? "New agent session" : "No active session")}</h1>
              <span>{agent?.title ?? agent?.name ?? "Agent"}{state.cwd ? ` · ${workspaceName(state.cwd)}` : ""}</span>
            </div>
          </div>
          <div className="topbar-actions">
            <div className="session-metrics">
              <span className="live-indicator"><StatusDot phase={state.phase} />{state.running ? "Working" : phaseLabel(state.phase)}</span>
            </div>
            {state.session ? (
              <button
                type="button"
                className={threadSearchOpen ? "topbar-icon-button active" : "topbar-icon-button"}
                aria-label="Search Agent thread"
                aria-keyshortcuts="Control+F Meta+F"
                aria-pressed={threadSearchOpen}
                title="Search thread · Ctrl/⌘F"
                onClick={toggleThreadSearch}
              ><SearchIcon size={15} /></button>
            ) : null}
            {state.session ? (
              <details className="thread-actions">
                <summary role="button" aria-label="Thread actions" title="Thread actions"><Ellipsis size={17} /></summary>
                <div>
                  <button
                    type="button"
                    onClick={openActiveThreadMarkdown}
                  ><FileText size={14} /> Open as Markdown</button>
                  {sessionCapabilities?.fork != null ? (
                    <button type="button" disabled={state.running || transitioning || changingControl || queuedPrompts.length > 0} onClick={forkSession}><GitFork size={14} /> Fork thread</button>
                  ) : null}
                  {sessionCapabilities?.close != null ? (
                    <button type="button" className="danger" disabled={state.running || transitioning || changingControl || queuedPrompts.length > 0} onClick={closeSession}><LogOut size={14} /> Close thread</button>
                  ) : null}
                </div>
              </details>
            ) : null}
          </div>
        </header>

        {threadSearchOpen ? (
          <ThreadSearchBar
            rootRef={scroll}
            containerRef={threadSearchContainer}
            contentVersion={state.timeline}
            terminalVersion={state.terminalSnapshots}
            focusRequest={threadSearchFocusRequest}
            onClose={closeThreadSearch}
          />
        ) : null}

        <div className="thread-body">
          <div
            className="scroll-region"
            ref={scroll}
            role="region"
            aria-label="Conversation thread"
            aria-keyshortcuts="Escape Home End Shift+PageUp Shift+PageDown"
            tabIndex={0}
            onScroll={measureThreadScroll}
            onKeyDown={(event) => {
              if (event.target !== event.currentTarget) return;
              if (event.key === "Escape" && state.running) {
                event.preventDefault();
                setQueuePaused(true);
                cancel();
              } else if (event.key === "Home" && !event.ctrlKey && !event.metaKey) {
                event.preventDefault();
                navigateThread("top");
              } else if (event.key === "End" && !event.ctrlKey && !event.metaKey) {
                event.preventDefault();
                navigateThread("bottom");
              } else if (event.shiftKey && event.key === "PageUp") {
                event.preventDefault();
                navigateThread("previous-message");
              } else if (event.shiftKey && event.key === "PageDown") {
                event.preventDefault();
                navigateThread("next-message");
              }
            }}
          >
            <div className="conversation-wrap">
              {showAuthCard && state.authStatus ? (
                <AgentAuthCard
                  agentName={agent?.title ?? agent?.name ?? "Agent"}
                  methods={authMethods}
                  status={state.authStatus}
                  pending={state.pendingAuth}
                  error={state.authError}
                  focusAction={state.session == null}
                  disabled={state.phase !== "ready"}
                  onAuthenticate={authenticate}
                />
              ) : null}
              {state.authTerminal && authTerminalMethod ? (
                <Suspense fallback={<div className="auth-terminal-loading" role="status">Opening Agent terminal…</div>}>
                  <AuthTerminalCard
                    terminalState={state.authTerminal}
                    method={authTerminalMethod}
                    onInput={writeAuthTerminal}
                    onResize={resizeAuthTerminal}
                    onCancel={cancelAuthTerminal}
                    onRetry={authenticate}
                    onDismiss={dismissAuthTerminal}
                  />
                </Suspense>
              ) : null}
              {(!showAuthCard && !terminalAuthOwnsInteraction) || state.timeline.length > 0 ? <Conversation
                timeline={state.timeline}
                terminalSnapshots={state.terminalSnapshots}
                agentActivity={state.agentActivity}
                onNavigateThread={navigateThread}
                onOpenThreadMarkdown={openActiveThreadMarkdown}
                canReusePrompt={ready && !state.running}
                onReusePrompt={(blocks: ContentBlock[]) => setComposerDraft({
                  id: randomId(),
                  blocks,
                })}
                onRetryPrompt={prompt}
              /> : null}
              {state.running && state.timeline.at(-1)?.type !== "stop" ? (
                <div className="agent-working" role="status">
                  <Activity size={14} /> {agentActivityLabel(state.agentActivity, state.permissions.length > 0 || state.elicitations.length > 0)}<span /><span /><span />
                </div>
              ) : null}
            </div>
          </div>
          {threadScroll.overflow ? (
            <nav className="thread-scroll-controls" aria-label="Thread navigation">
              <button
                type="button"
                aria-label="Jump to top of thread"
                title="Jump to top"
                disabled={threadScroll.atTop}
                onClick={() => navigateThread("top")}
              ><ArrowUpToLine size={14} /></button>
              <button
                type="button"
                aria-label="Jump to bottom of thread"
                title="Jump to bottom"
                disabled={threadScroll.atBottom}
                onClick={() => navigateThread("bottom")}
              ><ArrowDownToLine size={14} /></button>
            </nav>
          ) : null}
        </div>

        <div className="input-dock">
          <div className="input-inner">
            {state.permissions.map((pending) => (
              <PermissionCard
                key={pending.permissionId}
                pending={pending}
                toolCall={findToolCall(state.timeline, pending.request.toolCall.toolCallId)}
                onRespond={(outcome) => respondPermission(pending.permissionId, outcome)}
              />
            ))}
            {state.elicitations.map((pending) => (
              <ElicitationCard
                key={pending.elicitationId}
                pending={pending}
                onRespond={(response) => respondElicitation(pending.elicitationId, response)}
              />
            ))}
            {state.externalFlows.map((flow) => (
              <ExternalFlowCard
                key={flow.elicitationId}
                flow={flow}
                onDismiss={() => dismissExternalFlow(flow.elicitationId)}
              />
            ))}
            {state.activePlan ? (
              <PlanCard entryId={state.activePlan.id} update={state.activePlan.update} />
            ) : null}
            <QueuedPrompts
              prompts={queuedPrompts}
              error={queueError}
              paused={queuePaused}
              canSendNow={ready && state.running}
              onEdit={(queued) => {
                setQueuedPrompts((current) => current.filter(({ id }) => id !== queued.id));
                setQueueError(undefined);
                setComposerDraft({ id: randomId(), blocks: queued.blocks });
              }}
              onRemove={(id) => {
                setQueuedPrompts((current) => current.filter((queued) => queued.id !== id));
                setQueueError(undefined);
              }}
              onClear={() => {
                setQueuedPrompts([]);
                setQueueError(undefined);
                setQueuePaused(false);
              }}
              onSendNow={(id) => {
                if (!state.running) return;
                setQueuedPrompts((current) => {
                  const selected = current.find((queued) => queued.id === id);
                  return selected
                    ? [selected, ...current.filter((queued) => queued.id !== id)]
                    : current;
                });
                setQueuePaused(false);
                cancel();
              }}
            />
            <ChangeReview
              summary={reviewChanges}
              open={reviewOpen}
              onToggle={() => setReviewOpen((open) => !open)}
            />
            <PromptComposer
              key={state.session?.sessionId ?? "no-session"}
              disabled={!ready}
              running={state.running}
              capabilities={agentCapabilities?.promptCapabilities}
              commands={state.availableCommands}
              draft={composerDraft}
              history={promptHistory}
              usage={state.usage}
              interactionPending={
                state.permissions.length > 0 ||
                state.elicitations.length > 0 ||
                state.externalFlows.some(({ status }) => status === "waiting")
              }
              connectionRecovery={state.phase === "error" || state.phase === "stopped"
                ? {
                    message: state.phase === "error"
                      ? "The ACP bridge failed. Reconnect before sending another message."
                      : "The ACP connection stopped. Reconnect before continuing.",
                    onReconnect: reconnect,
                  }
                : undefined}
              sessionControls={(
                <SessionControls
                  options={state.configOptions}
                  modes={state.session?.modes}
                  currentMode={state.modeId}
                  disabled={!ready || state.running}
                  onMode={setMode}
                  onConfig={setConfig}
                />
              )}
              onCancel={() => {
                setQueuePaused(true);
                cancel();
              }}
              onNavigateThread={navigateThread}
              onSearchWorkspaceContext={state.transport === "stdio" ? searchWorkspaceContext : undefined}
              onReadWorkspaceContext={state.transport === "stdio" ? readWorkspaceContext : undefined}
              onSubmit={submitPrompt}
            />
          </div>
        </div>
      </main>
      {newThreadOpen ? (
        <NewSessionDialog
          transport={state.transport}
          defaultCwd={state.defaultCwd}
          disabled={transitioning || state.running || state.phase !== "ready"}
          onCancel={closeNewThread}
          onCreate={(cwd) => {
            if (!newSession(cwd)) return false;
            setNewThreadOpen(false);
            return true;
          }}
        />
      ) : null}
    </div>
  );
}

function openThreadMarkdown(markdown: string): void {
  const blob = new Blob([markdown], { type: "text/markdown;charset=utf-8" });
  const url = URL.createObjectURL(blob);
  const link = document.createElement("a");
  link.href = url;
  link.target = "_blank";
  link.rel = "noopener noreferrer";
  link.style.display = "none";
  document.body.append(link);
  link.click();
  link.remove();
  window.setTimeout(() => URL.revokeObjectURL(url), 60_000);
}

function findToolCall(timeline: TimelineItem[], toolCallId: string): ToolCall | undefined {
  const item = timeline.find(
    (candidate) => candidate.type === "tool" && candidate.call.toolCallId === toolCallId,
  );
  return item?.type === "tool" ? item.call : undefined;
}

function StatusDot({ phase }: { phase: string }) {
  return <span className={`status-dot-small phase-${phase}`} aria-hidden="true" />;
}

function phaseLabel(phase: string): string {
  if (phase === "ready") return "Connected";
  if (phase === "starting") return "Starting process";
  if (phase === "initializing") return "Initializing ACP";
  if (phase === "error") return "Connection error";
  return "Agent stopped";
}

function agentActivityLabel(activity: AgentActivity | undefined, waitingForInput: boolean): string {
  if (waitingForInput) return "Agent is waiting for input";
  if (activity?.kind === "thinking") return "Agent is thinking";
  if (activity?.kind === "tool") return `Running ${activity.title}`;
  if (activity?.kind === "planning") return "Agent is planning";
  if (activity?.kind === "compacting") return "Agent is compacting context";
  if (activity?.kind === "responding") return "Agent is responding";
  return "Agent is working";
}

function workspaceName(path: string): string {
  return path.split(/[\\/]/).filter(Boolean).at(-1) ?? path;
}
