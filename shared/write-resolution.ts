// Operation IDs and the resolution of a write whose answer was lost (docs/protocol.md
// §6.5, §6.6). A client chooses the ID of each operation it starts; with the `id` and the
// `after` the bridge records on the operation, it learns from its replica whether a write
// was admitted, without keeping a table of past requests.

/** A write to the operations of `scope`: the ID it chose and the operation it expected. */
export interface Write {
  readonly id: string;
  readonly expect: string;
  readonly scope: string;
}

/** The `op` a scope shows: only what the resolution reads. */
export interface ShownOperation {
  readonly id: string;
  /** The scope the operation was written to. */
  readonly scope: string;
  /** The operation it replaced in that scope. */
  readonly after: string | null;
  readonly status: string;
}

export type Resolution = "admitted" | "not_admitted" | "unknown";

const OPERATION_ID = /^[A-Za-z0-9_-]{22}$/u;

export function isOperationId(value: string): boolean {
  return OPERATION_ID.test(value);
}

function randomBytes(length: number): Uint8Array {
  return globalThis.crypto.getRandomValues(new Uint8Array(length));
}

/** A new operation ID: 128 random bits in unpadded base64url (§6.5). */
export function newOperationId(random: (length: number) => Uint8Array = randomBytes): string {
  const bytes = random(16);
  let binary = "";
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return btoa(binary).replace(/\+/gu, "-").replace(/\//gu, "_").replace(/=+$/u, "");
}

/**
 * Whether `write` was admitted, from the `op` its scope shows now (§6.6). An operation
 * written to another scope says nothing about this one, whatever its `after`: that is
 * unknown, left to the user.
 */
export function resolveWrite(write: Write, shown: ShownOperation | undefined): Resolution {
  if (shown === undefined) return "unknown";
  if (shown.id === write.id) return "admitted";
  if (shown.scope === write.scope) {
    if (shown.after === write.id) return "admitted";
    if (shown.after === write.expect) return "not_admitted";
  }
  if (shown.id === write.expect && shown.status !== "running") return "not_admitted";
  return "unknown";
}

/** The session a `create` or `fork` with ID `id` installed, while the bridge holds it (§6.6). */
export function originOf(
  sessions: Readonly<Record<string, { readonly sessionId: string; readonly origin?: string }>>,
  id: string,
): string | undefined {
  return Object.values(sessions).find((session) => session.origin === id)?.sessionId;
}
