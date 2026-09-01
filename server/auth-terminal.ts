import type { AuthMethodTerminal } from "@agentclientprotocol/sdk";
import { spawn, type IPty } from "node-pty";

const MAX_AUTH_ARGUMENTS = 256;
const MAX_AUTH_ARGUMENT_LENGTH = 16_384;
const MAX_AUTH_ENVIRONMENT_ENTRIES = 256;
const MAX_AUTH_ENVIRONMENT_NAME_LENGTH = 256;
const MAX_AUTH_ENVIRONMENT_VALUE_LENGTH = 65_536;
const MAX_AUTH_ENVIRONMENT_BYTES = 1_000_000;
const MAX_AUTH_TERMINAL_OUTPUT_BYTES = 4_000_000;
const MAX_AUTH_TERMINAL_EVENT_CHARS = 32_768;
const ENVIRONMENT_NAME = /^[A-Za-z_][A-Za-z0-9_]*$/;

export interface AuthTerminalExit {
  requestId: string;
  methodId: string;
  status: "succeeded" | "failed" | "cancelled";
  exitCode: number | null;
  signal?: number;
  message?: string;
}

interface ActiveAuthTerminal {
  requestId: string;
  methodId: string;
  pty: IPty;
  outputBytes: number;
  cancelled: boolean;
  failureMessage?: string;
  settled: boolean;
}

export class AuthTerminalManager {
  private active?: ActiveAuthTerminal;

  constructor(
    private readonly onOutput: (requestId: string, data: string) => void,
    private readonly onExit: (result: AuthTerminalExit) => void,
  ) {}

  start(options: {
    requestId: string;
    method: AuthMethodTerminal & { type: "terminal" };
    command: [string, ...string[]];
    cwd: string;
    env?: NodeJS.ProcessEnv;
    cols: number;
    rows: number;
  }): void {
    if (this.active) throw new Error("An Agent terminal authentication is already running");
    validateTerminalAuthMethod(options.method);
    const [program, ...baseArgs] = options.command;
    const env: NodeJS.ProcessEnv = {
      ...(options.env ?? process.env),
      TERM: "xterm-256color",
      ...options.method.env,
    };
    const pty = spawn(program, [...baseArgs, ...(options.method.args ?? [])], {
      name: "xterm-256color",
      cols: options.cols,
      rows: options.rows,
      cwd: options.cwd,
      env,
    });
    const active: ActiveAuthTerminal = {
      requestId: options.requestId,
      methodId: options.method.id,
      pty,
      outputBytes: 0,
      cancelled: false,
      settled: false,
    };
    this.active = active;
    pty.onData((data) => this.handleOutput(active, data));
    pty.onExit(({ exitCode, signal }) => this.finish(active, exitCode, signal));
  }

  write(requestId: string, data: string): void {
    const active = this.requireActive(requestId);
    active.pty.write(data);
  }

  resize(requestId: string, cols: number, rows: number): void {
    const active = this.requireActive(requestId);
    active.pty.resize(cols, rows);
  }

  cancel(requestId: string): void {
    const active = this.requireActive(requestId);
    active.cancelled = true;
    try {
      active.pty.kill();
    } catch (error) {
      this.finish(active, null, undefined, `Could not stop terminal authentication: ${errorMessage(error)}`);
    }
  }

  close(): void {
    const active = this.active;
    if (!active) return;
    active.cancelled = true;
    try {
      active.pty.kill();
    } catch {
      this.active = undefined;
      active.settled = true;
    }
  }

  private requireActive(requestId: string): ActiveAuthTerminal {
    const active = this.active;
    if (!active || active.requestId !== requestId || active.settled) {
      throw new Error("Agent terminal authentication request is no longer active");
    }
    return active;
  }

  private handleOutput(active: ActiveAuthTerminal, data: string): void {
    if (this.active !== active || active.settled) return;
    const bytes = Buffer.byteLength(data, "utf8");
    if (active.outputBytes + bytes > MAX_AUTH_TERMINAL_OUTPUT_BYTES) {
      active.failureMessage = `Terminal authentication output exceeded ${MAX_AUTH_TERMINAL_OUTPUT_BYTES} bytes`;
      try {
        active.pty.kill();
      } catch (error) {
        this.finish(active, null, undefined, `${active.failureMessage}: ${errorMessage(error)}`);
      }
      return;
    }
    active.outputBytes += bytes;
    for (let offset = 0; offset < data.length; offset += MAX_AUTH_TERMINAL_EVENT_CHARS) {
      this.onOutput(active.requestId, data.slice(offset, offset + MAX_AUTH_TERMINAL_EVENT_CHARS));
    }
  }

  private finish(
    active: ActiveAuthTerminal,
    exitCode: number | null,
    signal?: number,
    forcedMessage?: string,
  ): void {
    if (this.active !== active || active.settled) return;
    active.settled = true;
    this.active = undefined;
    const normalizedExitCode = Number.isSafeInteger(exitCode) &&
      Number(exitCode) >= 0 &&
      Number(exitCode) <= 0xffff_ffff
      ? Number(exitCode)
      : null;
    const message = forcedMessage ?? active.failureMessage;
    const status = active.cancelled
      ? "cancelled"
      : normalizedExitCode === 0 && message == null
        ? "succeeded"
        : "failed";
    this.onExit({
      requestId: active.requestId,
      methodId: active.methodId,
      status,
      exitCode: normalizedExitCode,
      ...(signal == null ? {} : { signal }),
      ...(message == null
        ? status === "failed"
          ? { message: normalizedExitCode == null
              ? "Terminal authentication ended without an exit status"
              : `Terminal authentication exited with status ${normalizedExitCode}` }
          : {}
        : { message }),
    });
  }
}

export function validateTerminalAuthMethod(
  method: AuthMethodTerminal & { type: "terminal" },
): void {
  const args = method.args ?? [];
  if (!Array.isArray(args) || args.length > MAX_AUTH_ARGUMENTS) {
    throw new Error(`Terminal authentication may provide at most ${MAX_AUTH_ARGUMENTS} arguments`);
  }
  for (const argument of args) {
    if (
      typeof argument !== "string" ||
      argument.includes("\0") ||
      argument.length > MAX_AUTH_ARGUMENT_LENGTH
    ) {
      throw new Error(
        `Terminal authentication arguments must be NUL-free strings of at most ${MAX_AUTH_ARGUMENT_LENGTH} characters`,
      );
    }
  }
  const environment = method.env ?? {};
  if (typeof environment !== "object" || environment == null || Array.isArray(environment)) {
    throw new Error("Terminal authentication environment must be an object");
  }
  const entries = Object.entries(environment);
  if (entries.length > MAX_AUTH_ENVIRONMENT_ENTRIES) {
    throw new Error(
      `Terminal authentication may provide at most ${MAX_AUTH_ENVIRONMENT_ENTRIES} environment variables`,
    );
  }
  let bytes = 0;
  for (const [name, value] of entries) {
    if (
      name.length === 0 ||
      name.length > MAX_AUTH_ENVIRONMENT_NAME_LENGTH ||
      !ENVIRONMENT_NAME.test(name)
    ) {
      throw new Error(`Terminal authentication has an invalid environment variable name: ${name}`);
    }
    if (
      typeof value !== "string" ||
      value.includes("\0") ||
      value.length > MAX_AUTH_ENVIRONMENT_VALUE_LENGTH
    ) {
      throw new Error(
        `Terminal authentication environment values must be NUL-free strings of at most ${MAX_AUTH_ENVIRONMENT_VALUE_LENGTH} characters`,
      );
    }
    bytes += Buffer.byteLength(name, "utf8") + Buffer.byteLength(value, "utf8");
  }
  if (bytes > MAX_AUTH_ENVIRONMENT_BYTES) {
    throw new Error(`Terminal authentication environment exceeds ${MAX_AUTH_ENVIRONMENT_BYTES} bytes`);
  }
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
