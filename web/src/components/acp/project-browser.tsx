import type { SessionInfo } from "@agentclientprotocol/sdk";
import { ArrowUpRight, FolderOpen, MessageSquare, Plus, RefreshCw, Search, Trash2, X } from "lucide-react";
import { useEffect, useId, useMemo, useState, type MouseEvent, type ReactNode } from "react";
import { filterSessions, groupSessionsByWorkspace } from "../../lib/session-history";
import { projectPath, sessionPath } from "../../lib/session-route";

export type ProjectBrowserProps = {
  sessions: SessionInfo[];
  projectCwd?: string;
  navigation?: ReactNode;
  actions?: ReactNode;
  nextCursor?: string | null;
  canList: boolean;
  canAttach: boolean;
  canDelete: boolean;
  disabled: boolean;
  busySessionIds: string[];
  attentionSessionIds: string[];
  deletingSessionIds: string[];
  openSessionIds: string[];
  onProject: (cwd: string) => void;
  onAttach: (session: SessionInfo) => void;
  onDelete: (sessionId: string) => void;
  onNew: () => void;
  onRefresh: () => void;
  onMore: (cursor: string) => void;
};

export function ProjectBrowser({
  sessions,
  projectCwd,
  navigation,
  actions,
  nextCursor,
  canList,
  canAttach,
  canDelete,
  disabled,
  busySessionIds,
  attentionSessionIds,
  deletingSessionIds,
  openSessionIds,
  onProject,
  onAttach,
  onDelete,
  onNew,
  onRefresh,
  onMore,
}: ProjectBrowserProps) {
  const [query, setQuery] = useState("");
  const id = useId();
  const isProject = projectCwd !== undefined;
  useEffect(() => setQuery(""), [projectCwd]);
  const groups = useMemo(() => groupSessionsByWorkspace(sessions), [sessions]);
  const projectSessions = useMemo(
    () => groups.find(({ cwd }) => cwd === projectCwd)?.sessions ?? [],
    [groups, projectCwd],
  );
  const filteredSessions = useMemo(
    () => filterSessions(isProject ? projectSessions : sessions, query),
    [isProject, projectSessions, sessions, query],
  );
  const filteredWorkspaces = useMemo(() => {
    const matchingPaths = new Set(filteredSessions.map(({ cwd }) => cwd));
    return groups.filter(({ cwd }) => matchingPaths.has(cwd));
  }, [filteredSessions, groups]);
  const filtering = query.trim().length > 0;
  const visibleCount = isProject ? filteredSessions.length : filteredWorkspaces.length;
  const totalCount = isProject ? projectSessions.length : groups.length;
  const countLabel = isProject ? "session" : "project";

  return (
    <section className={`project-browser ${isProject ? "project-detail" : "project-home"}`} aria-labelledby={`${id}-heading`}>
      {navigation ? <div className="project-browser-navigation">{navigation}</div> : null}
      <header className="project-browser-header">
        <div>
          <p className="section-kicker">{isProject ? "Project" : "Workspaces"}</p>
          <h1 id={`${id}-heading`}>{isProject ? workspaceName(projectCwd) : "Projects"}</h1>
          {isProject ? (
            <p className="project-browser-path" title={projectCwd}>{projectCwd || "Unknown workspace"}</p>
          ) : (
            <p className="project-browser-description">Choose a project to continue, or add a working directory.</p>
          )}
        </div>
        <div className="project-browser-actions">
          {canList ? (
            <button type="button" className="project-refresh" disabled={disabled} aria-label="Refresh projects and sessions" onClick={onRefresh}>
              <RefreshCw size={15} aria-hidden="true" />
            </button>
          ) : null}
          <button type="button" className="project-new" disabled={disabled} onClick={onNew}>
            <Plus size={15} aria-hidden="true" />
            {isProject ? "New session in project" : "New project"}
          </button>
          {actions}
        </div>
      </header>

      <div className="project-search" role="search">
        <Search size={16} aria-hidden="true" />
        <input
          type="search"
          value={query}
          maxLength={256}
          autoComplete="off"
          spellCheck="false"
          placeholder={isProject ? "Search sessions" : "Search projects or sessions"}
          aria-label={isProject ? "Search project sessions" : "Search projects"}
          aria-describedby={`${id}-status`}
          onChange={(event) => setQuery(event.currentTarget.value)}
          onKeyDown={(event) => {
            if (event.key === "Escape" && query) {
              event.preventDefault();
              setQuery("");
            }
          }}
        />
        {query ? (
          <button type="button" aria-label="Clear search" onClick={() => setQuery("")}><X size={15} aria-hidden="true" /></button>
        ) : null}
      </div>
      <p className="project-browser-status" id={`${id}-status`} aria-live="polite">
        {filtering
          ? `${visibleCount} of ${totalCount} ${countLabel}${totalCount === 1 ? "" : "s"}`
          : `${totalCount} ${countLabel}${totalCount === 1 ? "" : "s"}`}
        {!isProject ? ` · ${sessions.length} loaded session${sessions.length === 1 ? "" : "s"}` : " loaded"}
      </p>

      {isProject ? (
        <div className="project-session-list">
          {filteredSessions.map((session) => {
            const title = session.title || session.sessionId;
            const busy = busySessionIds.includes(session.sessionId);
            const attention = attentionSessionIds.includes(session.sessionId);
            const deleting = deletingSessionIds.includes(session.sessionId);
            const open = openSessionIds.includes(session.sessionId);
            const unavailable = disabled || deleting || (!open && !canAttach);
            return (
              <div className={`project-session-row${attention ? " attention" : ""}`} key={session.sessionId}>
                <a
                  className="project-session-link"
                  href={sessionPath(session.sessionId, session.cwd)}
                  aria-disabled={unavailable || undefined}
                  tabIndex={unavailable ? -1 : undefined}
                  onClick={(event) => navigate(event, () => onAttach(session), unavailable)}
                >
                  <MessageSquare size={18} aria-hidden="true" />
                  <span className="project-session-details">
                    <strong className="project-session-title">{title}</strong>
                    <span className="project-session-date">{formatSessionDate(session.updatedAt)}</span>
                  </span>
                  {attention || busy ? (
                    <span className={`project-session-status${attention ? " needs-attention" : ""}`}>
                      {attention ? "Input needed" : "Working"}
                    </span>
                  ) : null}
                  <ArrowUpRight size={15} aria-hidden="true" />
                </a>
                {canDelete ? (
                  <button
                    type="button"
                    className="project-session-delete"
                    disabled={disabled || busy || deleting}
                    aria-label={`${deleting ? "Deleting" : "Delete"} ${title}`}
                    title={busy ? "Stop this session before deleting it" : "Delete session"}
                    onClick={() => onDelete(session.sessionId)}
                  ><Trash2 size={15} aria-hidden="true" /></button>
                ) : null}
              </div>
            );
          })}
        </div>
      ) : (
        <div className="project-grid">
          {filteredWorkspaces.map(({ cwd, sessions: items }) => (
            <a
              className="project-card"
              key={cwd}
              href={projectPath(cwd)}
              aria-disabled={!cwd || undefined}
              tabIndex={!cwd ? -1 : undefined}
              onClick={(event) => navigate(event, () => onProject(cwd), !cwd)}
            >
              <span className="project-card-icon"><FolderOpen size={21} aria-hidden="true" /></span>
              <strong className="project-card-title">{workspaceName(cwd)}</strong>
              <span className="project-card-path" title={cwd}>{cwd || "Unknown workspace"}</span>
              <span className="project-card-meta">
                <span>{items.length} loaded session{items.length === 1 ? "" : "s"}</span>
                <span>{formatSessionDate(items[0]?.updatedAt)}</span>
              </span>
              <ArrowUpRight className="project-card-arrow" size={16} aria-hidden="true" />
            </a>
          ))}
        </div>
      )}

      {visibleCount === 0 ? (
        <div className="project-empty">
          <FolderOpen size={28} aria-hidden="true" />
          <h2>{filtering ? `No matching ${countLabel}s` : disabled ? "Loading your workspace…" : isProject ? "No sessions yet" : "Your projects start here"}</h2>
          <p>{filtering
            ? "Try another title or path, or clear your search."
            : disabled
              ? "Your projects and sessions will appear here."
              : isProject
                ? "Start a session in this project to begin."
                : "Add a working directory to create a project and start its first session."}</p>
          {!canList && !filtering && !disabled ? <p>Saved sessions are unavailable from this Agent.</p> : null}
        </div>
      ) : null}
      {nextCursor ? (
        <footer className="project-pagination">
          <p>{isProject ? "More sessions may be available in this project." : "Counts and search cover loaded sessions."}</p>
          <button type="button" disabled={disabled} onClick={() => onMore(nextCursor)}>Load more sessions</button>
        </footer>
      ) : null}
    </section>
  );
}

export function workspaceName(cwd: string): string {
  if (!cwd) return "Unknown workspace";
  return cwd.replace(/[\\/]+$/u, "").split(/[\\/]/u).pop() || cwd;
}

function navigate(event: MouseEvent<HTMLAnchorElement>, callback: () => void, disabled = false): void {
  if (disabled) {
    event.preventDefault();
  } else if (event.button === 0 && !event.metaKey && !event.ctrlKey && !event.shiftKey && !event.altKey) {
    event.preventDefault();
    callback();
  }
}

function formatSessionDate(value: string | null | undefined): string {
  if (!value) return "Date unavailable";
  const date = new Date(value);
  return Number.isNaN(date.valueOf())
    ? "Date unavailable"
    : date.toLocaleString([], { dateStyle: "medium", timeStyle: "short" });
}
