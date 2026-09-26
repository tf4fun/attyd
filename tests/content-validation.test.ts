import type { ContentBlock } from "@agentclientprotocol/sdk";
import { describe, expect, it } from "vitest";
import {
  safeMediaDataUrl,
  validateContentBlockSemantics,
} from "../shared/content-validation";

describe("ACP content block semantic validation", () => {
  it("accepts standard media and absolute ACP/MCP resource URIs", () => {
    const blocks: ContentBlock[] = [
      {
        type: "image",
        data: "iVBORw==",
        mimeType: "image/png",
        uri: "attyd://attachment/screenshot.png",
      },
      { type: "audio", data: "AA==", mimeType: "audio/mpeg" },
      {
        type: "resource_link",
        name: "workspace file",
        uri: "file:///workspace/readme.md",
        mimeType: "text/markdown;charset=utf-8",
        size: 0,
      },
      {
        type: "resource",
        resource: { uri: "urn:fixture:text", mimeType: "text/plain", text: "hello" },
      },
      {
        type: "resource",
        resource: { uri: "mcp://fixture/empty", blob: "" },
      },
    ];

    for (const block of blocks) {
      expect(() => validateContentBlockSemantics(block)).not.toThrow();
    }
    expect(safeMediaDataUrl(blocks[0] as Extract<ContentBlock, { type: "image" }>))
      .toBe("data:image/png;base64,iVBORw==");
  });

  it("rejects malformed and non-canonical base64", () => {
    for (const data of ["not base64", "AAA", "AA=A", "____"] as const) {
      expect(() => validateContentBlockSemantics({
        type: "image",
        data,
        mimeType: "image/png",
      })).toThrow("canonical base64");
    }
    expect(safeMediaDataUrl({
      type: "audio",
      data: "not-base64",
      mimeType: "audio/mpeg",
    })).toBeUndefined();
  });

  it("accepts MIME parameter whitespace, quoted strings, and case-insensitive media families", () => {
    for (const mimeType of [
      "text/plain; charset=utf-8",
      "Text/Plain \t; CHARSET=\"UTF-8\"; format=flowed",
      'text/plain; label="a; b, c"; note="a\\\"b\\\\c"',
      'text/plain; label=""; ; charset=utf-8;\t',
    ]) {
      expect(() => validateContentBlockSemantics({
        type: "resource",
        resource: { uri: "urn:fixture:text", mimeType, text: "中文" },
      })).not.toThrow();
    }
    expect(() => validateContentBlockSemantics({
      type: "image", data: "AA==", mimeType: "IMAGE/PNG; profile=\"Display P3\"",
    })).not.toThrow();
    expect(() => validateContentBlockSemantics({
      type: "audio", data: "AA==", mimeType: "Audio/OGG; codecs=\"opus\"",
    })).not.toThrow();
  });

  it("rejects invalid MIME syntax and control characters even inside quoted parameters", () => {
    for (const mimeType of [
      "text/", "/plain", "text /plain", "text/plain; charset=",
      "text/plain; charset =utf-8", "text/plain; charset= utf-8",
      'text/plain; note="unterminated', 'text/plain; note="value"extra',
      "text/plain\n", "text/plain\r\nX-Injected: yes",
      'text/plain; note="bad\r\nheader"', 'text/plain; note="bad\\\nheader"',
      'text/plain; note="bad\u0000value"',
      'text/plain; note="bad\u007fvalue"',
    ]) {
      expect(() => validateContentBlockSemantics({
        type: "resource",
        resource: { uri: "urn:fixture:text", mimeType, text: "hello" },
      }), mimeType).toThrow("MIME type is invalid");
    }
  });

  it("keeps MIME parameter delimiters out of the data URL payload and fragment", async () => {
    const url = safeMediaDataUrl({
      type: "image", data: "AQID", mimeType: 'IMAGE/PNG; note="a,b#c?d"',
    });
    expect(url).toBeDefined();
    expect(new URL(url!).hash).toBe("");
    expect(new URL(url!).search).toBe("");
    const response = await fetch(url!);
    expect(response.headers.get("content-type")).toMatch(/^image\/png;/);
    expect(new Uint8Array(await response.arrayBuffer())).toEqual(new Uint8Array([1, 2, 3]));
  });

  it("rejects MIME injection and a media block with the wrong MIME family", () => {
    expect(() => validateContentBlockSemantics({
      type: "image",
      data: "AA==",
      mimeType: "image/png,https://attacker.invalid/",
    })).toThrow("MIME type is invalid");
    expect(() => validateContentBlockSemantics({
      type: "image",
      data: "AA==",
      mimeType: "text/html",
    })).toThrow("image/* family");
    expect(() => validateContentBlockSemantics({
      type: "audio",
      data: "AA==",
      mimeType: "audio/mpeg;base64=surprise;bad",
    })).toThrow("MIME type is invalid");
  });

  it("rejects relative or malformed URIs and impossible resource sizes", () => {
    expect(() => validateContentBlockSemantics({
      type: "resource_link",
      name: "relative",
      uri: "./relative.txt",
    })).toThrow("resource URI is invalid");
    for (const size of [-1, 1.5, Number.MAX_SAFE_INTEGER + 1]) {
      expect(() => validateContentBlockSemantics({
        type: "resource_link",
        name: "bad size",
        uri: "urn:fixture:size",
        size,
      })).toThrow("non-negative safe integer");
    }
  });

  it("accepts binary payloads beyond the former size limit", () => {
    const oversized = "AAAA".repeat(Math.floor(3 * 1024 * 1024 / 3) + 1);
    expect(() => validateContentBlockSemantics({
      type: "resource",
      resource: {
        uri: "urn:fixture:large",
        mimeType: "application/octet-stream",
        blob: oversized,
      },
    })).not.toThrow();
  });

  it("accepts ACP annotations while bounding unsafe semantic values", () => {
    expect(() => validateContentBlockSemantics({
      type: "text",
      text: "Annotated",
      annotations: {
        audience: ["user", "assistant"],
        priority: 0.75,
        lastModified: "2026-08-31T12:00:00Z",
        _meta: { extension: true },
      },
    })).not.toThrow();
    expect(() => validateContentBlockSemantics({
      type: "text",
      text: "Invalid priority",
      annotations: { priority: Number.POSITIVE_INFINITY },
    })).toThrow("priority must be finite");
    expect(() => validateContentBlockSemantics({
      type: "resource",
      annotations: { lastModified: "x".repeat(16_385) },
      resource: {
        uri: "urn:fixture:annotated",
        text: "note",
      },
    })).not.toThrow();
  });
});
