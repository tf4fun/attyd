import type { ContentBlock } from "@agentclientprotocol/sdk";
import { createContext, useContext, useEffect, useState } from "react";
import type { AttachmentRef } from "../../../shared/protocol";

export type AttachedBlock = Extract<ContentBlock, { type: "image" | "audio" | "resource" }>;

/**
 * Reads the blocks a session's `AttachmentRef`s name (protocol §8). The reads
 * are a media cache for presentation only: nothing read here enters a
 * replica (C1). A block stays cached while a view shows it, so one reference
 * is read once however often it renders, and is dropped when no view needs it.
 */
export interface AttachmentReader {
  readonly sessionId: string;
  /** Reads `reference` and holds it until the returned release is called. */
  acquire(reference: AttachmentRef): { read: Promise<AttachedBlock>; release: () => void };
}

export function createAttachmentReader(
  sessionId: string,
  schedule: (release: () => void) => void = (release) => void setTimeout(release, 0),
): AttachmentReader {
  const entries = new Map<string, { read: Promise<AttachedBlock>; users: number }>();
  return {
    sessionId,
    acquire(reference) {
      let entry = entries.get(reference.id);
      if (entry == null) {
        const read = fetch(`/api/v2/sessions/${encodeURIComponent(sessionId)}/attachments/${reference.id}`)
          .then(async (response) => {
            if (!response.ok) throw new Error(`attachment ${reference.id} is not available (${response.status})`);
            const block: unknown = await response.json();
            if (!isAttachedBlock(block)) throw new Error(`attachment ${reference.id} is not a media block`);
            return block;
          });
        const created = { read, users: 0 };
        entry = created;
        entries.set(reference.id, created);
        // A failed read is not kept: a later view may read it again.
        read.catch(() => {
          if (entries.get(reference.id) === created) entries.delete(reference.id);
        });
      }
      const held = entry;
      held.users += 1;
      let released = false;
      return {
        read: held.read,
        release: () => {
          if (released) return;
          released = true;
          held.users -= 1;
          // A view remounted in the same commit acquires it again before this runs.
          schedule(() => {
            if (held.users === 0 && entries.get(reference.id) === held) entries.delete(reference.id);
          });
        },
      };
    },
  };
}

export const AttachmentReaderContext = createContext<AttachmentReader | undefined>(undefined);

/** The block `reference` names, once read; `undefined` while it is read or unavailable. */
export function useAttachedBlock(reference: AttachmentRef): AttachedBlock | undefined {
  const reader = useContext(AttachmentReaderContext);
  const [read, setRead] = useState<{ key: string; block: AttachedBlock }>();
  const key = reader == null ? undefined : `${reader.sessionId}\n${reference.id}`;
  useEffect(() => {
    if (reader == null || key == null) return;
    let current = true;
    const { read: block, release } = reader.acquire(reference);
    block.then((value) => {
      if (current) setRead({ key, block: value });
    }, () => undefined);
    return () => {
      current = false;
      release();
    };
    // The reference is content-addressed: its ID, in `key`, names the block.
  }, [reader, key]);
  return read != null && read.key === key ? read.block : undefined;
}

/**
 * A local object URL with `block`'s bytes for the browser to open. Only inert
 * media keeps its type; anything a browser could run as a document (HTML,
 * SVG, XML, or an unknown type) is offered as an opaque download, since an
 * object URL has the application's origin and no sandbox (`download`).
 */
export function useBlockUrl(block: AttachedBlock | undefined): { url: string; download: boolean } | undefined {
  const [state, setState] = useState<{ block: AttachedBlock; url: string; download: boolean }>();
  useEffect(() => {
    if (block == null) return;
    const { blob, download } = blockBlob(block);
    const url = URL.createObjectURL(blob);
    setState({ block, url, download });
    // Created and revoked by the same effect, so each URL is released exactly once.
    return () => {
      URL.revokeObjectURL(url);
      setState((current) => current?.url === url ? undefined : current);
    };
  }, [block]);
  return state != null && state.block === block ? { url: state.url, download: state.download } : undefined;
}

export function blockMimeType(block: AttachedBlock): string {
  const resource = block.type === "resource" ? block.resource : block;
  return resource.mimeType ?? (block.type === "resource" && "text" in block.resource ? "text/plain" : "application/octet-stream");
}

/** The type a local URL may show inline, or `undefined` when it must only be downloaded. */
export function inertType(mimeType: string): string | undefined {
  const type = mimeType.split(";", 1)[0].trim().toLowerCase();
  if (/^image\/(?:png|jpeg|gif|webp|avif|bmp)$/u.test(type)) return type;
  if (/^(?:audio|video)\/[a-z0-9.+-]+$/u.test(type)) return type;
  // Plain text never runs; other text types are shown as plain text unless they are markup.
  if (type.startsWith("text/") && !/(?:html|xml)/u.test(type)) return "text/plain;charset=utf-8";
  if (type === "application/json") return "text/plain;charset=utf-8";
  return undefined;
}

function blockBlob(block: AttachedBlock): { blob: Blob; download: boolean } {
  const shown = inertType(blockMimeType(block));
  const type = shown ?? "application/octet-stream";
  if (block.type === "resource" && "text" in block.resource) {
    return { blob: new Blob([block.resource.text], { type }), download: shown == null };
  }
  const data = block.type === "resource" ? ("blob" in block.resource ? block.resource.blob : "") : block.data;
  const bytes = Uint8Array.from(atob(data), (character) => character.charCodeAt(0));
  return { blob: new Blob([bytes], { type }), download: shown == null };
}

function isAttachedBlock(value: unknown): value is AttachedBlock {
  if (value == null || typeof value !== "object" || Array.isArray(value)) return false;
  const block = value as Record<string, unknown>;
  if (block.type === "image" || block.type === "audio") return typeof block.data === "string";
  return block.type === "resource" && block.resource != null && typeof block.resource === "object";
}
