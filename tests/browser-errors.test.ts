import { describe, expect, it } from "vitest";
import { isExpectedHttpError, type ExpectedHttpError } from "./browser/browser-errors";

const pageUrl = "http://127.0.0.1:7331/sessions/saved-session";
const missing = "Failed to load resource: the server responded with a status of 404 (Not Found)";
const sessionNode = "/api/v2/nodes/sessions/saved-session";

describe("browser expected HTTP errors", () => {
  const unavailable = "Failed to load resource: the server responded with a status of 503 (Service Unavailable)";
  const expected: ExpectedHttpError[] = [{ status: 503, pathname: sessionNode }];

  it("allows a deliberate outage only for the specified same-origin resource", () => {
    const url = `http://127.0.0.1:7331${sessionNode}?have=0`;
    expect(isExpectedHttpError(unavailable, url, pageUrl, expected)).toBe(true);
    expect(isExpectedHttpError(unavailable, url, pageUrl, [])).toBe(false);
    expect(isExpectedHttpError(missing, url, pageUrl, expected)).toBe(false);
    expect(isExpectedHttpError("Uncaught Error: 503", url, pageUrl, expected)).toBe(false);
  });

  it.each([
    `http://other-host:7331${sessionNode}`,
    "http://127.0.0.1:7331/api/v2/nodes/sessions/another-session",
    `http://127.0.0.1:7331${sessionNode}/ops`,
    "http://127.0.0.1:7331/api/v2/nodes/global",
    "http://127.0.0.1:7331/api/v2/stream?follow=/global",
    "http://127.0.0.1:7331/assets/client.js",
    "not a URL",
  ])("keeps other resources visible during a deliberate outage: %s", (url) => {
    expect(isExpectedHttpError(unavailable, url, pageUrl, expected)).toBe(false);
  });
});
