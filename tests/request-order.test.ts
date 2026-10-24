// Request ordering (docs/protocol-r4-design.md §7.3).
import { describe, expect, it } from "vitest";
import { RequestOrder } from "../shared/request-order";

describe("request ordering", () => {
  it("installs a result newer than what is held and discards an older or equal one", () => {
    const order = new RequestOrder();
    order.adopt("I");
    const ticket = order.issue();
    expect(order.decide(ticket, { instance: "I", version: 8 }, 7, false)).toBe("install");
    expect(order.decide(ticket, { instance: "I", version: 7 }, 7, false)).toBe("discard");
    expect(order.decide(ticket, { instance: "I", version: 6 }, 7, false)).toBe("discard");
    expect(order.decide(ticket, { instance: "I", version: 1 }, null, false)).toBe("install");
  });

  it("never installs a followed path from a read", () => {
    const order = new RequestOrder();
    order.adopt("I");
    expect(order.decide(order.issue(), { instance: "I", version: 99 }, 7, true)).toBe("discard");
  });

  it("discards what a request issued before the new instance was adopted brings", () => {
    const order = new RequestOrder();
    order.adopt("OLD");
    const late = order.issue();
    order.adopt("NEW");
    // A late answer of the dead instance, and of the new one: both lost their right.
    expect(order.decide(late, { instance: "OLD", version: 500 }, null, false)).toBe("discard");
    expect(order.decide(late, { instance: "NEW", version: 3 }, null, false)).toBe("discard");
    // The next request issued reaches the live instance.
    expect(order.decide(order.issue(), { instance: "NEW", version: 3 }, null, false)).toBe("install");
  });

  it("adopts an instance a read comes from when the request was issued after the current one", () => {
    const order = new RequestOrder();
    order.adopt("OLD");
    const ticket = order.issue();
    expect(order.decide(ticket, { instance: "NEW", version: 1 }, 40, false)).toBe("adopt-and-install");
    order.adopt("NEW");
    expect(order.instance).toBe("NEW");
    // Versions of the old instance mean nothing to the new: what was held is dropped by the caller.
    expect(order.decide(order.issue(), { instance: "NEW", version: 1 }, null, false)).toBe("install");
    // The request that adopted it is itself older than the adoption now.
    expect(order.decide(ticket, { instance: "NEW", version: 2 }, null, false)).toBe("discard");
  });

  it("does not count adopting the same instance again as a new one", () => {
    const order = new RequestOrder();
    order.adopt("I");
    const ticket = order.issue();
    order.adopt("I");
    expect(order.decide(ticket, { instance: "I", version: 2 }, 1, false)).toBe("install");
  });
});
