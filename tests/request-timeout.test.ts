// @vitest-environment happy-dom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { ApiError, REQUEST_TIMEOUT_MS, RequestTimeoutError, requestJson } from "../web/src/lib/business-api";
import i18n from "../web/src/i18n";

describe("ordinary API request deadlines", () => {
  beforeEach(async () => {
    vi.useFakeTimers();
    await i18n.changeLanguage("en");
  });
  afterEach(() => { vi.useRealTimers(); vi.unstubAllGlobals(); vi.restoreAllMocks(); });

  it.each(["GET", "POST", "DELETE"])("bounds a stalled %s request and aborts it without retrying", async (method) => {
    const fetchMock = vi.fn<typeof fetch>().mockImplementation(() => new Promise(() => {}));
    vi.stubGlobal("fetch", fetchMock);
    let failure: unknown;
    void requestJson("/api/v1/test", { method }).catch((error) => { failure = error; });
    await vi.advanceTimersByTimeAsync(REQUEST_TIMEOUT_MS - 1);
    expect(failure).toBeUndefined();
    await vi.advanceTimersByTimeAsync(1);
    expect(failure).toBeInstanceOf(RequestTimeoutError);
    expect(fetchMock.mock.calls[0][1]?.signal?.aborted).toBe(true);
    expect(fetchMock).toHaveBeenCalledTimes(1);
    expect(vi.getTimerCount()).toBe(0);
    if (method !== "GET") expect((failure as Error).message).toContain("may still be running");
  });

  it("keeps the deadline active through the response body, not only response headers", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue({ ok: true, text: () => new Promise(() => {}) }));
    let failure: unknown;
    void requestJson("/api/v1/test").catch((error) => { failure = error; });
    await vi.advanceTimersByTimeAsync(REQUEST_TIMEOUT_MS);
    expect(failure).toBeInstanceOf(RequestTimeoutError);
    expect(vi.getTimerCount()).toBe(0);
  });

  it("honors caller cancellation immediately and removes its listener and deadline", async () => {
    const caller = new AbortController();
    const remove = vi.spyOn(caller.signal, "removeEventListener");
    const fetchMock = vi.fn<typeof fetch>().mockImplementation(() => new Promise(() => {}));
    vi.stubGlobal("fetch", fetchMock);
    let failure: unknown;
    void requestJson("/api/v1/test", { signal: caller.signal }).catch((error) => { failure = error; });
    const reason = new DOMException("Navigated away", "AbortError");
    caller.abort(reason);
    await vi.advanceTimersByTimeAsync(0);
    expect(failure).toBe(reason);
    expect(fetchMock.mock.calls[0][1]?.signal?.aborted).toBe(true);
    expect(remove).toHaveBeenCalledWith("abort", expect.any(Function));
    expect(vi.getTimerCount()).toBe(0);
  });

  it("does not send a request with an already cancelled signal", async () => {
    const caller = new AbortController();
    caller.abort();
    const fetchMock = vi.fn();
    vi.stubGlobal("fetch", fetchMock);
    await expect(requestJson("/api/v1/test", { signal: caller.signal })).rejects.toBe(caller.signal.reason);
    expect(fetchMock).not.toHaveBeenCalled();
    expect(vi.getTimerCount()).toBe(0);
  });

  it("preserves successful JSON responses, headers and caller signals and cleans up", async () => {
    const caller = new AbortController();
    const fetchMock = vi.fn<typeof fetch>().mockResolvedValue(new Response('{"ok":true}'));
    vi.stubGlobal("fetch", fetchMock);
    await expect(requestJson("/api/v1/test", { method: "POST", body: "{}", signal: caller.signal,
      headers: { "Idempotency-Key": "intent" } })).resolves.toEqual({ ok: true });
    const headers = new Headers(fetchMock.mock.calls[0][1]?.headers);
    expect(headers.get("content-type")).toBe("application/json");
    expect(headers.get("Idempotency-Key")).toBe("intent");
    expect(caller.signal.aborted).toBe(false);
    expect(vi.getTimerCount()).toBe(0);
  });

  it("preserves network and API errors and removes the deadline", async () => {
    const network = new TypeError("Offline");
    const fetchMock = vi.fn().mockRejectedValueOnce(network)
      .mockResolvedValueOnce(new Response('{"error":"Conflict"}', { status: 409 }));
    vi.stubGlobal("fetch", fetchMock);
    await expect(requestJson("/api/v1/test")).rejects.toBe(network);
    await expect(requestJson("/api/v1/test")).rejects.toMatchObject({ name: "ApiError", status: 409, message: "Conflict" });
    expect(vi.getTimerCount()).toBe(0);
  });

  it("reports a server deadline as a timeout and keeps unrelated gateway errors intact", async () => {
    vi.stubGlobal("fetch", vi.fn()
      .mockResolvedValueOnce(new Response(JSON.stringify({ code: "request_timeout", timeoutMs: 60_000, error: "Timed out" }), { status: 504 }))
      .mockResolvedValueOnce(new Response("Gateway failed", { status: 504 })));
    await expect(requestJson("/api/v1/test", { method: "POST" })).rejects.toMatchObject({
      name: "RequestTimeoutError", timeoutMs: 60_000, method: "POST",
    });
    await expect(requestJson("/api/v1/test")).rejects.toBeInstanceOf(ApiError);
  });

  it("localizes timeout messages and starts each manual retry with a fresh deadline", async () => {
    await i18n.changeLanguage("zh-CN");
    const fetchMock = vi.fn<typeof fetch>().mockImplementationOnce(() => new Promise(() => {}))
      .mockResolvedValueOnce(new Response('{"ok":true}'));
    vi.stubGlobal("fetch", fetchMock);
    let failure: unknown;
    void requestJson("/api/v1/test").catch((error) => { failure = error; });
    await vi.advanceTimersByTimeAsync(REQUEST_TIMEOUT_MS);
    expect((failure as Error)?.message).toContain("请求超过 30 秒未完成");
    await expect(requestJson("/api/v1/test")).resolves.toEqual({ ok: true });
    expect(fetchMock).toHaveBeenCalledTimes(2);
    expect(vi.getTimerCount()).toBe(0);
  });
});
