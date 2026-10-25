// The reference client's polling against the request ordering of design §7.3. The transport is
// faked so each case fixes the order in which the answers of two reads arrive.
import { describe, expect, it } from "vitest";
import { wire } from "../shared/digest";
import { hashAt, tokensOf } from "../shared/tree-replica";
import { ReferenceClient } from "../scripts/reference-client";

const SESSION = "S";
const OP_PATH = `/sessions/${SESSION}/op`;

interface Pending {
  readonly path: string;
  resolve(response: Response): void;
}

/** A `fetch` whose answers the case resolves itself, one per request. */
function fakeFetch() {
  const pending: Pending[] = [];
  const fetchFake = ((input: RequestInfo | URL) => {
    const url = new URL(String(input));
    const path = url.pathname.replace("/api/v2/nodes", "");
    return new Promise<Response>((resolve) => pending.push({ path, resolve }));
  }) as typeof fetch;
  return {
    fetch: fetchFake,
    /** The oldest request of `path` not yet answered, or the newest. */
    take(path: string, which: "oldest" | "newest" = "oldest"): Pending {
      const matching = pending.filter((request) => request.path === path);
      const found = which === "oldest" ? matching[0] : matching.at(-1);
      if (found === undefined) throw new Error(`no request for ${path}`);
      pending.splice(pending.indexOf(found), 1);
      return found;
    },
  };
}

const json = (body: unknown): Response => new Response(JSON.stringify(body), { status: 200 });
const unchanged = (instance: string): Response => json({ instance, rev: 1, unchanged: true });

function opAnswer(instance: string, at: number, id: string): Response {
  const value = { id, kind: "prompt", status: "completed" };
  const hash = wire(hashAt(tokensOf(OP_PATH), value));
  return json({
    instance,
    rev: at,
    change: { path: OP_PATH, snapshot: { base: null, at, hash, node: { v: value } } },
  });
}

/** Lets the client's pending polls reach their next request. */
const settle = () => new Promise<void>((resolve) => setTimeout(resolve, 0));

describe("a reference client's polls", () => {
  it("does not install the late answer of an instance it has since left", async () => {
    const transport = fakeFetch();
    const client = new ReferenceClient("http://device.test", SESSION, { fetch: transport.fetch });
    const outputPath = client.outputPath;

    // The first poll is answered slowly by instance A; a second, issued later, by instance B first.
    const first = client.poll();
    await settle();
    transport.take(outputPath).resolve(unchanged("A"));
    await settle();
    const second = client.poll();
    await settle();
    transport.take(outputPath).resolve(unchanged("B"));
    await settle();
    transport.take(OP_PATH, "newest").resolve(opAnswer("B", 3, "new-instance-op"));
    await expect(second).resolves.toBe("changed");
    expect(client.op?.id).toBe("new-instance-op");
    expect(client.instance).toBe("B");

    // The answer of the dead instance A arrives after the client adopted B: it installs nothing.
    transport.take(OP_PATH).resolve(opAnswer("A", 9, "dead-instance-op"));
    await expect(first).resolves.toBe("unchanged");
    expect(client.op?.id).toBe("new-instance-op");
    expect(client.instance).toBe("B");
  });

  it("installs an answer newer than what it holds and discards an older one of the same instance", async () => {
    const transport = fakeFetch();
    const client = new ReferenceClient("http://device.test", SESSION, { fetch: transport.fetch });
    const poll = async (answer: Response): Promise<string> => {
      const done = client.poll();
      await settle();
      transport.take(client.outputPath).resolve(unchanged("A"));
      await settle();
      transport.take(OP_PATH).resolve(answer);
      return done;
    };
    expect(await poll(opAnswer("A", 5, "five"))).toBe("changed");
    expect(await poll(opAnswer("A", 7, "seven"))).toBe("changed");
    expect(client.op?.id).toBe("seven");
    // An answer for version 6 of the same instance, delayed behind version 7: not installed.
    expect(await poll(opAnswer("A", 6, "six"))).toBe("unchanged");
    expect(client.op?.id).toBe("seven");
  });
});
