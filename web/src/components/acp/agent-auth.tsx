import type { AuthMethod } from "@agentclientprotocol/sdk";
import type { TFunction } from "i18next";
import { ChevronRight, KeyRound, LogOut, ShieldCheck } from "lucide-react";
import { useEffect, useRef, type RefObject } from "react";
import { useTranslation } from "../../i18n";
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
  const { t } = useTranslation("workspace");
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
        <span className="eyebrow">{t("auth.eyebrow")}</span>
        <h2 id="agent-auth-heading">
          {status === "logged_out" ? t("auth.signedOutOf", { agentName }) : t("auth.signInToContinue")}
        </h2>
        <p>
          {t("auth.description", { agentName })}
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
        <p className="agent-auth-progress" role="status">{t("auth.waitingToFinish")}</p>
      ) : null}
      {error ? <p className="agent-auth-error" role="alert">{error}</p> : null}
      <footer><ShieldCheck size={12} /> {t("auth.flowNotice")}</footer>
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
  const { t } = useTranslation("workspace");
  return (
    <details className="sidebar-details agent-auth-details">
      <summary>
        <KeyRound size={13} /> {t("auth.title")}
        <span className={`agent-auth-state state-${status}`}>{authStatusLabel(status, t)}</span>
        <ChevronRight size={12} />
      </summary>
      <div className="agent-auth-sidebar">
        <p>{t("auth.sidebarDescription")}</p>
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
            {pending?.kind === "logout" ? t("auth.signingOut") : t("auth.signOut")}
          </button>
        ) : null}
        {pending?.kind === "authenticate" ? (
          <p className="agent-auth-progress" role="status">{t("auth.waitingForSignIn")}</p>
        ) : null}
        {error ? <p className="agent-auth-error" role="alert">{error}</p> : null}
        {lastResponse ? (
          <RawJson
            label={lastResponse.kind === "authenticate" ? t("auth.authenticateResponse") : t("auth.logoutResponse")}
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
  const { t } = useTranslation("workspace");
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
            aria-label={t("auth.authenticateWith", { method: method.name })}
            onClick={() => onAuthenticate(method.id)}
          >
            <span>
              <strong>{method.name}</strong>
              <small>{terminal
                ? method.description || t("auth.terminalMethodDescription")
                : method.description || t("auth.methodDescription")}</small>
            </span>
            <span>{active
              ? terminal ? t("auth.opening") : t("auth.waiting")
              : terminal ? compact ? t("auth.open") : t("auth.openTerminal")
              : compact ? t("auth.use") : t("auth.continue")}</span>
          </button>
        );
      })}
    </div>
  );
}

function authStatusLabel(status: AgentAuthStatus, t: TFunction<"workspace">): string {
  if (status === "required") return t("auth.status.required");
  if (status === "authenticated") return t("auth.status.signedIn");
  if (status === "logged_out") return t("auth.status.signedOut");
  return t("auth.status.agentManaged");
}
