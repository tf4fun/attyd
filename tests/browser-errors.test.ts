import { describe, expect, it } from "vitest";
import { isExpectedRetainedViewMiss } from "./browser/browser-errors";

const pageUrl = "http://127.0.0.1:7331/sessions/saved-session";
const missing = "Failed to load resource: the server responded with a status of 404 (Not Found)";

describe("browser retained-view probe error classification", () => {
  it.each(["", "?presentation=compact"])("allows the initial retained-session miss with query %s", (query) => {
    expect(isExpectedRetainedViewMiss(missing, `http://127.0.0.1:7331/api/v1/sessions/saved-session${query}`, pageUrl)).toBe(true);
  });

  it.each([
    "?cwd=%2Fworkspace&presentation=compact",
    "?presentation=compact&expectedEpoch=epoch&expectedIncarnation=1",
    "?presentation=compact&includeProcessFrom=op",
    "?presentation=compact&unexpected=value",
    "?presentation=compact&presentation=compact",
    "?presentation=full",
    "?presentation=",
  ])("keeps directory, owner-fenced and other lookup failures visible: %s", (query) => {
    expect(isExpectedRetainedViewMiss(missing, `http://127.0.0.1:7331/api/v1/sessions/saved-session${query}`, pageUrl)).toBe(false);
  });

  it.each([
    "http://other-host:7331/api/v1/sessions/saved-session?presentation=compact",
    "http://127.0.0.1:7331/api/v1/sessions/saved-session/events",
    "http://127.0.0.1:7331/api/v1/sessions/saved-session/turns/turn/process?presentation=compact",
    "http://127.0.0.1:7331/api/v1/sessions",
    "http://127.0.0.1:7331/assets/missing.js",
    "not a URL",
    "",
  ])("does not suppress errors for another resource: %s", (location) => {
    expect(isExpectedRetainedViewMiss(missing, location, pageUrl)).toBe(false);
  });

  it.each([
    "Failed to load resource: the server responded with a status of 500 (Internal Server Error)",
    "Failed to load resource: the server responded with a status of 409 (Conflict)",
    "Uncaught TypeError: cannot read property",
  ])("keeps other HTTP statuses and application errors visible: %s", (text) => {
    expect(isExpectedRetainedViewMiss(text, "http://127.0.0.1:7331/api/v1/sessions/saved-session?presentation=compact", pageUrl)).toBe(false);
  });
});
