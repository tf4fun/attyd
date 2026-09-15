import type { Annotations, ContentBlock } from "@agentclientprotocol/sdk";

const MIME_TOKEN = "[A-Za-z0-9!#$%&'*+.^_`|~-]+";
const MIME_TYPE = new RegExp(
  `^${MIME_TOKEN}/${MIME_TOKEN}(?:;${MIME_TOKEN}=${MIME_TOKEN})*$`,
);
const BASE64 = /^[A-Za-z0-9+/]*={0,2}$/;

type MediaContentBlock = Extract<ContentBlock, { type: "image" | "audio" }>;

/**
 * Enforces semantic constraints that are deliberately stricter than the SDK's
 * structural JSON schema. It leaves ACP extension metadata opaque.
 */
export function validateContentBlockSemantics(
  block: ContentBlock,
  subject = "ACP content block",
): void {
  validateAnnotations(block.annotations, `${subject} annotations`);
  switch (block.type) {
    case "text":
      return;
    case "image":
      validateBase64(block.data, `${subject} image data`);
      validateMimeType(block.mimeType, `${subject} image MIME type`, "image");
      if (block.uri != null) validateUri(block.uri, `${subject} image URI`);
      return;
    case "audio":
      validateBase64(block.data, `${subject} audio data`);
      validateMimeType(block.mimeType, `${subject} audio MIME type`, "audio");
      return;
    case "resource_link":
      validateUri(block.uri, `${subject} resource URI`);
      validateLabel(block.name, `${subject} resource name`);
      if (block.title != null) validateLabel(block.title, `${subject} resource title`);
      if (block.description != null) {
        validateLabel(block.description, `${subject} resource description`);
      }
      if (block.mimeType != null) {
        validateMimeType(block.mimeType, `${subject} resource MIME type`);
      }
      if (
        block.size != null &&
        (!Number.isSafeInteger(block.size) || block.size < 0)
      ) {
        throw new Error(`${subject} resource size must be a non-negative safe integer`);
      }
      return;
    case "resource": {
      const resource = block.resource;
      validateUri(resource.uri, `${subject} embedded resource URI`);
      if (resource.mimeType != null) {
        validateMimeType(resource.mimeType, `${subject} embedded resource MIME type`);
      }
      if ("blob" in resource) {
        validateBase64(resource.blob, `${subject} embedded resource blob`);
      }
      return;
    }
  }
}

function validateAnnotations(
  annotations: Annotations | null | undefined,
  subject: string,
): void {
  if (annotations == null) return;
  if (annotations.lastModified != null) {
    validateLabel(annotations.lastModified, `${subject} last-modified timestamp`);
  }
  if (annotations.priority != null && !Number.isFinite(annotations.priority)) {
    throw new Error(`${subject} priority must be finite`);
  }
}

/** Returns a renderable data URL, or undefined for an invalid block. */
export function safeMediaDataUrl(block: MediaContentBlock): string | undefined {
  try {
    validateContentBlockSemantics(block);
    return `data:${block.mimeType};base64,${block.data}`;
  } catch {
    return undefined;
  }
}

function validateBase64(value: string, subject: string): void {
  if (value.length % 4 !== 0 || !BASE64.test(value)) {
    throw new Error(`${subject} must be canonical base64`);
  }
}

function validateMimeType(
  value: string,
  subject: string,
  expectedFamily?: "image" | "audio",
): void {
  if (
    value.length === 0 ||
    !MIME_TYPE.test(value)
  ) {
    throw new Error(`${subject} is invalid`);
  }
  const family = value.slice(0, value.indexOf("/")).toLowerCase();
  if (expectedFamily != null && family !== expectedFamily) {
    throw new Error(`${subject} must use the ${expectedFamily}/* family`);
  }
}

function validateUri(value: string, subject: string): void {
  if (value.length === 0) {
    throw new Error(`${subject} must not be empty`);
  }
  try {
    new URL(value);
  } catch {
    throw new Error(`${subject} is invalid`);
  }
}

function validateLabel(value: string, subject: string): void {
  if (typeof value !== "string") throw new Error(`${subject} must be a string`);
}
