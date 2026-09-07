import { AGENT_METHODS, CLIENT_METHODS, PROTOCOL_METHODS } from "@agentclientprotocol/sdk";
import { describe, expect, it } from "vitest";
import {
  AGENT_METHOD_COVERAGE,
  CLIENT_METHOD_COVERAGE,
  CONTENT_BLOCK_COVERAGE,
  PROTOCOL_METHOD_COVERAGE,
  SESSION_UPDATE_COVERAGE,
} from "../shared/protocol-coverage";

describe("ACP schema coverage guard", () => {
  it("classifies every generated protocol method", () => {
    expect(Object.keys(AGENT_METHOD_COVERAGE).sort()).toEqual(
      [...new Set(Object.values(AGENT_METHODS))].sort(),
    );
    expect(Object.keys(CLIENT_METHOD_COVERAGE).sort()).toEqual(
      [...new Set(Object.values(CLIENT_METHODS))].sort(),
    );
    expect(Object.keys(PROTOCOL_METHOD_COVERAGE).sort()).toEqual(
      [...new Set(Object.values(PROTOCOL_METHODS))].sort(),
    );
  });

  it("keeps product exclusions explicit and all client handlers supported", () => {
    expect(Object.entries(AGENT_METHOD_COVERAGE)
      .filter(([, status]) => status === "product-exclusion")
      .map(([method]) => method)
      .sort()).toEqual([
        "providers/disable",
        "providers/list",
        "providers/set",
      ]);
    expect(Object.values(CLIENT_METHOD_COVERAGE).every((status) => status === "supported"))
      .toBe(true);
  });

  it("classifies every generated UI union discriminator", () => {
    expect(Object.keys(SESSION_UPDATE_COVERAGE)).toHaveLength(15);
    expect(Object.keys(CONTENT_BLOCK_COVERAGE)).toHaveLength(5);
  });
});
