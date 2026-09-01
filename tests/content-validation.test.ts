import type { ContentBlock } from "@agentclientprotocol/sdk";
import { describe, expect, it } from "vitest";
import {
  MAX_CONTENT_BINARY_BYTES,
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

  it("bounds decoded binary payloads independently of JSON envelope size", () => {
    const oversized = "AAAA".repeat(Math.floor(MAX_CONTENT_BINARY_BYTES / 3) + 1);
    expect(() => validateContentBlockSemantics({
      type: "resource",
      resource: {
        uri: "urn:fixture:large",
        mimeType: "application/octet-stream",
        blob: oversized,
      },
    })).toThrow(`${MAX_CONTENT_BINARY_BYTES} decoded bytes`);
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
    })).toThrow("annotations last-modified timestamp exceeds");
  });
});
