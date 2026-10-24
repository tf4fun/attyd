// Which answers of a client's requests may be installed (docs/protocol-r4-design.md §7.3).
//
// Within an instance, versions are ordered: the result of a `GET` of a path the client does not
// follow is installed only if its path version is newer than what the client holds there; a
// followed path is installed from the stream only. Across instances versions have no order: when
// the client adopts a new instance, every request it issued before loses its right to install, so
// a late answer of a dead instance never displaces a newer one.

/** What a request remembers of the client when it is issued. */
export interface Ticket {
  /** How many instances the client had adopted. */
  readonly epoch: number;
}

export type Decision =
  /** Install the result. */
  | "install"
  /** The result is of an instance the client has not adopted: adopt it, then install. */
  | "adopt-and-install"
  /** The result must not be installed. */
  | "discard";

export class RequestOrder {
  private current: string | null = null;
  private epoch = 0;

  /** The instance the client holds, if any. */
  get instance(): string | null {
    return this.current;
  }

  /** Called when a request is issued. */
  issue(): Ticket {
    return { epoch: this.epoch };
  }

  /**
   * Adopts `instance` (from the first event of a stream or from a result): everything held of the
   * old instance is dropped by the caller, and every request issued before loses its right to
   * install. Adopting the instance already held changes nothing.
   */
  adopt(instance: string): void {
    if (instance === this.current) return;
    this.current = instance;
    this.epoch += 1;
  }

  /**
   * Whether the result of a request, answered by `instance` with the path at `version`, may be
   * installed over `held` (the version of the path the client holds, null for none).
   */
  decide(
    ticket: Ticket,
    result: { instance: string; version: number },
    held: number | null,
    followed: boolean,
  ): Decision {
    // A request issued before the current instance was adopted has lost its right.
    if (ticket.epoch < this.epoch) return "discard";
    // A followed path is installed from the stream only.
    if (followed) return "discard";
    if (result.instance !== this.current) return "adopt-and-install";
    return held === null || result.version > held ? "install" : "discard";
  }
}
