import type { Annotations, ContentBlock } from "@agentclientprotocol/sdk";

export const MAX_CONTENT_BINARY_BYTES = 3 * 1024 * 1024;

const MAX_URI_LENGTH = 16_384;
const MAX_MIME_TYPE_LENGTH = 255;
const MAX_RESOURCE_LABEL_LENGTH = 16_384;
const MAX_ANNOTATION_TIMESTAMP_LENGTH = 16_384;
const MIME_TOKEN = "[A-Za-z0-9!#$%&'*+.^_`|~-]+";
const MIME_TYPE = new RegExp(
  `^${MIME_TOKEN}/${MIME_TOKEN}(?:;${MIME_TOKEN}=${MIME_TOKEN})*$`,
);
const BASE64 = /^(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=)?$/;

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
  if (
    annotations.lastModified != null &&
    annotations.lastModified.length > MAX_ANNOTATION_TIMESTAMP_LENGTH
  ) {
    throw new Error(
      `${subject} last-modified timestamp exceeds ${MAX_ANNOTATION_TIMESTAMP_LENGTH} characters`,
    );
  }
  if (annotations.priority != null && !Number.isFinite(annotations.priority)) {
    throw new Error(`${subject} priority must be finite`);
  }
}

/** Returns a renderable data URL, or undefined for an invalid/unbounded block. */
export function safeMediaDataUrl(block: MediaContentBlock): string | undefined {
  try {
    validateContentBlockSemantics(block);
    return `data:${block.mimeType};base64,${block.data}`;
  } catch {
    return undefined;
  }
}

function validateBase64(value: string, subject: string): void {
  const maximumCharacters = Math.ceil(MAX_CONTENT_BINARY_BYTES / 3) * 4;
  if (value.length > maximumCharacters) {
    throw new Error(`${subject} exceeds ${MAX_CONTENT_BINARY_BYTES} decoded bytes`);
  }
  if (value.length % 4 !== 0 || !BASE64.test(value)) {
    throw new Error(`${subject} must be canonical base64`);
  }
  const padding = value.endsWith("==") ? 2 : value.endsWith("=") ? 1 : 0;
  const decodedBytes = (value.length / 4) * 3 - padding;
  if (decodedBytes > MAX_CONTENT_BINARY_BYTES) {
    throw new Error(`${subject} exceeds ${MAX_CONTENT_BINARY_BYTES} decoded bytes`);
  }
}

function validateMimeType(
  value: string,
  subject: string,
  expectedFamily?: "image" | "audio",
): void {
  if (
    value.length === 0 ||
    value.length > MAX_MIME_TYPE_LENGTH ||
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
  if (value.length === 0 || value.length > MAX_URI_LENGTH) {
    throw new Error(`${subject} must contain between 1 and ${MAX_URI_LENGTH} characters`);
  }
  try {
    new URL(value);
  } catch {
    throw new Error(`${subject} is invalid`);
  }
}

function validateLabel(value: string, subject: string): void {
  if (value.length > MAX_RESOURCE_LABEL_LENGTH) {
    throw new Error(`${subject} exceeds ${MAX_RESOURCE_LABEL_LENGTH} characters`);
  }
}
