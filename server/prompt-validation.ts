import type { PromptResponse, Usage } from "@agentclientprotocol/sdk";

const MAX_PROMPT_RESPONSE_BYTES = 1_000_000;

export function validatePromptResponse(response: PromptResponse): void {
  if (Buffer.byteLength(JSON.stringify(response), "utf8") > MAX_PROMPT_RESPONSE_BYTES) {
    throw new Error(`Agent prompt response exceeds ${MAX_PROMPT_RESPONSE_BYTES} bytes`);
  }
  if (response.usage != null) validatePromptUsage(response.usage);
}

function validatePromptUsage(usage: Usage): void {
  const counts = [
    ["totalTokens", usage.totalTokens],
    ["inputTokens", usage.inputTokens],
    ["outputTokens", usage.outputTokens],
    ["thoughtTokens", usage.thoughtTokens],
    ["cachedReadTokens", usage.cachedReadTokens],
    ["cachedWriteTokens", usage.cachedWriteTokens],
  ] as const;
  for (const [name, value] of counts) {
    if (value == null) continue;
    if (!Number.isSafeInteger(value) || value < 0) {
      throw new Error(`Agent prompt usage ${name} must be a non-negative safe integer`);
    }
    if (name !== "totalTokens" && value > usage.totalTokens) {
      throw new Error(`Agent prompt usage ${name} exceeds totalTokens`);
    }
  }
  if (usage.inputTokens + usage.outputTokens > usage.totalTokens) {
    throw new Error("Agent prompt usage inputTokens plus outputTokens exceeds totalTokens");
  }
}
