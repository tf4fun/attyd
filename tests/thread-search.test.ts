import { describe, expect, it } from "vitest";
import {
  MAX_THREAD_SEARCH_MATCHES,
  MAX_THREAD_SEARCH_QUERY_CHARS,
  compileThreadSearch,
} from "../web/src/lib/thread-search";

const defaults = { caseSensitive: false, wholeWord: false, regex: false };

describe("Agent thread search", () => {
  it("finds literal text with optional case and Unicode whole-word matching", () => {
    expect(compileThreadSearch("alpha", defaults).find("Alpha alphabet alpha").matches)
      .toEqual([{ start: 0, end: 5 }, { start: 6, end: 11 }, { start: 15, end: 20 }]);
    expect(compileThreadSearch("alpha", { ...defaults, caseSensitive: true })
      .find("Alpha alpha").matches).toEqual([{ start: 6, end: 11 }]);
    expect(compileThreadSearch("猫", { ...defaults, wholeWord: true })
      .find("猫 猫咪 猫").matches).toEqual([{ start: 0, end: 1 }, { start: 5, end: 6 }]);
    expect(compileThreadSearch("foo.", { ...defaults, wholeWord: true })
      .find("foo. foobar.").matches).toEqual([{ start: 0, end: 4 }]);
  });

  it("supports regular expressions and reports malformed patterns", () => {
    const search = compileThreadSearch("tool-(\\d+)", { ...defaults, regex: true });
    expect(search.error).toBeUndefined();
    expect(search.find("tool-12 tool-x tool-34").matches)
      .toEqual([{ start: 0, end: 7 }, { start: 15, end: 22 }]);

    const malformed = compileThreadSearch("(", { ...defaults, regex: true });
    expect(malformed.error).toMatch(/unterminated|invalid|parenthes/i);
    expect(malformed.find("anything").matches).toEqual([]);
    expect(compileThreadSearch("^|$", { ...defaults, regex: true }).find("safe").matches)
      .toEqual([]);
  });

  it("bounds queries and retained match counts", () => {
    const tooLong = compileThreadSearch("x".repeat(MAX_THREAD_SEARCH_QUERY_CHARS + 1), defaults);
    expect(tooLong.error).toContain(String(MAX_THREAD_SEARCH_QUERY_CHARS));

    const bounded = compileThreadSearch("x", defaults).find(
      "x".repeat(MAX_THREAD_SEARCH_MATCHES + 2),
    );
    expect(bounded.matches).toHaveLength(MAX_THREAD_SEARCH_MATCHES);
    expect(bounded.limited).toBe(true);
  });
});
