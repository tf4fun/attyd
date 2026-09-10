import type { TFunction } from "i18next";
import { useTranslation } from "./i18n";
import { translateHistoryNotice } from "./i18n/history-notice";
import { InterfaceSettings } from "./components/interface-settings";
import type { ContentBlock, SessionInfo, ToolCall } from "@agentclientprotocol/sdk";
import {
  Activity,
  ArrowDownToLine,
  ArrowLeft,
  ArrowUpToLine,
  Bot,
  ChevronDown,
  ChevronRight,
  Ellipsis,
  FileText,
  FolderGit2,
  GitFork,
  LogOut,
  Plus,
  Search as SearchIcon,
  ScrollText,
  Settings2,
  ShieldCheck,
  Trash2,
} from "lucide-react";
import { lazy, Suspense, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { AgentAuthCard, AgentAuthControls } from "./components/acp/agent-auth";
import { Conversation } from "./components/acp/conversation";
import { ElicitationCard, ExternalFlowCard } from "./components/acp/elicitation";
import { PermissionCard } from "./components/acp/permission";
import { PlanCard } from "./components/acp/plan";
import { CloseSessionDialog } from "./components/acp/close-session-dialog";
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
import { ProjectBrowser } from "./components/acp/project-browser";
import { projectPath } from "./lib/session-route";
import { randomId } from "./lib/id";
import { collectPromptHistory } from "./lib/prompt-history";
import { timelineToMarkdown } from "./lib/thread-markdown";
import type { AgentActivity, TimelineItem } from "./lib/state";
import { useAcp } from "./lib/use-acp";

const AuthTerminalCard = lazy(() => import("./components/acp/auth-terminal").then(
  ({ AuthTerminalCard: component }) => ({ default: component }),
));

export default function App() {
  const { t } = useTranslation("app");
  const [closingSessionId, setClosingSessionId] = useState<string>();
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
    goHome,
    projectCwd,
    openProject,
    forkSession,
    closeSession,
    deleteSession,
    searchWorkspaceContext,
    readWorkspaceContext,
  } = useAcp();
  const [newThreadOpen, setNewThreadOpen] = useState(false);
  const [composerDraft, setComposerDraft] = useState<ComposerDraft>();
  const [queuedPrompts, setQueuedPrompts] = useState<QueuedPrompt[]>([]);
  const [queueError, setQueueError] = useState<"queue.sessionChanged" | "queue.sendFailed" | "queue.limit">();
  const [queuePaused, setQueuePaused] = useState(false);
  const [threadSearchOpen, setThreadSearchOpen] = useState(false);
  const [threadSearchFocusRequest, setThreadSearchFocusRequest] = useState(0);
  const [threadScroll, setThreadScroll] = useState({
    overflow: false,
    atTop: true,
    atBottom: true,
  });
  const scroll = useRef<HTMLDivElement>(null);
  const threadSearchContainer = useRef<HTMLDivElement>(null);
  const newThreadOpener = useRef<HTMLElement | null>(null);
  const sessionSwitcher = useRef<HTMLDetailsElement>(null);
  const agentSettings = useRef<HTMLDetailsElement>(null);
  const interfaceSettings = useRef<HTMLDetailsElement>(null);
  const threadActions = useRef<HTMLDetailsElement>(null);
  const sessionHeading = useRef<HTMLElement>(null);
  const lastPositionedSession = useRef<string | undefined>(undefined);
  const followLatestOnViewport = useRef(false);
  const followLatestContent = useRef(true);
  const lastThreadScrollTop = useRef(0);

  const pauseThreadFollowing = useCallback(() => {
    followLatestContent.current = false;
    followLatestOnViewport.current = false;
  }, []);

  const canAutoCollapseTurn = useCallback(() => followLatestContent.current, []);

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
    followLatestOnViewport.current = target === "bottom";
    followLatestContent.current = target === "bottom";
    if (target === "top" || target === "bottom") {
      // Boundary controls should be deterministic even while streamed content
      // is changing the scroll height. Smooth scrolling can be interrupted by
      // those layout updates and leave the thread between endpoints.
      const previousBehavior = element.style.scrollBehavior;
      element.style.scrollBehavior = "auto";
      element.scrollTop = target === "top"
        ? 0
        : Math.max(0, element.scrollHeight - element.clientHeight);
      requestAnimationFrame(() => {
        element.style.scrollBehavior = previousBehavior;
        measureThreadScroll();
      });
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
    const entries = [...element.querySelectorAll<HTMLElement>(selector)]
      .filter((entry) => entry.getClientRects().length > 0);
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
    followLatestContent.current = true;
    followLatestOnViewport.current = true;

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

    let frame = 0;
    const composerOwnsFocus = () =>
      document.activeElement instanceof Element &&
      document.activeElement.closest(".composer") != null;
    const moveToLatest = () => {
      if (!followLatestOnViewport.current || !composerOwnsFocus()) return;
      element.scrollTop = Math.max(0, element.scrollHeight - element.clientHeight);
      measureThreadScroll();
    };
    const followViewport = () => {
      if (!followLatestOnViewport.current || !composerOwnsFocus()) return;
      cancelAnimationFrame(frame);
      frame = requestAnimationFrame(() => {
        moveToLatest();
        frame = requestAnimationFrame(moveToLatest);
      });
    };
    const rememberPosition = (event: FocusEvent) => {
      if (!(event.target instanceof Element) || event.target.closest(".composer") == null) return;
      const distance = element.scrollHeight - element.scrollTop - element.clientHeight;
      followLatestOnViewport.current = distance <= 2;
      followViewport();
    };
    const stopFollowing = () => {
      followLatestOnViewport.current = false;
      followLatestContent.current = false;
      cancelAnimationFrame(frame);
    };
    const scrollsThreadUp = (target: EventTarget | null) => {
      for (let node = target instanceof Element ? target : null;
        node && node !== element; node = node.parentElement) {
        if (node.scrollTop > 0 && /^(auto|scroll)$/.test(getComputedStyle(node).overflowY)) {
          return false;
        }
      }
      return element.scrollTop > 0;
    };
    const handleWheel = (event: WheelEvent) => {
      if (!event.defaultPrevented && event.deltaY < 0 && scrollsThreadUp(event.target)) stopFollowing();
    };
    let touchY: number | undefined;
    const rememberTouch = (event: TouchEvent) => {
      touchY = event.touches.length === 1 ? event.touches[0].clientY : undefined;
    };
    const handleTouchMove = (event: TouchEvent) => {
      const nextY = event.touches.length === 1 ? event.touches[0].clientY : undefined;
      if (!event.defaultPrevented && touchY != null && nextY != null && nextY > touchY &&
        scrollsThreadUp(event.target)) stopFollowing();
      touchY = nextY;
    };

    document.addEventListener("focusin", rememberPosition);
    element.addEventListener("touchstart", rememberTouch, { passive: true });
    element.addEventListener("touchmove", handleTouchMove, { passive: true });
    element.addEventListener("touchend", rememberTouch, { passive: true });
    element.addEventListener("touchcancel", rememberTouch, { passive: true });
    element.addEventListener("wheel", handleWheel, { passive: true });
    window.addEventListener("resize", followViewport);
    window.visualViewport?.addEventListener("resize", followViewport);
    window.visualViewport?.addEventListener("scroll", followViewport);
    return () => {
      cancelAnimationFrame(frame);
      document.removeEventListener("focusin", rememberPosition);
      element.removeEventListener("touchstart", rememberTouch);
      element.removeEventListener("touchmove", handleTouchMove);
      element.removeEventListener("touchend", rememberTouch);
      element.removeEventListener("touchcancel", rememberTouch);
      element.removeEventListener("wheel", handleWheel);
      window.removeEventListener("resize", followViewport);
      window.visualViewport?.removeEventListener("resize", followViewport);
      window.visualViewport?.removeEventListener("scroll", followViewport);
    };
  }, [measureThreadScroll, state.session?.sessionId]);

  const closeNewThread = useCallback(() => {
    setNewThreadOpen(false);
    requestAnimationFrame(() => {
      if (newThreadOpener.current?.isConnected) newThreadOpener.current.focus();
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
    const popovers = () => [sessionSwitcher.current, agentSettings.current, interfaceSettings.current, threadActions.current];
    const fitPopovers = () => {
      const viewport = window.visualViewport;
      const bottom = viewport ? viewport.offsetTop + viewport.height : window.innerHeight;
      for (const popover of popovers()) {
        if (!popover?.open) continue;
        const panel = popover.querySelector<HTMLElement>(":scope > div");
        if (!panel) continue;
        popover.style.setProperty("--menu-space", `${Math.max(0, bottom - panel.getBoundingClientRect().top - 16)}px`);
      }
    };
    const onToggle = (event: Event) => {
      const opened = popovers().find((popover) => popover === event.target);
      if (!opened?.open) return;
      for (const popover of popovers()) {
        if (popover && popover !== opened) popover.open = false;
      }
      fitPopovers();
    };
    const closeOnEscape = (event: KeyboardEvent) => {
      if (event.key !== "Escape" || event.defaultPrevented) return;
      const open = popovers().find((popover) => popover?.open);
      if (!open) return;
      event.preventDefault();
      open.open = false;
      open.querySelector<HTMLElement>("summary")?.focus();
    };
    const closeOutside = (event: PointerEvent) => {
      for (const popover of popovers()) {
        if (popover?.open && event.target instanceof Node && !popover.contains(event.target)) {
          popover.open = false;
        }
      }
    };
    window.addEventListener("keydown", closeOnEscape);
    window.addEventListener("pointerdown", closeOutside);
    window.addEventListener("toggle", onToggle, true);
    window.addEventListener("resize", fitPopovers);
    window.addEventListener("scroll", fitPopovers, true);
    window.visualViewport?.addEventListener("resize", fitPopovers);
    window.visualViewport?.addEventListener("scroll", fitPopovers);
    return () => {
      window.removeEventListener("keydown", closeOnEscape);
      window.removeEventListener("pointerdown", closeOutside);
      window.removeEventListener("toggle", onToggle, true);
      window.removeEventListener("resize", fitPopovers);
      window.removeEventListener("scroll", fitPopovers, true);
      window.visualViewport?.removeEventListener("resize", fitPopovers);
      window.visualViewport?.removeEventListener("scroll", fitPopovers);
    };
  }, []);

  useLayoutEffect(() => {
    const heading = sessionHeading.current;
    if (!heading) return;
    const updateComposerBoundary = () => {
      heading.parentElement?.style.setProperty(
        "--session-heading-end", `${heading.offsetTop + heading.offsetHeight + 14}px`,
      );
    };
    updateComposerBoundary();
    if (typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(updateComposerBoundary);
    observer.observe(heading);
    return () => observer.disconnect();
  }, [state.session?.sessionId]);

  useLayoutEffect(() => {
    const element = scroll.current;
    if (!element) return;
    if (followLatestContent.current) {
      element.scrollTop = Math.max(0, element.scrollHeight - element.clientHeight);
    }
    const frame = requestAnimationFrame(measureThreadScroll);
    return () => cancelAnimationFrame(frame);
  }, [measureThreadScroll, state.timeline, state.permissions, state.elicitations]);

  const handleThreadScroll = useCallback(() => {
    const element = scroll.current;
    if (!element) return;
    if (element.scrollHeight - element.scrollTop - element.clientHeight <= 2) {
      followLatestContent.current = true;
      followLatestOnViewport.current = true;
    } else if (element.scrollTop < lastThreadScrollTop.current) {
      // Also respect navigation initiated by the browser or assistive tools.
      pauseThreadFollowing();
    }
    lastThreadScrollTop.current = element.scrollTop;
    measureThreadScroll();
  }, [measureThreadScroll, pauseThreadFollowing]);

  useEffect(() => {
    const element = scroll.current;
    if (!element) return;
    const syncLayout = () => {
      // Child disclosures, media, and composer resizing can change the bottom
      // without producing a new ACP timeline event.
      if (followLatestContent.current) {
        element.scrollTop = Math.max(0, element.scrollHeight - element.clientHeight);
      }
      measureThreadScroll();
    };
    syncLayout();
    if (typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(syncLayout);
    observer.observe(element, { box: "border-box" });
    const content = element.querySelector(".conversation-wrap");
    if (content) observer.observe(content, { box: "border-box" });
    return () => observer.disconnect();
  }, [measureThreadScroll, state.session?.sessionId]);

  useEffect(() => {
    setClosingSessionId(undefined);
    setComposerDraft(undefined);
    setQueuedPrompts([]);
    setQueueError(undefined);
    setQueuePaused(false);
    setThreadSearchOpen(false);
  }, [state.session?.sessionId]);

  useEffect(() => {
    if (state.phase !== "stopped" && state.phase !== "error") return;
    setQueuedPrompts([]);
    setQueueError(undefined);
    setQueuePaused(false);
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
  const ready = state.phase === "ready" && state.sessionSyncPhase === "ready" && state.session != null && !transitioning && !changingControl && state.runtimeOperation == null && !authBlocksCurrent;
  const composerAvailable = state.phase === "ready" && state.session != null &&
    !transitioning && !changingControl && state.runtimeOperation == null && !authBlocksCurrent &&
    (state.sessionSyncPhase === "ready" || state.running);
  const deletingCurrentSession = state.pendingSessionDeletions.some(
    ({ sessionId }) => sessionId === state.session?.sessionId,
  );
  const terminalAuthOwnsInteraction = state.authTerminal?.status === "starting" ||
    state.authTerminal?.status === "running" ||
    state.authTerminal?.status === "succeeded";
  const showAuthCard = authMethods.length > 0 &&
    (state.authStatus === "required" || state.authStatus === "logged_out") &&
    !terminalAuthOwnsInteraction;
  const promptHistory = useMemo(() => collectPromptHistory(state.timeline), [state.timeline]);
  const selectedProjectCwd = state.session ? state.cwd : projectCwd;
  const knownSessions = useMemo(() => {
    const sessions = new Map(state.sessions.map((session) => [session.sessionId, session]));
    for (const [sessionId, cached] of state.cachedSessions) {
      if (!sessions.has(sessionId)) sessions.set(sessionId, {
        sessionId, cwd: cached.cwd, title: cached.title,
      });
    }
    if (state.session) sessions.set(state.session.sessionId, {
      ...sessions.get(state.session.sessionId),
      sessionId: state.session.sessionId, cwd: state.cwd, title: state.title,
    });
    return [...sessions.values()];
  }, [state.sessions, state.cachedSessions, state.session, state.cwd, state.title]);
  const projectSessions = useMemo(() => knownSessions.filter(
    (session) => session.cwd === selectedProjectCwd,
  ), [knownSessions, selectedProjectCwd]);
  const busySessionIds = [
    ...((state.running || state.runtimeOperation != null) && state.session ? [state.session.sessionId] : []),
    ...[...state.cachedSessions].filter(([, session]) => session.running || session.runtimeOperation != null)
      .map(([sessionId]) => sessionId),
  ];
  const navigationDisabled = transitioning || changingControl || authBlocksCurrent || queuedPrompts.length > 0;
  const browseProject = (cwd: string) => {
    if (sessionSwitcher.current) sessionSwitcher.current.open = false;
    openProject(cwd);
  };
  const browseHome = () => {
    if (sessionSwitcher.current) sessionSwitcher.current.open = false;
    goHome();
  };
  const openThread = (session: SessionInfo) => {
    if (sessionSwitcher.current) sessionSwitcher.current.open = false;
    attachSession(session);
  };
  const openNewThread = () => {
    newThreadOpener.current = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    if (sessionSwitcher.current) sessionSwitcher.current.open = false;
    if (agentSettings.current) agentSettings.current.open = false;
    if (interfaceSettings.current) interfaceSettings.current.open = false;
    setNewThreadOpen(true);
  };
  const authContent = <>
    {showAuthCard && state.authStatus ? (
      <AgentAuthCard
        agentName={agent?.title ?? agent?.name ?? t("agent")}
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
      <Suspense fallback={<div className="auth-terminal-loading" role="status">{t("openingTerminal")}</div>}>
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
  </>;
  const openActiveThreadMarkdown = useCallback(() => {
    openThreadMarkdown(timelineToMarkdown(state.timeline, {
      title: state.title ?? t("agentThread"),
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
    t,
  ]);
  const requestSessionDeletion = useCallback((sessionId: string) => {
    if (window.confirm(t("deleteConfirm"))) {
      deleteSession(sessionId);
    }
  }, [deleteSession, t]);

  useEffect(() => {
    if (!ready || state.running || queuePaused || queuedPrompts.length === 0) return;
    const next = queuedPrompts[0];
    if (next.sessionId !== state.session?.sessionId) {
      setQueuedPrompts((current) => current.filter(({ id }) => id !== next.id));
      setQueueError("queue.sessionChanged");
      return;
    }
    if (!prompt(next.blocks)) {
      setQueueError("queue.sendFailed");
      return;
    }
    setQueuedPrompts((current) => current[0]?.id === next.id
      ? current.slice(1)
      : current.filter(({ id }) => id !== next.id));
    setQueueError(undefined);
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
    // Capture the position before sending changes the timeline/composer layout.
    const element = scroll.current;
    if (element) {
      const atBottom = element.scrollHeight - element.scrollTop - element.clientHeight <= 2;
      followLatestContent.current = atBottom;
      followLatestOnViewport.current = atBottom;
    }
    setComposerDraft(undefined);
    if (!state.running) {
      setQueuePaused(false);
      return prompt(blocks);
    }
    const sessionId = state.session?.sessionId;
    if (!sessionId) return false;
    if (queuedPrompts.length >= MAX_QUEUED_PROMPTS) {
      setQueueError("queue.limit");
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

  const parentNavigation = selectedProjectCwd != null ? (
    <div className="page-navigation">
      <a
        className="page-back"
        href={state.session ? projectPath(selectedProjectCwd) : "/"}
        aria-label={state.session ? t("backProject") : t("backProjects")}
        title={state.session ? t("backProject") : t("backProjects")}
        onClick={(event) => {
          if (event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
          event.preventDefault();
          if (state.session) browseProject(selectedProjectCwd);
          else browseHome();
        }}
      ><ArrowLeft size={18} aria-hidden="true" /></a>
      <nav className="workspace-breadcrumbs" aria-label={t("breadcrumb")}>
        <a href="/" aria-label={t("allProjects")} onClick={(event) => {
          if (event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
          event.preventDefault();
          browseHome();
        }}>{t("projects")}</a>
        {selectedProjectCwd != null ? <>
          <ChevronRight size={12} aria-hidden="true" />
          <a href={projectPath(selectedProjectCwd)} aria-label={t("projectSessions")} title={selectedProjectCwd}
            aria-current={!state.session ? "page" : undefined} onClick={(event) => {
              if (event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
              event.preventDefault();
              browseProject(selectedProjectCwd);
            }}>{workspaceName(selectedProjectCwd)}</a>
        </> : null}
      </nav>
    </div>
  ) : null;
  const agentSettingsMenu = (
    <details className="agent-details" ref={agentSettings} onToggle={(event) => {
      if (event.currentTarget.open && sessionSwitcher.current) sessionSwitcher.current.open = false;
    }}>
      <summary role="button" aria-label={t("agentSettings")} title={t("agentSettings")}>
        <StatusDot phase={state.phase} /><Bot size={17} />
      </summary>
      <div className="agent-details-body">
        <div className="agent-settings-heading">
          <strong>{agent?.title ?? agent?.name ?? t("startingAgent")}</strong>
          <small>{phaseLabel(state.phase, t)} · ACP v{state.initialized?.protocolVersion ?? "–"}</small>
        </div>
        <div className="workspace-path"><FolderGit2 size={14} /><span title={state.cwd}>{state.cwd || "…"}</span></div>
        {state.additionalDirectories.map((directory) => (
          <div className="workspace-path workspace-extra" key={directory}><Plus size={12} /><span title={directory}>{directory}</span></div>
        ))}
        <div className="safety-row"><ShieldCheck size={14} />{state.readOnly ? t("readOnly") : t("filesystemConfined")}</div>
        <code className="agent-command" title={state.command.join(" ")}>
          {state.transport} · {state.command.join(" ") || t("connecting")}
        </code>
        {state.mcpServers.length > 0 ? (
          <div className="mcp-summary" title={state.mcpServers.map(({ name, type }) => `${name} (${type})`).join("\n")}>
            {t("mcpServers", { count: state.mcpServers.length })}
            {state.mcpConnections.length > 0 ? ` · ${t("mcpActive", { count: state.mcpConnections.length })}` : ""}
          </div>
        ) : null}
        {(authMethods.length > 0 || agentCapabilities?.auth?.logout != null) && state.authStatus ? (
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
              if (window.confirm(t("signOutConfirm"))) logout();
            }}
          />
        ) : null}
        {state.initialized ? (
          <details className="sidebar-details">
            <summary><Settings2 size={13} /> {t("capabilities")} <ChevronDown size={12} /></summary>
            <RawJson label={t("initializeResponse")} value={state.initialized} />
          </details>
        ) : null}
        {state.stderr ? (
          <details className="sidebar-details logs">
            <summary><ScrollText size={13} /> {t("stderr")} <ChevronDown size={12} /></summary>
            <pre>{state.stderr}</pre>
          </details>
        ) : null}
        {state.backgroundEvents.length > 0 ? (
          <details className="sidebar-details">
            <summary><ScrollText size={13} /> {t("backgroundEvents", { count: state.backgroundEvents.length })} <ChevronDown size={12} /></summary>
            <RawJson label={t("otherSessionEvents")} value={state.backgroundEvents} />
          </details>
        ) : null}
        {state.mcpActivity.length > 0 ? (
          <details className="sidebar-details">
            <summary><Activity size={13} /> MCP-over-ACP ({state.mcpActivity.length}) <ChevronDown size={12} /></summary>
            <RawJson label={t("mcpActivity")} value={{ activeConnections: state.mcpConnections, messages: state.mcpActivity }} />
          </details>
        ) : null}
      </div>
    </details>
  );

  return (
    <div className={`app-shell ${state.session ? "session-page" : "browse-page"}`}>
      <main
        className="main-panel"
        onKeyDownCapture={(event) => {
          if (
            state.session &&
            !(event.target instanceof Element && event.target.closest(".session-switcher, .agent-details, .interface-settings")) &&
            event.key.toLowerCase() === "f" &&
            (event.ctrlKey || event.metaKey) &&
            !event.altKey
          ) {
            event.preventDefault();
            toggleThreadSearch();
          }
        }}
      >
        {state.session ? (
          <header className="session-header" ref={sessionHeading}>
            {parentNavigation}
            <div className="session-header-main">
              <div className="thread-heading">
                <div className="thread-agent-icon"><Bot size={16} /></div>
                <div>
                  <div className="session-title-row">
                    <h1>{state.title ?? t("newSession")}</h1>
                    <details className="session-switcher" ref={sessionSwitcher} onToggle={(event) => {
                      if (!event.currentTarget.open) return;
                      if (agentSettings.current) agentSettings.current.open = false;
                      sessionSwitcher.current?.querySelector<HTMLInputElement>("input")?.focus();
                    }}>
                      <summary role="button" aria-label={t("switchSession")} title={t("switchSession")}>
                        <ChevronDown size={14} />
                      </summary>
                      <div className="session-switcher-panel" role="dialog" aria-label={t("projectSessions")}>
                        <SessionHistory
                          key={selectedProjectCwd}
                          sessions={projectSessions}
                          activeSessionId={state.session?.sessionId}
                          activeTitle={state.title ?? (state.session ? t("newSession") : undefined)}
                          activeCwd={state.cwd}
                          canList={sessionCapabilities?.list != null}
                          nextCursor={state.nextSessionCursor}
                          canAttach={Boolean(agentCapabilities?.loadSession || sessionCapabilities?.resume != null)}
                          canDelete={sessionCapabilities?.delete != null}
                          deletingSessionIds={state.pendingSessionDeletions.map(({ sessionId }) => sessionId)}
                          busySessionIds={busySessionIds}
                          attentionSessionIds={state.attentionSessionIds}
                          openSessionIds={[...state.cachedSessions.keys()]}
                          disabled={navigationDisabled}
                          onAttach={openThread}
                          onDelete={requestSessionDeletion}
                          onRefresh={() => listSessions()}
                          onMore={(cursor) => listSessions(cursor)}
                        />
                      </div>
                    </details>
                  </div>
                  <span>{agent?.title ?? agent?.name ?? t("agent")}{state.cwd ? ` · ${workspaceName(state.cwd)}` : ""}</span>
                </div>
              </div>
              <div className="session-header-actions page-actions">
                {state.session ? (
                  <button type="button" className="page-icon-button" aria-label={t("newThread")} title={t("newThread")}
                    disabled={state.phase !== "ready" || navigationDisabled || authBlocksNewSession}
                    onClick={openNewThread}><Plus size={16} /></button>
                ) : null}
                {state.session ? (
                  <button
                    type="button"
                    className={threadSearchOpen ? "page-icon-button active" : "page-icon-button"}
                    aria-label={t("searchThread")}
                    aria-keyshortcuts="Control+F Meta+F"
                    aria-pressed={threadSearchOpen}
                    title={t("searchShortcut")}
                    onClick={toggleThreadSearch}
                  ><SearchIcon size={15} /></button>
                ) : null}
                {state.session ? (
                  <details className="thread-actions" ref={threadActions}>
                    <summary role="button" aria-label={t("threadActions")} title={t("threadActions")}><Ellipsis size={17} /></summary>
                    <div>
                      <button
                        type="button"
                        onClick={openActiveThreadMarkdown}
                      ><FileText size={14} /> {t("openMarkdown")}</button>
                      {sessionCapabilities?.fork != null ? (
                        <button type="button" disabled={state.running || transitioning || changingControl || queuedPrompts.length > 0} onClick={forkSession}><GitFork size={14} /> {t("forkThread")}</button>
                      ) : null}
                      {sessionCapabilities?.close != null ? (
                        <button type="button" className="danger" disabled={transitioning || changingControl || state.runtimeOperation != null} onClick={() => setClosingSessionId(state.session?.sessionId)}><LogOut size={14} /> {t("closeThread")}</button>
                      ) : null}
                      {sessionCapabilities?.delete != null ? (
                        <button
                          type="button"
                          className="danger"
                          disabled={state.running || transitioning || changingControl || deletingCurrentSession || queuedPrompts.length > 0}
                          onClick={() => state.session && requestSessionDeletion(state.session.sessionId)}
                        ><Trash2 size={14} /> {t("deleteThread")}</button>
                      ) : null}
                    </div>
                  </details>
                ) : null}
                {agentSettingsMenu}
                <InterfaceSettings menuRef={interfaceSettings} />
              </div>
            </div>
          </header>
        ) : null}

        {state.session && threadSearchOpen ? (
          <ThreadSearchBar
            rootRef={scroll}
            containerRef={threadSearchContainer}
            contentVersion={state.timeline}
            terminalVersion={state.terminalSnapshots}
            focusRequest={threadSearchFocusRequest}
            onNavigate={pauseThreadFollowing}
            onClose={closeThreadSearch}
          />
        ) : null}

        {!state.session ? <div className="browse-panel">
          {authContent}
          {state.elicitations.map((pending) => (
            <ElicitationCard
              key={pending.elicitationId}
              agentName={agent?.title ?? agent?.name ?? t("agent")}
              pending={pending}
              onRespond={(response) => respondElicitation(pending.elicitationId, response)}
            />
          ))}
          {state.externalFlows.map((flow) => (
            <ExternalFlowCard key={flow.elicitationId} flow={flow}
              onDismiss={() => dismissExternalFlow(flow.elicitationId)} />
          ))}
          {state.timeline.some((item) => item.type === "error") ? (
            <Conversation timeline={state.timeline.filter((item) => item.type === "error")} />
          ) : null}
          {state.phase === "stopped" || state.phase === "error" ? (
            <div className="project-connection-error" role="status">
              <span>{t("connectionInterrupted")}</span><button type="button" onClick={reconnect}>{t("reconnect")}</button>
            </div>
          ) : null}
          <ProjectBrowser
            navigation={parentNavigation}
            actions={<>{agentSettingsMenu}<InterfaceSettings menuRef={interfaceSettings} /></>}
            sessions={knownSessions}
            projectCwd={selectedProjectCwd}
            nextCursor={state.nextSessionCursor}
            canList={sessionCapabilities?.list != null}
            canAttach={Boolean(agentCapabilities?.loadSession || sessionCapabilities?.resume != null)}
            canDelete={sessionCapabilities?.delete != null}
            disabled={navigationDisabled || state.phase !== "ready" || authBlocksNewSession}
            busySessionIds={busySessionIds}
            attentionSessionIds={state.attentionSessionIds}
            deletingSessionIds={state.pendingSessionDeletions.map(({ sessionId }) => sessionId)}
            openSessionIds={[...state.cachedSessions.keys()]}
            onProject={browseProject}
            onAttach={openThread}
            onDelete={requestSessionDeletion}
            onNew={openNewThread}
            onRefresh={() => listSessions()}
            onMore={(cursor) => listSessions(cursor)}
          />
        </div> : <>
        <div className="thread-body">
          <div
            className="scroll-region"
            ref={scroll}
            role="region"
            aria-label={t("conversationThread")}
            aria-keyshortcuts="Escape Home End Shift+PageUp Shift+PageDown"
            tabIndex={0}
            onScroll={handleThreadScroll}
            onPointerDown={(event) => {
              const bounds = event.currentTarget.getBoundingClientRect();
              if (event.clientX < bounds.right - 20) return;
              followLatestContent.current = false;
              followLatestOnViewport.current = false;
            }}
            onKeyDown={(event) => {
              if (event.target !== event.currentTarget) return;
              if (
                event.key === "ArrowUp" ||
                event.key === "PageUp" ||
                (event.key === " " && event.shiftKey)
              ) {
                followLatestContent.current = false;
                followLatestOnViewport.current = false;
              }
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
              {authContent}
              {state.historyNotice ? <p className="history-notice" role="status">{translateHistoryNotice(state.historyNotice, t)}</p> : null}
              {(!showAuthCard && !terminalAuthOwnsInteraction) || state.timeline.length > 0 ? <Conversation
                key={state.session?.sessionId}
                timeline={state.timeline}
                settled={!state.running && state.sessionSyncPhase === "ready" && !state.sessionTransition}
                atBottom={threadScroll.atBottom}
                canAutoCollapse={canAutoCollapseTurn}
                onProcessToggle={pauseThreadFollowing}
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
                  <Activity size={14} /> {agentActivityLabel(state.agentActivity, state.permissions.length > 0 || state.elicitations.length > 0, t)}<span /><span /><span />
                </div>
              ) : null}
            </div>
          </div>
          {threadScroll.overflow ? (
            <nav className="thread-scroll-controls" aria-label={t("threadNavigation")}>
              <button
                type="button"
                aria-label={t("jumpTopThread")}
                title={t("jumpTop")}
                disabled={threadScroll.atTop}
                onClick={() => navigateThread("top")}
              ><ArrowUpToLine size={14} /></button>
              <button
                type="button"
                aria-label={t("jumpBottomThread")}
                title={t("jumpBottom")}
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
                agentName={agent?.title ?? agent?.name ?? t("agent")}
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
              error={queueError ? t(queueError, { count: MAX_QUEUED_PROMPTS }) : undefined}
              paused={queuePaused}
              canSendNow={composerAvailable && state.running}
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
            <PromptComposer
              key={state.session?.sessionId ?? "no-session"}
              disabled={!composerAvailable}
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
                      ? t("bridgeFailed")
                      : t("connectionStopped"),
                    onReconnect: reconnect,
                  }
                : undefined}
              sessionControls={(
                <SessionControls
                  options={state.configOptions}
                  modes={state.session?.modes}
                  currentMode={state.modeId}
                  disabled={state.phase !== "ready" || !state.session || transitioning || changingControl || state.runtimeOperation != null || authBlocksCurrent || !["ready", "running", "reconciling"].includes(state.sessionSyncPhase ?? "")}
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
        </>}
      </main>
      {closingSessionId && closingSessionId === state.session?.sessionId ? (
        <CloseSessionDialog
          disabled={transitioning || changingControl || state.runtimeOperation != null}
          onCancel={() => setClosingSessionId(undefined)}
          onConfirm={() => {
            setClosingSessionId(undefined);
            setQueuePaused(true);
            closeSession();
          }}
        />
      ) : null}
      {newThreadOpen ? (
        <NewSessionDialog
          transport={state.transport}
          purpose={selectedProjectCwd == null ? "project" : "session"}
          defaultCwd={selectedProjectCwd ?? state.defaultCwd}
          disabled={transitioning || state.phase !== "ready"}
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

function phaseLabel(phase: string, t: TFunction<"app">): string {
  if (phase === "ready") return t("phase.ready");
  if (phase === "starting") return t("phase.starting");
  if (phase === "initializing") return t("phase.initializing");
  if (phase === "error") return t("phase.error");
  return t("phase.stopped");
}

function agentActivityLabel(activity: AgentActivity | undefined, waitingForInput: boolean, t: TFunction<"app">): string {
  if (waitingForInput) return t("activity.waiting");
  if (activity?.kind === "thinking") return t("activity.thinking");
  if (activity?.kind === "tool") return t("activity.tool", { title: activity.title });
  if (activity?.kind === "planning") return t("activity.planning");
  if (activity?.kind === "compacting") return t("activity.compacting");
  if (activity?.kind === "responding") return t("activity.responding");
  return t("activity.working");
}

function workspaceName(path: string): string {
  return path.split(/[\\/]/).filter(Boolean).at(-1) ?? path;
}
