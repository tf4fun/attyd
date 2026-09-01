import type { SessionInfo } from "@agentclientprotocol/sdk";

export type SessionHistoryGroupLabel =
  | "Today"
  | "Yesterday"
  | "Previous 7 Days"
  | "Previous 30 Days"
  | "Older"
  | "Saved by Agent";

export type SessionHistoryGroup = {
  label: SessionHistoryGroupLabel;
  sessions: SessionInfo[];
};

const GROUP_ORDER: SessionHistoryGroupLabel[] = [
  "Today",
  "Yesterday",
  "Previous 7 Days",
  "Previous 30 Days",
  "Older",
  "Saved by Agent",
];

export function filterSessions(sessions: SessionInfo[], query: string): SessionInfo[] {
  const terms = normalizeQuery(query);
  if (terms.length === 0) return sessions;
  return sessions.filter((session) => terms.every((term) => matchesTerm(session, term)));
}

export function sessionMatchesFilter(session: SessionInfo, query: string): boolean {
  const terms = normalizeQuery(query);
  return terms.length === 0 || terms.every((term) => matchesTerm(session, term));
}

export function groupSessionsByRecency(
  sessions: SessionInfo[],
  now = new Date(),
): SessionHistoryGroup[] {
  const groups = new Map<SessionHistoryGroupLabel, Array<{ session: SessionInfo; index: number; time?: number }>>();

  sessions.forEach((session, index) => {
    const time = session.updatedAt ? new Date(session.updatedAt).valueOf() : Number.NaN;
    const label = Number.isFinite(time)
      ? recencyLabel(new Date(time), now)
      : "Saved by Agent";
    const items = groups.get(label) ?? [];
    items.push({ session, index, time: Number.isFinite(time) ? time : undefined });
    groups.set(label, items);
  });

  return GROUP_ORDER.flatMap((label) => {
    const items = groups.get(label);
    if (!items) return [];
    items.sort((left, right) => {
      if (left.time == null || right.time == null) return left.index - right.index;
      return right.time - left.time || left.index - right.index;
    });
    return [{ label, sessions: items.map(({ session }) => session) }];
  });
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

function recencyLabel(date: Date, now: Date): SessionHistoryGroupLabel {
  const dayDifference = calendarDay(now) - calendarDay(date);
  if (dayDifference <= 0) return "Today";
  if (dayDifference === 1) return "Yesterday";
  if (dayDifference <= 7) return "Previous 7 Days";
  if (dayDifference <= 30) return "Previous 30 Days";
  return "Older";
}

function calendarDay(value: Date): number {
  return Date.UTC(value.getFullYear(), value.getMonth(), value.getDate()) / 86_400_000;
}
