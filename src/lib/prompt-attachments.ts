import type { ContentBlock, PromptCapabilities } from "@agentclientprotocol/sdk";
import { randomId } from "./id";

export const MAX_ATTACHMENT_BYTES = 3 * 1024 * 1024;

export interface PromptAttachment {
  id: string;
  name: string;
  size: number;
  block: ContentBlock;
}

export async function createPromptAttachments(
  files: File[],
  capabilities: PromptCapabilities | null | undefined,
): Promise<PromptAttachment[]> {
  const total = files.reduce((sum, file) => sum + file.size, 0);
  if (total > MAX_ATTACHMENT_BYTES) {
    throw new Error("Attachments are limited to 3 MB per prompt");
  }

  return Promise.all(
    files.map(async (file, index) => {
      const name = promptFileName(file, index);
      return {
        id: randomId(),
        name,
        size: file.size,
        block: await fileToContentBlock(file, capabilities, name),
      };
    }),
  );
}

async function fileToContentBlock(
  file: File,
  capabilities: PromptCapabilities | null | undefined,
  name: string,
): Promise<ContentBlock> {
  const mimeType = file.type || "application/octet-stream";
  const data = await toBase64(file);

  if (mimeType.startsWith("image/") && capabilities?.image) {
    return { type: "image", data, mimeType, uri: attachmentUri(name) };
  }
  if (mimeType.startsWith("audio/") && capabilities?.audio) {
    return { type: "audio", data, mimeType };
  }
  if (capabilities?.embeddedContext) {
    const resource = isTextFile(mimeType, file.name)
      ? { uri: attachmentUri(name), mimeType, text: await file.text() }
      : { uri: attachmentUri(name), mimeType, blob: data };
    return { type: "resource", resource };
  }

  throw new Error(`The agent cannot accept ${name}`);
}

function promptFileName(file: File, index: number): string {
  if (file.name.trim()) return file.name;
  const family = file.type.split("/", 1)[0];
  const rawSubtype = file.type.split("/", 2)[1]?.split(/[;+]/, 1)[0];
  const subtype = rawSubtype?.replace(/[^a-z0-9]+/gi, "-").replace(/^-|-$/g, "");
  if (family === "image") return `pasted-image-${index + 1}.${subtype || "bin"}`;
  if (family === "audio") return `pasted-audio-${index + 1}.${subtype || "bin"}`;
  return `attachment-${index + 1}${subtype ? `.${subtype}` : ""}`;
}

function attachmentUri(name: string): string {
  return `attyd://attachment/${encodeURIComponent(name)}`;
}

function isTextFile(mimeType: string, name: string): boolean {
  return (
    mimeType.startsWith("text/") ||
    /(?:json|xml|yaml|javascript|typescript|toml|sql)/.test(mimeType) ||
    /\.(?:md|txt|json|ya?ml|toml|tsx?|jsx?|css|html?|xml|sql|sh)$/i.test(name)
  );
}

async function toBase64(file: File): Promise<string> {
  const bytes = new Uint8Array(await file.arrayBuffer());
  let binary = "";
  for (let offset = 0; offset < bytes.length; offset += 0x8000) {
    binary += String.fromCharCode(...bytes.subarray(offset, offset + 0x8000));
  }
  return btoa(binary);
}
