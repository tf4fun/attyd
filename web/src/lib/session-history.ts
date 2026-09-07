import type { SessionInfo } from "@agentclientprotocol/sdk";

export type SessionHistoryGroup = {
  cwd: string;
  label: string;
  sessions: SessionInfo[];
};

export function filterSessions(sessions: SessionInfo[], query: string): SessionInfo[] {
  const terms = normalizeQuery(query);
  if (terms.length === 0) return sessions;
  return sessions.filter((session) => terms.every((term) => matchesTerm(session, term)));
}

export function groupSessionsByWorkspace(sessions: SessionInfo[]): SessionHistoryGroup[] {
  const groups = new Map<string, {
    index: number;
    latest: number;
    items: Array<{ session: SessionInfo; index: number; time: number }>;
  }>();

  sessions.forEach((session, index) => {
    const cwd = session.cwd ?? "";
    const parsedTime = session.updatedAt ? new Date(session.updatedAt).valueOf() : Number.NaN;
    const time = Number.isFinite(parsedTime) ? parsedTime : Number.NEGATIVE_INFINITY;
    const group = groups.get(cwd) ?? { index, latest: time, items: [] };
    group.latest = Math.max(group.latest, time);
    group.items.push({ session, index, time });
    groups.set(cwd, group);
  });

  return [...groups.entries()]
    .sort(([, left], [, right]) => right.latest - left.latest || left.index - right.index)
    .map(([cwd, { items }]) => ({
      cwd,
      label: cwd || "Unknown workspace",
      sessions: items
        .sort((left, right) => right.time - left.time || left.index - right.index)
        .map(({ session }) => session),
    }));
}

function normalizeQuery(query: string): string[] {
  return query.trim().toLocaleLowerCase().split(/\s+/u).filter(Boolean);
}

function matchesTerm(session: SessionInfo, term: string): boolean {
  const title = (session.title ?? "").toLocaleLowerCase();
  const exactFields = [session.sessionId, session.cwd, ...(session.additionalDirectories ?? [])]
    .map((value) => value.toLocaleLowerCase());
  return title.includes(term)
    || isSubsequence(term, title)
    || exactFields.some((value) => value.includes(term));
}

function isSubsequence(needle: string, haystack: string): boolean {
  let needleIndex = 0;
  for (const character of haystack) {
    if (character === needle[needleIndex]) needleIndex += 1;
    if (needleIndex === needle.length) return true;
  }
  return false;
}
