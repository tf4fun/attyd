import type { SessionInfo } from "@agentclientprotocol/sdk";
import type { TFunction } from "i18next";
import { ArrowUpRight, FolderOpen, MessageSquare, Plus, RefreshCw, Search, Trash2, X } from "lucide-react";
import { useEffect, useId, useMemo, useState, type MouseEvent, type ReactNode } from "react";
import { filterSessions, groupSessionsByWorkspace } from "../../lib/session-history";
import { projectPath, sessionPath } from "../../lib/session-route";
import i18n, { useTranslation } from "../../i18n";

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
  const { t, i18n } = useTranslation("workspace");
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

  return (
    <section className={`project-browser ${isProject ? "project-detail" : "project-home"}`} aria-labelledby={`${id}-heading`}>
      {navigation ? <div className="project-browser-navigation">{navigation}</div> : null}
      <header className="project-browser-header">
        <div>
          <p className="section-kicker">{isProject ? t("projects.project") : t("projects.workspaces")}</p>
          <h1 id={`${id}-heading`}>{isProject ? workspaceName(projectCwd, t) : t("projects.title")}</h1>
          {isProject ? (
            <p className="project-browser-path" title={projectCwd}>{projectCwd || t("unknownWorkspace")}</p>
          ) : (
            <p className="project-browser-description">{t("projects.description")}</p>
          )}
        </div>
        <div className="project-browser-actions">
          {canList ? (
            <button type="button" className="project-refresh" disabled={disabled} aria-label={t("projects.refresh")} onClick={onRefresh}>
              <RefreshCw size={15} aria-hidden="true" />
            </button>
          ) : null}
          <button type="button" className="project-new" disabled={disabled} onClick={onNew}>
            <Plus size={15} aria-hidden="true" />
            {isProject ? t("projects.newSession") : t("projects.newProject")}
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
          placeholder={isProject ? t("projects.searchSessions") : t("projects.searchProjectsOrSessions")}
          aria-label={isProject ? t("projects.searchProjectSessions") : t("projects.searchProjects")}
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
          <button type="button" aria-label={t("projects.clearSearch")} onClick={() => setQuery("")}><X size={15} aria-hidden="true" /></button>
        ) : null}
      </div>
      <p className="project-browser-status" id={`${id}-status`} aria-live="polite">
        {isProject
          ? filtering
            ? t("projects.filteredSessions", { count: totalCount, visible: visibleCount })
            : t("projects.loadedSessions", { count: totalCount })
          : t("projects.projectStatus", {
            projects: filtering
              ? t("projects.filteredProjects", { count: totalCount, visible: visibleCount })
              : t("projects.projectCount", { count: totalCount }),
            sessions: t("projects.loadedSessionCount", { count: sessions.length }),
          })}
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
                    <span className="project-session-date">{formatSessionDate(session.updatedAt, t, i18n.resolvedLanguage)}</span>
                  </span>
                  {attention || busy ? (
                    <span className={`project-session-status${attention ? " needs-attention" : ""}`}>
                      {attention ? t("projects.inputNeeded") : t("projects.working")}
                    </span>
                  ) : null}
                  <ArrowUpRight size={15} aria-hidden="true" />
                </a>
                {canDelete ? (
                  <button
                    type="button"
                    className="project-session-delete"
                    disabled={disabled || busy || deleting}
                    aria-label={deleting ? t("deletingSession", { title }) : t("deleteSession", { title })}
                    title={busy ? t("projects.stopBeforeDelete") : t("projects.deleteSession")}
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
              <strong className="project-card-title">{workspaceName(cwd, t)}</strong>
              <span className="project-card-path" title={cwd}>{cwd || t("unknownWorkspace")}</span>
              <span className="project-card-meta">
                <span>{t("projects.loadedSessionCount", { count: items.length })}</span>
                <span>{formatSessionDate(items[0]?.updatedAt, t, i18n.resolvedLanguage)}</span>
              </span>
              <ArrowUpRight className="project-card-arrow" size={16} aria-hidden="true" />
            </a>
          ))}
        </div>
      )}

      {visibleCount === 0 ? (
        <div className="project-empty">
          <FolderOpen size={28} aria-hidden="true" />
          <h2>{filtering
            ? isProject ? t("projects.noMatchingSessions") : t("projects.noMatchingProjects")
            : disabled ? t("projects.loading") : isProject ? t("projects.noSessions") : t("projects.noProjects")}</h2>
          <p>{filtering
            ? t("projects.searchHelp")
            : disabled
              ? t("projects.loadingHelp")
              : isProject
                ? t("projects.newSessionHelp")
                : t("projects.newProjectHelp")}</p>
          {!canList && !filtering && !disabled ? <p>{t("projects.savedSessionsUnavailable")}</p> : null}
        </div>
      ) : null}
      {nextCursor ? (
        <footer className="project-pagination">
          <p>{isProject ? t("projects.moreSessionsHelp") : t("projects.loadedSearchHelp")}</p>
          <button type="button" disabled={disabled} onClick={() => onMore(nextCursor)}>{t("projects.loadMore")}</button>
        </footer>
      ) : null}
    </section>
  );
}

export function workspaceName(cwd: string, t: TFunction<"workspace"> = i18n.getFixedT(null, "workspace")): string {
  if (!cwd) return t("unknownWorkspace");
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

function formatSessionDate(value: string | null | undefined, t: TFunction<"workspace">, language?: string): string {
  if (!value) return t("projects.dateUnavailable");
  const date = new Date(value);
  return Number.isNaN(date.valueOf())
    ? t("projects.dateUnavailable")
    : date.toLocaleString(language, { dateStyle: "medium", timeStyle: "short" });
}
