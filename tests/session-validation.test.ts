import { describe, expect, it } from "vitest";
import {
  validateSessionConfigOptions,
  validateSessionControls,
  validateSessionModeReference,
  validateSessionModes,
} from "../server/session-validation";

describe("Agent-provided session controls", () => {
  it("accepts consistent modes and grouped/select/boolean config", () => {
    expect(() => validateSessionControls(
      {
        currentModeId: "build",
        availableModes: [
          { id: "build", name: "Build" },
          { id: "plan", name: "Plan" },
        ],
      },
      [
        { type: "boolean", id: "verbose", name: "Verbose", currentValue: true },
        {
          type: "select",
          id: "model",
          name: "Model",
          currentValue: "fast",
          options: [
            {
              group: "local",
              name: "Local",
              options: [{ value: "fast", name: "Fast" }],
            },
            {
              group: "remote",
              name: "Remote",
              options: [{ value: "deep", name: "Deep" }],
            },
          ],
        },
      ],
    )).not.toThrow();
  });

  it("rejects contradictory or ambiguous mode/config state", () => {
    expect(() => validateSessionModes({
      currentModeId: "missing",
      availableModes: [{ id: "build", name: "Build" }],
    })).toThrow("not included");
    expect(() => validateSessionModes({
      currentModeId: "build",
      availableModes: [
        { id: "build", name: "Build" },
        { id: "build", name: "Again" },
      ],
    })).toThrow("duplicate session mode ID");
    expect(() => validateSessionConfigOptions([
      { type: "boolean", id: "same", name: "One", currentValue: false },
      { type: "boolean", id: "same", name: "Two", currentValue: true },
    ])).toThrow("duplicate config option ID");
    expect(() => validateSessionConfigOptions([{
      type: "select",
      id: "model",
      name: "Model",
      currentValue: "missing",
      options: [{ value: "fast", name: "Fast" }],
    }])).toThrow("current value");
    expect(() => validateSessionConfigOptions([{
      type: "select",
      id: "model",
      name: "Model",
      currentValue: "fast",
      options: [
        { value: "fast", name: "Fast" },
        { value: "fast", name: "Duplicate" },
      ],
    }])).toThrow("duplicate value");
    expect(() => validateSessionModeReference({
      currentModeId: "build",
      availableModes: [{ id: "build", name: "Build" }],
    }, "ghost")).toThrow("not offered");
    expect(() => validateSessionModeReference(undefined, "build")).toThrow("not offered");
  });

  it("bounds untrusted control counts", () => {
    expect(() => validateSessionModes({
      currentModeId: "mode-0",
      availableModes: Array.from({ length: 257 }, (_, index) => ({
        id: `mode-${index}`,
        name: `Mode ${index}`,
      })),
    })).toThrow("more than 256");
    expect(() => validateSessionConfigOptions(Array.from({ length: 257 }, (_, index) => ({
      type: "boolean" as const,
      id: `option-${index}`,
      name: `Option ${index}`,
      currentValue: false,
    })))).toThrow("more than 256");
  });
});
