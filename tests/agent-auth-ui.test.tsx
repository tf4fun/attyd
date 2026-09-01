// @vitest-environment happy-dom

import type { AuthMethod } from "@agentclientprotocol/sdk";
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { AgentAuthCard, AgentAuthControls } from "../src/components/acp/agent-auth";

(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT: boolean })
  .IS_REACT_ACT_ENVIRONMENT = true;

const methods: AuthMethod[] = [{
  id: "agent-login",
  name: "Continue with Agent",
  description: "Uses the Agent-owned account",
}];

describe("ACP Agent authentication UI", () => {
  let container: HTMLDivElement;
  let root: Root;

  beforeEach(() => {
    container = document.createElement("div");
    document.body.append(container);
    root = createRoot(container);
  });

  afterEach(async () => {
    await act(async () => root.unmount());
    container.remove();
  });

  it("focuses an Agent-advertised sign-in action and never asks for credentials", async () => {
    const onAuthenticate = vi.fn();
    await act(async () => {
      root.render(
        <AgentAuthCard
          agentName="Fixture Agent"
          methods={methods}
          status="required"
          focusAction
          disabled={false}
          onAuthenticate={onAuthenticate}
        />,
      );
      await new Promise<void>((resolve) => requestAnimationFrame(() => resolve()));
    });

    const action = requireButton(container, "Authenticate with Continue with Agent");
    expect(document.activeElement).toBe(action);
    expect(container.querySelector("input")).toBeNull();
    expect(container.textContent).toContain("Authentication stays inside the Agent-provided flow.");
    await act(async () => action.click());
    expect(onAuthenticate).toHaveBeenCalledWith("agent-login");

    await act(async () => root.render(
      <AgentAuthCard
        agentName="Fixture Agent"
        methods={methods}
        status="required"
        pending={{ requestId: "auth", kind: "authenticate", methodId: "agent-login" }}
        focusAction={false}
        disabled={false}
        onAuthenticate={onAuthenticate}
      />,
    ));
    expect(action.disabled).toBe(true);
    expect(container.querySelector(".agent-auth-card")?.getAttribute("role")).not.toBe("dialog");
    expect(container.textContent).toContain("Waiting for the Agent to finish sign-in");
  });

  it("offers negotiated terminal authentication as an interactive action", async () => {
    const onAuthenticate = vi.fn();
    await act(async () => root.render(
      <AgentAuthCard
        agentName="Fixture Agent"
        methods={[{
          id: "terminal-login",
          name: "Sign in in terminal",
          type: "terminal",
          args: ["--login"],
        }]}
        status="required"
        focusAction={false}
        disabled={false}
        onAuthenticate={onAuthenticate}
      />,
    ));
    const action = requireButton(container, "Authenticate with Sign in in terminal");
    expect(action.disabled).toBe(false);
    expect(action.textContent).toContain("Open terminal");
    await act(async () => action.click());
    expect(onAuthenticate).toHaveBeenCalledWith("terminal-login");
  });

  it("keeps logout capability-gated and exposes the raw ACP response", async () => {
    const onAuthenticate = vi.fn();
    const onLogout = vi.fn();
    await act(async () => root.render(
      <AgentAuthControls
        methods={methods}
        status="authenticated"
        canLogout
        lastResponse={{ kind: "authenticate", response: { _meta: { account: "agent" } } }}
        disabled={false}
        onAuthenticate={onAuthenticate}
        onLogout={onLogout}
      />,
    ));

    expect(container.textContent).toContain("Signed in");
    await act(async () => buttonWithText(container, "Sign out of Agent").click());
    expect(onLogout).toHaveBeenCalledOnce();
    expect(container.querySelector("details.raw-json")?.textContent).toContain("Authenticate response");

    await act(async () => root.render(
      <AgentAuthControls
        methods={methods}
        status="available"
        canLogout={false}
        disabled={false}
        onAuthenticate={onAuthenticate}
        onLogout={onLogout}
      />,
    ));
    expect([...container.querySelectorAll("button")].some((button) =>
      button.textContent?.includes("Sign out")
    )).toBe(false);
  });
});

function requireButton(container: ParentNode, label: string): HTMLButtonElement {
  const button = container.querySelector<HTMLButtonElement>(`button[aria-label="${label}"]`);
  if (!button) throw new Error(`Missing button: ${label}`);
  return button;
}

function buttonWithText(container: ParentNode, text: string): HTMLButtonElement {
  const button = [...container.querySelectorAll<HTMLButtonElement>("button")]
    .find((candidate) => candidate.textContent?.trim() === text);
  if (!button) throw new Error(`Missing button: ${text}`);
  return button;
}
