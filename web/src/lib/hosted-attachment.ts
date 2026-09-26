import type { ContentBlock } from "@agentclientprotocol/sdk";

export const ATTACHMENT_REFERENCE_KEY = "attyd/attachment";

export function attachmentHref(block: ContentBlock): string | undefined {
  if (block.type !== "resource_link") return undefined;
  const reference = block._meta?.[ATTACHMENT_REFERENCE_KEY];
  if (reference == null || typeof reference !== "object" || Array.isArray(reference)) return undefined;
  const { id, sessionId, bridgeEpoch, sessionIncarnation } = reference as Record<string, unknown>;
  if (typeof id !== "string" || !/^[a-f0-9]{64}$/.test(id) ||
    typeof sessionId !== "string" || !sessionId || typeof bridgeEpoch !== "string" || !bridgeEpoch ||
    typeof sessionIncarnation !== "number" || !Number.isSafeInteger(sessionIncarnation) || sessionIncarnation < 1) return undefined;
  const query = new URLSearchParams({ expectedEpoch: bridgeEpoch, expectedIncarnation: String(sessionIncarnation) });
  return `/api/v1/sessions/${encodeURIComponent(sessionId)}/attachments/${id}?${query}`;
}

/** Export can request full bodies explicitly without installing them in UI state. */
export function containsHostedAttachment(value: unknown): boolean {
  if (value == null || typeof value !== "object") return false;
  if (Array.isArray(value)) return value.some(containsHostedAttachment);
  if (ATTACHMENT_REFERENCE_KEY in value) return true;
  return Object.values(value).some(containsHostedAttachment);
}
