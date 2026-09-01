import type { SessionInfo } from "@agentclientprotocol/sdk";
import { History, RefreshCw, Search, Trash2, X } from "lucide-react";
import { useMemo, useState } from "react";
import {
  filterSessions,
  groupSessionsByRecency,
  sessionMatchesFilter,
} from "../../lib/session-history";

export function SessionHistory({
  sessions,
  activeSessionId,
  activeTitle,
  canList,
  nextCursor,
  canAttach,
  canDelete,
  deletingSessionIds,
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
  canList: boolean;
  nextCursor?: string | null;
  canAttach: boolean;
  canDelete: boolean;
  deletingSessionIds: string[];
  openSessionIds?: string[];
  disabled: boolean;
  onAttach: (session: SessionInfo) => void;
  onDelete: (sessionId: string) => void;
  onRefresh: () => void;
  onMore: (cursor: string) => void;
}) {
  const [query, setQuery] = useState("");
  const activeSession = useMemo(() => {
    if (!activeSessionId) return undefined;
    const reported = sessions.find(({ sessionId }) => sessionId === activeSessionId);
    return {
      sessionId: activeSessionId,
      cwd: reported?.cwd ?? "",
      additionalDirectories: reported?.additionalDirectories,
      title: activeTitle ?? reported?.title,
      updatedAt: reported?.updatedAt,
    } satisfies SessionInfo;
  }, [activeSessionId, activeTitle, sessions]);
  const otherSessions = useMemo(
    () => sessions.filter(({ sessionId }) => sessionId !== activeSessionId),
    [activeSessionId, sessions],
  );
  const filteredSessions = useMemo(
    () => filterSessions(otherSessions, query),
    [otherSessions, query],
  );
  const groups = useMemo(
    () => groupSessionsByRecency(filteredSessions),
    [filteredSessions],
  );
  const activeVisible = Boolean(activeSession && sessionMatchesFilter(activeSession, query));
  const totalCount = otherSessions.length + (activeSession ? 1 : 0);
  const visibleCount = filteredSessions.length + (activeVisible ? 1 : 0);
  const filtering = query.trim().length > 0;

  return (
    <section className="sidebar-section session-history" aria-labelledby="threads-heading">
      <div className="section-heading">
        <span className="section-kicker" id="threads-heading">Threads</span>
        {canList ? (
          <button type="button" aria-label="Refresh Agent threads" onClick={onRefresh}>
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
            placeholder="Filter threads"
            aria-label="Filter loaded Agent threads"
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
            <button type="button" className="session-filter-clear" aria-label="Clear thread filter" onClick={() => setQuery("")}>
              <X size={11} />
            </button>
          ) : null}
        </div>
      ) : null}
      <p className="session-filter-status" id="session-filter-status" aria-live="polite">
        {filtering ? `${visibleCount} of ${totalCount} loaded threads` : `${totalCount} loaded ${totalCount === 1 ? "thread" : "threads"}`}
      </p>
      <div className="session-list">
        {activeVisible && activeSession ? (
          <div className="session-group">
            <h3 className="session-group-label">Current</h3>
            <SessionRow
              session={activeSession}
              active
              deleting={false}
              disabled
              canAttach={canAttach}
              canDelete={false}
              onAttach={onAttach}
              onDelete={onDelete}
            />
          </div>
        ) : null}
        {groups.map((group) => (
          <div className="session-group" key={group.label}>
            <h3 className="session-group-label">{group.label}</h3>
            {group.sessions.map((session) => (
              <SessionRow
                key={session.sessionId}
                session={session}
                active={false}
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
      {canList && !filtering && otherSessions.length === 0 ? (
        <p className="no-saved-threads">No other threads reported by the Agent.</p>
      ) : null}
      {filtering && visibleCount === 0 ? (
        <p className="no-saved-threads">No loaded Agent threads match this filter.</p>
      ) : null}
      {filtering && nextCursor ? (
        <p className="session-filter-scope">Filter covers loaded threads only.</p>
      ) : null}
      {nextCursor ? (
        <button type="button" className="load-more" disabled={disabled} onClick={() => onMore(nextCursor)}>
          Load more from Agent
        </button>
      ) : null}
    </section>
  );
}

function SessionRow({
  session,
  active,
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
  deleting: boolean;
  open?: boolean;
  disabled: boolean;
  canAttach: boolean;
  canDelete: boolean;
  onAttach: (session: SessionInfo) => void;
  onDelete: (sessionId: string) => void;
}) {
  const title = session.title || shortId(session.sessionId);
  return (
    <div className={`session-row ${active ? "active current-thread" : ""}`}>
      <button
        type="button"
        className="session-open"
        disabled={disabled || active || deleting || !canAttach}
        aria-current={active ? "page" : undefined}
        title={active || canAttach ? session.sessionId : "Agent can list sessions but cannot load or resume them"}
        onClick={() => onAttach(session)}
      >
        <History size={13} />
        <span>
          <strong>{title}</strong>
          <small>{active
            ? `${shortId(session.sessionId)} · active`
            : `${formatSessionDate(session.updatedAt)}${open ? " · open" : ""}`}</small>
        </span>
      </button>
      {canDelete && !active ? (
        <button
          type="button"
          className="session-delete"
          disabled={disabled || deleting || open}
          aria-label={`${deleting ? "Deleting" : "Delete"} ${title}`}
          title={open ? "Close this open thread before deleting it" : undefined}
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

function formatSessionDate(value: string | null | undefined): string {
  if (!value) return "Saved by Agent";
  const date = new Date(value);
  return Number.isNaN(date.valueOf())
    ? "Saved by Agent"
    : date.toLocaleString([], { dateStyle: "medium", timeStyle: "short" });
}
