export const MAX_THREAD_SEARCH_QUERY_CHARS = 256;
export const MAX_THREAD_SEARCH_MATCHES = 10_000;
export const MAX_THREAD_SEARCH_CHARS = 2_000_000;

export interface ThreadSearchOptions {
  caseSensitive: boolean;
  wholeWord: boolean;
  regex: boolean;
}

export interface TextSearchMatch {
  start: number;
  end: number;
}

export interface TextSearchResult {
  matches: TextSearchMatch[];
  limited: boolean;
}

export interface CompiledThreadSearch {
  error?: string;
  find(text: string, limit?: number): TextSearchResult;
}

const WORD_CHARACTER = /[\p{L}\p{N}_]/u;

export function compileThreadSearch(
  query: string,
  options: ThreadSearchOptions,
): CompiledThreadSearch {
  if (query.length === 0) return emptySearch();
  if (query.length > MAX_THREAD_SEARCH_QUERY_CHARS) {
    return invalidSearch(`Search is limited to ${MAX_THREAD_SEARCH_QUERY_CHARS} characters`);
  }

  let expression: RegExp;
  try {
    expression = new RegExp(
      options.regex ? query : escapeRegExp(query),
      `gu${options.caseSensitive ? "" : "i"}`,
    );
  } catch (error) {
    return invalidSearch(error instanceof Error ? error.message : String(error));
  }

  return {
    find(text, requestedLimit = MAX_THREAD_SEARCH_MATCHES) {
      const limit = Math.min(
        MAX_THREAD_SEARCH_MATCHES,
        Math.max(0, Math.trunc(requestedLimit)),
      );
      if (limit === 0 || text.length === 0) return { matches: [], limited: false };

      const matches: TextSearchMatch[] = [];
      expression.lastIndex = 0;
      let match: RegExpExecArray | null;
      while ((match = expression.exec(text)) != null) {
        const start = match.index;
        const end = start + match[0].length;
        if (
          end > start &&
          (!options.wholeWord || isWholeWord(text, start, end))
        ) {
          if (matches.length >= limit) return { matches, limited: true };
          matches.push({ start, end });
        }

        if (match[0].length === 0) {
          expression.lastIndex = nextCodePointOffset(text, expression.lastIndex);
        }
      }
      return { matches, limited: false };
    },
  };
}

function emptySearch(): CompiledThreadSearch {
  return { find: () => ({ matches: [], limited: false }) };
}

function invalidSearch(error: string): CompiledThreadSearch {
  return {
    error,
    find: () => ({ matches: [], limited: false }),
  };
}

function escapeRegExp(value: string): string {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

function isWholeWord(text: string, start: number, end: number): boolean {
  const first = codePointAt(text, start);
  const last = codePointBefore(text, end);
  const startsWithWord = isWordCharacter(first);
  const endsWithWord = isWordCharacter(last);
  return (!startsWithWord || !isWordCharacter(codePointBefore(text, start))) &&
    (!endsWithWord || !isWordCharacter(codePointAt(text, end)));
}

function isWordCharacter(value: string | undefined): boolean {
  return value != null && WORD_CHARACTER.test(value);
}

function codePointAt(text: string, offset: number): string | undefined {
  if (offset < 0 || offset >= text.length) return undefined;
  const point = text.codePointAt(offset);
  return point == null ? undefined : String.fromCodePoint(point);
}

function codePointBefore(text: string, offset: number): string | undefined {
  if (offset <= 0 || offset > text.length) return undefined;
  let start = offset - 1;
  const trailing = text.charCodeAt(start);
  if (trailing >= 0xdc00 && trailing <= 0xdfff && start > 0) {
    const leading = text.charCodeAt(start - 1);
    if (leading >= 0xd800 && leading <= 0xdbff) start -= 1;
  }
  return text.slice(start, offset);
}

function nextCodePointOffset(text: string, offset: number): number {
  if (offset >= text.length) return text.length + 1;
  const point = text.codePointAt(offset);
  return offset + (point != null && point > 0xffff ? 2 : 1);
}
