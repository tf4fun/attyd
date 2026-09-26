import { describe, expect, it } from "vitest";
import { isExpectedHttpError, isExpectedRetainedViewMiss, type ExpectedHttpError } from "./browser/browser-errors";

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

describe("browser expected HTTP errors", () => {
  const unavailable = "Failed to load resource: the server responded with a status of 503 (Service Unavailable)";
  const expected: ExpectedHttpError[] = [{ status: 503, pathname: "/api/v1/sessions/saved-session" }];

  it("allows a deliberate outage only for the specified same-origin resource", () => {
    const url = "http://127.0.0.1:7331/api/v1/sessions/saved-session?presentation=compact&expectedEpoch=old&expectedIncarnation=1";
    expect(isExpectedHttpError(unavailable, url, pageUrl, expected)).toBe(true);
    expect(isExpectedHttpError(unavailable, url, pageUrl, [])).toBe(false);
    expect(isExpectedHttpError(missing, url, pageUrl, expected)).toBe(false);
    expect(isExpectedHttpError("Uncaught Error: 503", url, pageUrl, expected)).toBe(false);
  });

  it.each([
    "http://other-host:7331/api/v1/sessions/saved-session",
    "http://127.0.0.1:7331/api/v1/sessions/another-session",
    "http://127.0.0.1:7331/api/v1/sessions/saved-session/events",
    "http://127.0.0.1:7331/api/v1/sessions",
    "http://127.0.0.1:7331/api/v1/runtime",
    "http://127.0.0.1:7331/assets/client.js",
    "not a URL",
  ])("keeps other resources visible during a deliberate outage: %s", (url) => {
    expect(isExpectedHttpError(unavailable, url, pageUrl, expected)).toBe(false);
  });
});
