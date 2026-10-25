// Operation IDs and the resolution of a write whose answer was lost (docs/protocol-r4-design.md
// §9.2, §9.3). A client chooses the ID of each operation it starts; with the instance and the
// path version of the target it held, the target's `op` and its log of operations (`outcome.ts`),
// it learns whether a write was admitted without keeping a table of past requests.

import { next, outcome, type Next, type TargetState } from "./outcome";

/** A write to the operations of `scope`: the ID it chose, the operation it expected and what the client held. */
export interface Write {
  readonly id: string;
  readonly expect: string;
  readonly scope: string;
  /** The instance of the bridge the client held when it wrote. */
  readonly instance?: string;
  /** The path version of the target the client held. */
  readonly at?: number;
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
 * What became of `write`, from the target's state (design §9.3). Only `admitted` is a definite yes;
 * `superseded` is a definite no (it can never be admitted); `open` and `unknown` are left to the
 * user, whom the page asks before repeating or resubmitting.
 */
export function resolveWrite(write: Write, target: TargetState | undefined): Resolution {
  if (target === undefined) return "unknown";
  const result = outcome(target, {
    id: write.id,
    expect: write.expect,
    instance: write.instance ?? target.instance,
    at: write.at ?? 0,
  });
  switch (result.outcome) {
    case "admitted":
      return "admitted";
    case "superseded":
      return "not_admitted";
    default:
      return "unknown";
  }
}

/**
 * What a client may do next with `write`, whose answer was lost (design §9.3): `superseded` alone
 * allows a new request, `open` only the same one unchanged, `unknown` is left to the user.
 */
export function nextStep(write: Write, target: TargetState | undefined): Next {
  if (target === undefined) return "ask-the-user";
  return next(outcome(target, {
    id: write.id,
    expect: write.expect,
    instance: write.instance ?? target.instance,
    at: write.at ?? 0,
  }));
}

/** The session a `create` or `fork` with ID `id` installed, while the bridge holds it (§6.6). */
export function originOf(
  sessions: Readonly<Record<string, { readonly sessionId: string; readonly origin?: string }>>,
  id: string,
): string | undefined {
  return Object.values(sessions).find((session) => session.origin === id)?.sessionId;
}
