// Mirrors `entries_use_the_documented_wire_shape` in src/protocol/canon.rs, so
// normalized content and attachment references are checked against the
// generated types (acceptance 1).
import type { LogEntry, PromptBlock } from "../../shared/protocol";

const image = {
  type: "attachment",
  id: "c44aabab6f5fe238799641da5a1ac8d3df1e546078398e3b727c2e7bc6806319",
  block: "image",
  mimeType: "image/png",
} as const;

export const entry = {
  turn: {
    prompt: [{ type: "text", text: "look" }, image],
    reply: [
      { type: "text", text: "I see" },
      { type: "resource_link", uri: "file:///a", name: "a" },
    ],
  },
  hash: "AAAAAAAAAAAAAAAAAAAAAA",
} satisfies LogEntry;

export const prompt = [
  { type: "text", text: "again" },
  image,
  { type: "image", mimeType: "image/png", data: "aGk=" },
] satisfies PromptBlock[];
