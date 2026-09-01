import type { AuthMethod } from "@agentclientprotocol/sdk";
import { FitAddon } from "@xterm/addon-fit";
import { Terminal } from "@xterm/xterm";
import "@xterm/xterm/css/xterm.css";
import { RotateCcw, SquareTerminal, X } from "lucide-react";
import { useEffect, useRef } from "react";
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
        background: "#171914",
        foreground: "#e3e5dc",
        cursor: "#b7c98a",
        cursorAccent: "#171914",
        selectionBackground: "#6f7f4d66",
        black: "#171914",
        red: "#dc8075",
        green: "#a8c477",
        yellow: "#d6ba73",
        blue: "#87a7c5",
        magenta: "#ba96bd",
        cyan: "#7fbbb2",
        white: "#d9dcd2",
        brightBlack: "#777b70",
        brightRed: "#ec9288",
        brightGreen: "#bbd68c",
        brightYellow: "#e8cc85",
        brightBlue: "#9ab9d5",
        brightMagenta: "#cca9cf",
        brightCyan: "#92cec5",
        brightWhite: "#f2f3ed",
      },
    });
    const fit = new FitAddon();
    instance.loadAddon(fit);
    instance.open(element);
    instance.textarea?.setAttribute("aria-label", "Agent terminal authentication input");
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
          <span className="eyebrow">Agent terminal authentication</span>
          <h2 id="auth-terminal-heading">{method.name}</h2>
        </div>
        <span className={`auth-terminal-status status-${terminalState.status}`} role="status">
          {terminalStatusLabel(terminalState.status)}
        </span>
      </header>
      {method.description ? <p className="auth-terminal-description">{method.description}</p> : null}
      {terminalState.truncated ? (
        <p className="auth-terminal-notice">Earlier terminal output was truncated.</p>
      ) : null}
      <div className="auth-terminal-host" ref={host} />
      {terminalState.message ? (
        <p className="auth-terminal-error" role={terminalState.status === "failed" ? "alert" : undefined}>
          {terminalState.message}
        </p>
      ) : null}
      <footer>
        <span>This terminal is ephemeral; the Agent process controls input echo and credential handling.</span>
        <div>
          {active ? (
            <button type="button" className="auth-terminal-cancel" onClick={() => onCancel(terminalState.requestId)}>
              <X size={12} /> Cancel
            </button>
          ) : terminalState.status === "failed" || terminalState.status === "cancelled" ? (
            <button type="button" onClick={() => onRetry(method.id)}>
              <RotateCcw size={12} /> Retry
            </button>
          ) : null}
          {!active && terminalState.status !== "succeeded" ? (
            <button type="button" onClick={onDismiss}>Close</button>
          ) : null}
        </div>
      </footer>
    </section>
  );
}

function terminalStatusLabel(status: AuthTerminalState["status"]): string {
  if (status === "starting") return "Opening…";
  if (status === "running") return "Interactive";
  if (status === "succeeded") return "Signed in · reconnecting…";
  if (status === "cancelled") return "Cancelled";
  return "Failed";
}
