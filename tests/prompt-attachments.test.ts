// @vitest-environment happy-dom

import { describe, expect, it } from "vitest";
import {
  MAX_ATTACHMENT_BYTES,
  createPromptAttachments,
} from "../src/lib/prompt-attachments";

describe("ACP prompt attachment conversion", () => {
  it("maps browser files to the exact negotiated ContentBlock variants", async () => {
    const attachments = await createPromptAttachments([
      new File([new Uint8Array([1, 2, 3])], "pixel.png", { type: "image/png" }),
      new File([new Uint8Array([4, 5])], "sample.wav", { type: "audio/wav" }),
      new File(["# Context\n"], "notes.md", { type: "text/markdown" }),
      new File([new Uint8Array([6, 7])], "fixture.bin", {
        type: "application/octet-stream",
      }),
    ], {
      image: true,
      audio: true,
      embeddedContext: true,
    });

    expect(attachments.map(({ block }) => block.type)).toEqual([
      "image",
      "audio",
      "resource",
      "resource",
    ]);
    expect(attachments[0]?.block).toMatchObject({
      type: "image",
      mimeType: "image/png",
      data: "AQID",
      uri: "attyd://attachment/pixel.png",
    });
    expect(attachments[1]?.block).toMatchObject({
      type: "audio",
      mimeType: "audio/wav",
      data: "BAU=",
    });
    expect(attachments[2]?.block).toMatchObject({
      type: "resource",
      resource: {
        uri: "attyd://attachment/notes.md",
        mimeType: "text/markdown",
        text: "# Context\n",
      },
    });
    expect(attachments[3]?.block).toMatchObject({
      type: "resource",
      resource: {
        uri: "attyd://attachment/fixture.bin",
        mimeType: "application/octet-stream",
        blob: "Bgc=",
      },
    });
  });

  it("uses safe names for unnamed clipboard media and rejects unoffered input", async () => {
    const [image] = await createPromptAttachments([
      new File([new Uint8Array([1])], "", { type: "image/png" }),
    ], { image: true });
    expect(image).toMatchObject({
      name: "pasted-image-1.png",
      block: { type: "image", uri: "attyd://attachment/pasted-image-1.png" },
    });

    await expect(createPromptAttachments([
      new File([new Uint8Array([1])], "pixel.png", { type: "image/png" }),
    ], {})).rejects.toThrow("agent cannot accept pixel.png");
  });

  it("bounds each conversion batch before reading file content", async () => {
    await expect(createPromptAttachments([
      new File([new Uint8Array(MAX_ATTACHMENT_BYTES + 1)], "large.bin", {
        type: "application/octet-stream",
      }),
    ], { embeddedContext: true })).rejects.toThrow("limited to 3 MB");
  });
});
