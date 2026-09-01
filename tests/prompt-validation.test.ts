import { describe, expect, it } from "vitest";
import { validatePromptResponse } from "../server/prompt-validation";

describe("ACP prompt response validation", () => {
  it("accepts coherent integer token usage", () => {
    expect(() => validatePromptResponse({
      stopReason: "max_tokens",
      usage: {
        totalTokens: 21,
        inputTokens: 13,
        outputTokens: 8,
        thoughtTokens: 3,
        cachedReadTokens: 5,
        cachedWriteTokens: 2,
      },
    })).not.toThrow();
  });

  it("rejects invalid counts and impossible totals", () => {
    expect(() => validatePromptResponse({
      stopReason: "end_turn",
      usage: { totalTokens: 1, inputTokens: -1, outputTokens: 1 },
    })).toThrow("non-negative safe integer");
    expect(() => validatePromptResponse({
      stopReason: "end_turn",
      usage: { totalTokens: 2, inputTokens: 1.5, outputTokens: 0 },
    })).toThrow("non-negative safe integer");
    expect(() => validatePromptResponse({
      stopReason: "end_turn",
      usage: { totalTokens: 4, inputTokens: 3, outputTokens: 2 },
    })).toThrow("exceeds totalTokens");
    expect(() => validatePromptResponse({
      stopReason: "end_turn",
      usage: {
        totalTokens: 4,
        inputTokens: 2,
        outputTokens: 2,
        cachedReadTokens: 5,
      },
    })).toThrow("exceeds totalTokens");
  });

  it("bounds the complete response including opaque metadata", () => {
    expect(() => validatePromptResponse({
      stopReason: "end_turn",
      _meta: { padding: "x".repeat(1_000_000) },
    })).toThrow("exceeds 1000000 bytes");
  });
});
