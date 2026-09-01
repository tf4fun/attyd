import type { CreateElicitationRequest } from "@agentclientprotocol/sdk";
import { describe, expect, it } from "vitest";
import {
  validateElicitationRequest,
  validateElicitationResponse,
} from "../server/elicitation-validation";

const formRequest: CreateElicitationRequest = {
  sessionId: "s",
  mode: "form",
  message: "Configure",
  requestedSchema: {
    type: "object",
    required: ["name", "count"],
    properties: {
      name: { type: "string", minLength: 2 },
      count: { type: "integer", minimum: 1, maximum: 3 },
      tags: {
        type: "array",
        maxItems: 2,
        items: { type: "string", enum: ["a", "b"] },
      },
    },
  },
};

describe("elicitation response validation", () => {
  it("accepts only the advertised form and safe URL request shapes", () => {
    expect(() => validateElicitationRequest({
      sessionId: "s",
      mode: "url",
      message: "Connect",
      elicitationId: "safe-flow",
      url: "https://example.test/connect",
    })).not.toThrow();
    expect(() => validateElicitationRequest({
      sessionId: "s",
      mode: "url",
      message: "Run this",
      elicitationId: "unsafe-flow",
      url: "javascript:alert(1)",
    })).toThrow("HTTP or HTTPS");
    expect(() => validateElicitationRequest({
      sessionId: "s",
      mode: "custom",
      message: "Unsupported",
    } as unknown as CreateElicitationRequest)).toThrow("unadvertised elicitation mode");
  });

  it("requires one bounded session or request scope", () => {
    expect(() => validateElicitationRequest({
      requestId: 42,
      mode: "form",
      message: "Configure before a session",
      requestedSchema: { type: "object", properties: {} },
    })).not.toThrow();
    expect(() => validateElicitationRequest({
      mode: "form",
      message: "No scope",
      requestedSchema: { type: "object", properties: {} },
    } as unknown as CreateElicitationRequest)).toThrow("exactly one");
    expect(() => validateElicitationRequest({
      sessionId: "session",
      requestId: "request",
      mode: "form",
      message: "Ambiguous",
      requestedSchema: { type: "object", properties: {} },
    } as unknown as CreateElicitationRequest)).toThrow("exactly one");
    expect(() => validateElicitationRequest({
      requestId: null,
      mode: "form",
      message: "Discouraged but valid JSON-RPC ID",
      requestedSchema: { type: "object", properties: {} },
    })).not.toThrow();
    expect(() => validateElicitationRequest({
      requestId: "",
      mode: "form",
      message: "Empty but valid JSON-RPC ID",
      requestedSchema: { type: "object", properties: {} },
    })).not.toThrow();
    expect(() => validateElicitationRequest({
      requestId: Number.MAX_SAFE_INTEGER + 1,
      mode: "form",
      message: "Unsafe numeric ID",
      requestedSchema: { type: "object", properties: {} },
    })).toThrow("invalid request scope");
  });

  it("bounds request text, field names, and total payload before rendering", () => {
    expect(() => validateElicitationRequest({
      ...formRequest,
      message: "",
    })).toThrow("message is empty or too long");
    expect(() => validateElicitationRequest({
      ...formRequest,
      message: "x".repeat(16_385),
    })).toThrow("message is empty or too long");
    expect(() => validateElicitationRequest({
      sessionId: "s",
      mode: "form",
      message: "Long field",
      requestedSchema: {
        type: "object",
        properties: {
          ["x".repeat(257)]: { type: "string" },
        },
      },
    })).toThrow("invalid field name");
    expect(() => validateElicitationRequest({
      ...formRequest,
      message: "x".repeat(2_000_001),
    })).toThrow("exceeds 2000000 bytes");
  });

  it("accepts content matching the agent schema", () => {
    expect(() => validateElicitationResponse(formRequest, {
      action: "accept",
      content: { name: "ok", count: 2, tags: ["a"] },
    })).not.toThrow();
  });

  it("rejects schema defaults that its own constraints would reject", () => {
    for (const property of [
      { type: "string", minLength: 2, default: "x" },
      { type: "string", enum: ["a", "b"], default: "c" },
      { type: "integer", maximum: 3, default: 99 },
      {
        type: "array",
        items: { type: "string", enum: ["a", "b"] },
        default: ["unknown"],
      },
    ] as const) {
      expect(() => validateElicitationRequest({
        sessionId: "s",
        mode: "form",
        message: "Invalid default",
        requestedSchema: {
          type: "object",
          properties: {
            value: property,
          },
        },
      })).toThrow("value");
    }
  });

  it("rejects missing, unknown, incorrectly typed, and out-of-range values", () => {
    expect(() => validateElicitationResponse(formRequest, {
      action: "accept",
      content: { name: "ok" },
    })).toThrow("count");
    expect(() => validateElicitationResponse(formRequest, {
      action: "accept",
      content: { name: "ok", count: 2, extra: true },
    })).toThrow("Unknown");
    expect(() => validateElicitationResponse(formRequest, {
      action: "accept",
      content: { name: "ok", count: 2.5 },
    })).toThrow("integer");
    expect(() => validateElicitationResponse(formRequest, {
      action: "accept",
      content: { name: "ok", count: 4 },
    })).toThrow("maximum");
  });

  it("keeps URL consent content-free", () => {
    const urlRequest: CreateElicitationRequest = {
      sessionId: "s",
      mode: "url",
      message: "Connect",
      elicitationId: "flow",
      url: "https://example.test",
    };
    expect(() => validateElicitationResponse(urlRequest, {
      action: "accept",
      content: { token: "must-not-cross-acp" },
    })).toThrow("must not contain content");
  });

  it("rejects unimplemented actions and content attached to non-accept responses", () => {
    expect(() => validateElicitationResponse(formRequest, {
      action: "invented",
    })).toThrow("Unsupported elicitation response action");
    expect(() => validateElicitationResponse(formRequest, {
      action: "decline",
      content: { name: "must not cross" },
    } as unknown as CreateElicitationResponse)).toThrow("Only accepted elicitation responses");
  });

  it("bounds the complete elicitation response payload", () => {
    const request: CreateElicitationRequest = {
      sessionId: "s",
      mode: "form",
      message: "Large",
      requestedSchema: {
        type: "object",
        properties: { value: { type: "string" } },
      },
    };
    expect(() => validateElicitationResponse(request, {
      action: "accept",
      content: { value: "x".repeat(2_000_001) },
    })).toThrow("exceeds 2000000 bytes");
  });

  it("enforces string patterns, formats, fallback enums, and unique selections", () => {
    const request: CreateElicitationRequest = {
      sessionId: "s",
      mode: "form",
      message: "Identity",
      requestedSchema: {
        type: "object",
        properties: {
          code: { type: "string", pattern: "^[A-Z]{2}$" },
          email: { type: "string", format: "email" },
          choice: { type: "string", oneOf: [], enum: ["a", "b"] },
          tags: {
            type: "array",
            items: { type: "string", enum: ["x", "y"] },
          },
        },
      },
    };
    expect(() => validateElicitationRequest(request)).not.toThrow();
    expect(() => validateElicitationResponse(request, {
      action: "accept",
      content: { code: "AB", email: "a@example.test", choice: "b", tags: ["x"] },
    })).not.toThrow();
    expect(() => validateElicitationResponse(request, {
      action: "accept",
      content: { code: "ab" },
    })).toThrow("pattern");
    expect(() => validateElicitationResponse(request, {
      action: "accept",
      content: { email: "not-an-email" },
    })).toThrow("email");
    expect(() => validateElicitationResponse(request, {
      action: "accept",
      content: { choice: "invented" },
    })).toThrow("allowed");
    expect(() => validateElicitationResponse(request, {
      action: "accept",
      content: { tags: ["x", "x"] },
    })).toThrow("duplicate");
  });

  it("rejects inconsistent or potentially explosive Agent schemas before rendering", () => {
    const request = (requestedSchema: Extract<CreateElicitationRequest, { mode: "form" }>["requestedSchema"]): CreateElicitationRequest => ({
      sessionId: "s",
      mode: "form",
      message: "Unsafe",
      requestedSchema,
    });
    expect(() => validateElicitationRequest(request({
      type: "object",
      required: ["missing"],
      properties: {},
    }))).toThrow("unknown field");
    expect(() => validateElicitationRequest(request({
      type: "object",
      properties: {
        value: { type: "string", pattern: "(a+)+$" },
      },
    }))).toThrow("unsafe");
    expect(() => validateElicitationRequest(request({
      type: "object",
      properties: {
        value: { type: "string", enum: ["a"], oneOf: [{ const: "b", title: "B" }] },
      },
    }))).toThrow("both enum and oneOf");
  });
});
