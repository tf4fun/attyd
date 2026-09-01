import { describe, expect, it } from "vitest";
import { limitNdjsonLineBytes } from "../server/limited-stream";

const encoder = new TextEncoder();
const decoder = new TextDecoder();

describe("bounded Agent NDJSON input", () => {
  it("passes chunked lines and resets the byte count at newlines", async () => {
    const input = streamFrom(["123", "45\n12", "345\n"]);
    const output = limitNdjsonLineBytes(input, 5);
    await expect(readText(output)).resolves.toBe("12345\n12345\n");
  });

  it("rejects an oversized line even when it is split across chunks", async () => {
    const input = streamFrom(["123", "456", "\n"]);
    const output = limitNdjsonLineBytes(input, 5);
    await expect(readText(output)).rejects.toThrow("exceeds 5 bytes");
  });

  it("counts encoded bytes instead of JavaScript characters", async () => {
    const input = streamFrom(["你", "好\n"]);
    const output = limitNdjsonLineBytes(input, 5);
    await expect(readText(output)).rejects.toThrow("exceeds 5 bytes");
  });
});

function streamFrom(chunks: string[]): ReadableStream<Uint8Array> {
  return new ReadableStream({
    start(controller) {
      for (const chunk of chunks) controller.enqueue(encoder.encode(chunk));
      controller.close();
    },
  });
}

async function readText(stream: ReadableStream<Uint8Array>): Promise<string> {
  const reader = stream.getReader();
  let output = "";
  for (;;) {
    const { value, done } = await reader.read();
    if (done) return output;
    output += decoder.decode(value, { stream: true });
  }
}
