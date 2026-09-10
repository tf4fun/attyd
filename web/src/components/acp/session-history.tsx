import type { SessionInfo } from "@agentclientprotocol/sdk";
import type { TFunction } from "i18next";
import { FolderOpen, History, RefreshCw, Search, Trash2, X } from "lucide-react";
import { useMemo, useState } from "react";
import { useTranslation } from "../../i18n";
import {
  filterSessions,
  groupSessionsByWorkspace,
} from "../../lib/session-history";

export function SessionHistory({
  sessions,
  activeSessionId,
  activeTitle,
  activeCwd,
  canList,
  nextCursor,
  canAttach,
  canDelete,
  deletingSessionIds,
  busySessionIds = [],
  attentionSessionIds = [],
  openSessionIds = [],
  disabled,
  onAttach,
  onDelete,
  onRefresh,
  onMore,
}: {
  sessions: SessionInfo[];
  activeSessionId?: string;
  activeTitle?: string;
  activeCwd?: string;
  canList: boolean;
  nextCursor?: string | null;
  canAttach: boolean;
  canDelete: boolean;
  deletingSessionIds: string[];
  busySessionIds?: string[];
  attentionSessionIds?: string[];
  openSessionIds?: string[];
  disabled: boolean;
  onAttach: (session: SessionInfo) => void;
  onDelete: (sessionId: string) => void;
  onRefresh: () => void;
  onMore: (cursor: string) => void;
}) {
  const { t } = useTranslation("workspace");
  const [query, setQuery] = useState("");
  const activeSession = useMemo(() => {
    if (!activeSessionId) return undefined;
    const reported = sessions.find(({ sessionId }) => sessionId === activeSessionId);
    return {
      sessionId: activeSessionId,
      cwd: activeCwd || reported?.cwd || "",
      additionalDirectories: reported?.additionalDirectories,
      title: activeTitle ?? reported?.title,
      updatedAt: reported?.updatedAt,
    } satisfies SessionInfo;
  }, [activeSessionId, activeTitle, activeCwd, sessions]);
  const displayedSessions = useMemo(() => {
    if (!activeSession) return sessions;
    const listed = sessions.some(({ sessionId }) => sessionId === activeSession.sessionId);
    return listed
      ? sessions.map((session) => session.sessionId === activeSession.sessionId ? activeSession : session)
      : [activeSession, ...sessions];
  }, [activeSession, sessions]);
  const filteredSessions = useMemo(
    () => filterSessions(displayedSessions, query),
    [displayedSessions, query],
  );
  const groups = useMemo(
    () => groupSessionsByWorkspace(filteredSessions),
    [filteredSessions],
  );
  const totalCount = displayedSessions.length;
  const visibleCount = filteredSessions.length;
  const filtering = query.trim().length > 0;

  return (
    <section className="sidebar-section session-history" aria-labelledby="threads-heading">
      <div className="section-heading">
        <span className="section-kicker" id="threads-heading">{t("history.title")}</span>
        {canList ? (
          <button type="button" aria-label={t("history.refresh")} onClick={onRefresh}>
            <RefreshCw size={12} />
          </button>
        ) : null}
      </div>
      {canList ? (
        <div className="session-filter" role="search">
          <Search size={12} aria-hidden="true" />
          <input
            type="search"
            value={query}
            maxLength={256}
            autoComplete="off"
            spellCheck="false"
            placeholder={t("history.filter")}
            aria-label={t("history.filterLoaded")}
            aria-describedby="session-filter-status"
            onChange={(event) => setQuery(event.currentTarget.value)}
            onKeyDown={(event) => {
              if (event.key === "Escape" && query) {
                event.preventDefault();
                setQuery("");
              }
            }}
          />
          {query ? (
            <button type="button" className="session-filter-clear" aria-label={t("history.clearFilter")} onClick={() => setQuery("")}>
              <X size={11} />
            </button>
          ) : null}
        </div>
      ) : null}
      <p className="session-filter-status" id="session-filter-status" aria-live="polite">
        {filtering
          ? t("history.filteredThreads", { count: totalCount, visible: visibleCount })
          : t("history.loadedThreads", { count: totalCount })}
      </p>
      <div className="session-list">
        {groups.map((group) => (
          <div className="session-group" key={group.cwd}>
            <h3 className="session-group-label" title={group.cwd || undefined}>
              <FolderOpen size={12} aria-hidden="true" />
              <span>{group.cwd || t("unknownWorkspace")}</span>
            </h3>
            {group.sessions.map((session) => (
              <SessionRow
                key={session.sessionId}
                session={session}
                active={session.sessionId === activeSessionId}
                busy={busySessionIds.includes(session.sessionId)}
                attention={attentionSessionIds.includes(session.sessionId)}
                deleting={deletingSessionIds.includes(session.sessionId)}
                open={openSessionIds.includes(session.sessionId)}
                disabled={disabled}
                canAttach={canAttach}
                canDelete={canDelete}
                onAttach={onAttach}
                onDelete={onDelete}
              />
            ))}
          </div>
        ))}
      </div>
      {canList && !filtering && totalCount === (activeSession ? 1 : 0) ? (
        <p className="no-saved-threads">{t("history.noOtherSessions")}</p>
      ) : null}
      {filtering && visibleCount === 0 ? (
        <p className="no-saved-threads">{t("history.noMatchingThreads")}</p>
      ) : null}
      {filtering && nextCursor ? (
        <p className="session-filter-scope">{t("history.filterScope")}</p>
      ) : null}
      {nextCursor ? (
        <button type="button" className="load-more" disabled={disabled} onClick={() => onMore(nextCursor)}>
          {t("history.loadMore")}
        </button>
      ) : null}
    </section>
  );
}

function SessionRow({
  session,
  active,
  busy,
  attention,
  deleting,
  open = false,
  disabled,
  canAttach,
  canDelete,
  onAttach,
  onDelete,
}: {
  session: SessionInfo;
  active: boolean;
  busy: boolean;
  attention: boolean;
  deleting: boolean;
  open?: boolean;
  disabled: boolean;
  canAttach: boolean;
  canDelete: boolean;
  onAttach: (session: SessionInfo) => void;
  onDelete: (sessionId: string) => void;
}) {
  const { t, i18n } = useTranslation("workspace");
  const title = session.title || shortId(session.sessionId);
  return (
    <div className={`session-row ${active ? "active current-thread" : ""} ${attention ? "attention" : ""}`}>
      <button
        type="button"
        className="session-open"
        disabled={disabled || active || deleting || (!open && !canAttach)}
        aria-current={active ? "page" : undefined}
        title={active || open || canAttach ? session.sessionId : t("history.cannotAttach")}
        onClick={() => onAttach(session)}
      >
        <History size={13} />
        <span>
          <strong>{title}</strong>
          <small>{active
            ? t("history.activeSession", { id: shortId(session.sessionId) })
            : formatSessionDate(session.updatedAt, t, i18n.resolvedLanguage)}</small>
        </span>
        {attention ? (
          <i className="session-attention" aria-label={t("history.inputRequired")} title={t("history.inputRequired")} />
        ) : null}
      </button>
      {canDelete ? (
        <button
          type="button"
          className="session-delete"
          disabled={disabled || busy || deleting}
          aria-label={deleting ? t("deletingSession", { title }) : t("deleteSession", { title })}
          title={busy
            ? t("history.stopBeforeDelete")
            : open || active
              ? t("history.closeAndDelete")
              : undefined}
          onClick={() => onDelete(session.sessionId)}
        >
          <Trash2 size={12} />
        </button>
      ) : null}
    </div>
  );
}

function shortId(value: string): string {
  return value.length > 14 ? `${value.slice(0, 7)}…${value.slice(-5)}` : value;
}

function formatSessionDate(value: string | null | undefined, t: TFunction<"workspace">, language?: string): string {
  if (!value) return t("history.savedByAgent");
  const date = new Date(value);
  return Number.isNaN(date.valueOf())
    ? t("history.savedByAgent")
    : date.toLocaleString(language, { dateStyle: "medium", timeStyle: "short" });
}
