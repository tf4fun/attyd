import type { AuthMethod } from "@agentclientprotocol/sdk";
import { ChevronRight, KeyRound, LogOut, ShieldCheck } from "lucide-react";
import { useEffect, useRef, type RefObject } from "react";
import type {
  AgentAuthResponse,
  AgentAuthStatus,
  PendingAuthOperation,
} from "../../lib/state";
import { RawJson } from "./raw-json";

export function AgentAuthCard({
  agentName,
  methods,
  status,
  pending,
  error,
  focusAction,
  disabled,
  onAuthenticate,
}: {
  agentName: string;
  methods: AuthMethod[];
  status: AgentAuthStatus;
  pending?: PendingAuthOperation;
  error?: string;
  focusAction: boolean;
  disabled: boolean;
  onAuthenticate: (methodId: string) => void;
}) {
  const firstAction = useRef<HTMLButtonElement>(null);
  useEffect(() => {
    if (!focusAction || pending) return;
    const frame = requestAnimationFrame(() => firstAction.current?.focus());
    return () => cancelAnimationFrame(frame);
  }, [focusAction, pending]);

  return (
    <section className="agent-auth-card" aria-labelledby="agent-auth-heading">
      <div className="agent-auth-icon"><KeyRound size={18} /></div>
      <div className="agent-auth-copy">
        <span className="eyebrow">ACP Agent authentication</span>
        <h2 id="agent-auth-heading">
          {status === "logged_out" ? `Signed out of ${agentName}` : "Sign in to continue"}
        </h2>
        <p>
          {agentName} owns its account, provider, and billing. attyd only invokes the
          authentication method the Agent advertised through ACP.
        </p>
      </div>
      <AuthMethodButtons
        methods={methods}
        pending={pending}
        disabled={disabled}
        firstActionRef={firstAction}
        onAuthenticate={onAuthenticate}
      />
      {pending?.kind === "authenticate" ? (
        <p className="agent-auth-progress" role="status">Waiting for the Agent to finish sign-in…</p>
      ) : null}
      {error ? <p className="agent-auth-error" role="alert">{error}</p> : null}
      <footer><ShieldCheck size={12} /> Authentication stays inside the Agent-provided flow.</footer>
    </section>
  );
}

export function AgentAuthControls({
  methods,
  status,
  pending,
  error,
  canLogout,
  lastResponse,
  disabled,
  onAuthenticate,
  onLogout,
}: {
  methods: AuthMethod[];
  status: AgentAuthStatus;
  pending?: PendingAuthOperation;
  error?: string;
  canLogout: boolean;
  lastResponse?: AgentAuthResponse;
  disabled: boolean;
  onAuthenticate: (methodId: string) => void;
  onLogout: () => void;
}) {
  return (
    <details className="sidebar-details agent-auth-details">
      <summary>
        <KeyRound size={13} /> Agent authentication
        <span className={`agent-auth-state state-${status}`}>{authStatusLabel(status)}</span>
        <ChevronRight size={12} />
      </summary>
      <div className="agent-auth-sidebar">
        <p>Authentication and provider access stay with the ACP Agent.</p>
        <AuthMethodButtons
          methods={methods}
          pending={pending}
          disabled={disabled}
          onAuthenticate={onAuthenticate}
          compact
        />
        {canLogout ? (
          <button
            type="button"
            className="agent-auth-logout"
            disabled={disabled || pending != null}
            onClick={onLogout}
          >
            <LogOut size={12} />
            {pending?.kind === "logout" ? "Signing out…" : "Sign out of Agent"}
          </button>
        ) : null}
        {pending?.kind === "authenticate" ? (
          <p className="agent-auth-progress" role="status">Waiting for Agent sign-in…</p>
        ) : null}
        {error ? <p className="agent-auth-error" role="alert">{error}</p> : null}
        {lastResponse ? (
          <RawJson
            label={`${lastResponse.kind === "authenticate" ? "Authenticate" : "Logout"} response`}
            value={lastResponse.response}
          />
        ) : null}
      </div>
    </details>
  );
}

function AuthMethodButtons({
  methods,
  pending,
  disabled,
  firstActionRef,
  onAuthenticate,
  compact = false,
}: {
  methods: AuthMethod[];
  pending?: PendingAuthOperation;
  disabled: boolean;
  firstActionRef?: RefObject<HTMLButtonElement | null>;
  onAuthenticate: (methodId: string) => void;
  compact?: boolean;
}) {
  return (
    <div className={compact ? "agent-auth-methods compact" : "agent-auth-methods"}>
      {methods.map((method, index) => {
        const terminal = "type" in method && method.type === "terminal";
        const active = (pending?.kind === "authenticate" || pending?.kind === "terminal") &&
          pending.methodId === method.id;
        return (
          <button
            type="button"
            key={method.id}
            ref={index === 0 ? firstActionRef : undefined}
            disabled={disabled || pending != null}
            aria-label={`Authenticate with ${method.name}`}
            onClick={() => onAuthenticate(method.id)}
          >
            <span>
              <strong>{method.name}</strong>
              <small>{terminal
                ? method.description || "Open the Agent-provided interactive login terminal"
                : method.description || "Continue with this Agent-provided method"}</small>
            </span>
            <span>{active
              ? terminal ? "Open…" : "Waiting…"
              : terminal ? compact ? "Open" : "Open terminal"
              : compact ? "Use" : "Continue"}</span>
          </button>
        );
      })}
    </div>
  );
}

function authStatusLabel(status: AgentAuthStatus): string {
  if (status === "required") return "Required";
  if (status === "authenticated") return "Signed in";
  if (status === "logged_out") return "Signed out";
  return "Agent managed";
}
