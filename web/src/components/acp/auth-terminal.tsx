import type { AuthMethod } from "@agentclientprotocol/sdk";
import type { TFunction } from "i18next";
import { FitAddon } from "@xterm/addon-fit";
import { Terminal } from "@xterm/xterm";
import "@xterm/xterm/css/xterm.css";
import { RotateCcw, SquareTerminal, X } from "lucide-react";
import { useEffect, useRef } from "react";
import { useTranslation } from "../../i18n";
import type { AuthTerminalState } from "../../lib/state";

export function AuthTerminalCard({
  terminalState,
  method,
  onInput,
  onResize,
  onCancel,
  onRetry,
  onDismiss,
}: {
  terminalState: AuthTerminalState;
  method: AuthMethod;
  onInput: (requestId: string, data: string) => void;
  onResize: (requestId: string, cols: number, rows: number) => void;
  onCancel: (requestId: string) => void;
  onRetry: (methodId: string) => void;
  onDismiss: () => void;
}) {
  const { t } = useTranslation("workspace");
  const host = useRef<HTMLDivElement>(null);
  const xterm = useRef<Terminal | undefined>(undefined);
  const writtenOutput = useRef("");
  const active = terminalState.status === "starting" || terminalState.status === "running";
  const callbacks = useRef({ onInput, onResize, active });
  callbacks.current = { onInput, onResize, active };

  useEffect(() => {
    const element = host.current;
    if (!element) return;
    const instance = new Terminal({
      allowProposedApi: false,
      cursorBlink: true,
      cursorStyle: "bar",
      convertEol: false,
      fontFamily: "ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace",
      fontSize: 12,
      lineHeight: 1.25,
      scrollback: 5_000,
      theme: {
        background: "#1c1c1e",
        foreground: "#e5e5ea",
        cursor: "#e5e5ea",
        cursorAccent: "#1c1c1e",
        selectionBackground: "#4c90da66",
        black: "#1c1c1e",
        red: "#ef8585",
        green: "#8bd49c",
        yellow: "#e5c07b",
        blue: "#8ab4ed",
        magenta: "#c8a0df",
        cyan: "#80c7cd",
        white: "#d1d1d6",
        brightBlack: "#8e8e93",
        brightRed: "#f5a1a1",
        brightGreen: "#a7e3b5",
        brightYellow: "#f0d496",
        brightBlue: "#a4c8f5",
        brightMagenta: "#d9b7ed",
        brightCyan: "#a0dce0",
        brightWhite: "#f2f2f7",
      },
    });
    const fit = new FitAddon();
    instance.loadAddon(fit);
    instance.open(element);
    xterm.current = instance;
    writtenOutput.current = "";
    const fitAndReport = () => {
      try {
        fit.fit();
      } catch {
        return;
      }
      const cols = Math.min(500, Math.max(2, instance.cols));
      const rows = Math.min(300, Math.max(2, instance.rows));
      if (callbacks.current.active) {
        callbacks.current.onResize(terminalState.requestId, cols, rows);
      }
    };
    const frame = requestAnimationFrame(() => {
      fitAndReport();
      instance.focus();
    });
    const data = instance.onData((value) => {
      if (callbacks.current.active) callbacks.current.onInput(terminalState.requestId, value);
    });
    const observer = typeof ResizeObserver === "undefined"
      ? undefined
      : new ResizeObserver(fitAndReport);
    observer?.observe(element);
    return () => {
      cancelAnimationFrame(frame);
      observer?.disconnect();
      data.dispose();
      instance.dispose();
      xterm.current = undefined;
      writtenOutput.current = "";
    };
  }, [terminalState.requestId]);

  useEffect(() => {
    xterm.current?.textarea?.setAttribute("aria-label", t("terminal.inputLabel"));
  }, [t, terminalState.requestId]);

  useEffect(() => {
    const instance = xterm.current;
    if (!instance) return;
    if (terminalState.output.startsWith(writtenOutput.current)) {
      const addition = terminalState.output.slice(writtenOutput.current.length);
      if (addition) instance.write(addition);
    } else {
      instance.reset();
      if (terminalState.output) instance.write(terminalState.output);
    }
    writtenOutput.current = terminalState.output;
  }, [terminalState.output]);

  useEffect(() => {
    const instance = xterm.current;
    if (!instance) return;
    instance.options.disableStdin = !active;
    if (active) instance.focus();
  }, [active]);

  return (
    <section className="auth-terminal-card" aria-labelledby="auth-terminal-heading">
      <header>
        <span className="auth-terminal-mark"><SquareTerminal size={15} /></span>
        <div>
          <span className="eyebrow">{t("terminal.title")}</span>
          <h2 id="auth-terminal-heading">{method.name}</h2>
        </div>
        <span className={`auth-terminal-status status-${terminalState.status}`} role="status">
          {terminalStatusLabel(terminalState.status, t)}
        </span>
      </header>
      {method.description ? <p className="auth-terminal-description">{method.description}</p> : null}
      {terminalState.truncated ? (
        <p className="auth-terminal-notice">{t("terminal.truncatedOutput")}</p>
      ) : null}
      <div className="auth-terminal-host" ref={host} />
      {terminalState.message ? (
        <p className="auth-terminal-error" role={terminalState.status === "failed" ? "alert" : undefined}>
          {terminalState.message}
        </p>
      ) : null}
      <footer>
        <span>{t("terminal.ephemeralNotice")}</span>
        <div>
          {active ? (
            <button type="button" className="auth-terminal-cancel" onClick={() => onCancel(terminalState.requestId)}>
              <X size={12} /> {t("cancel")}
            </button>
          ) : terminalState.status === "failed" || terminalState.status === "cancelled" ? (
            <button type="button" onClick={() => onRetry(method.id)}>
              <RotateCcw size={12} /> {t("terminal.retry")}
            </button>
          ) : null}
          {!active && terminalState.status !== "succeeded" ? (
            <button type="button" onClick={onDismiss}>{t("terminal.close")}</button>
          ) : null}
        </div>
      </footer>
    </section>
  );
}

function terminalStatusLabel(status: AuthTerminalState["status"], t: TFunction<"workspace">): string {
  if (status === "starting") return t("terminal.status.opening");
  if (status === "running") return t("terminal.status.interactive");
  if (status === "succeeded") return t("terminal.status.reconnecting");
  if (status === "cancelled") return t("terminal.status.cancelled");
  return t("terminal.status.failed");
}
