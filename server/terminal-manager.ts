import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { randomUUID } from "node:crypto";
import {
  RequestError,
  type CreateTerminalRequest,
  type CreateTerminalResponse,
  type KillTerminalRequest,
  type KillTerminalResponse,
  type ReleaseTerminalRequest,
  type ReleaseTerminalResponse,
  type TerminalOutputRequest,
  type TerminalOutputResponse,
  type WaitForTerminalExitRequest,
  type WaitForTerminalExitResponse,
} from "@agentclientprotocol/sdk";
import type { WorkspaceFileSystem } from "./safe-fs.js";
import type { TerminalSnapshot } from "../shared/bridge.js";

interface TerminalWaiter {
  resolve: (status: WaitForTerminalExitResponse) => void;
  reject: (error: Error) => void;
  removeAbort?: () => void;
}

interface TerminalState {
  terminalId: string;
  sessionId: string;
  process: ChildProcessWithoutNullStreams;
  chunks: Buffer[];
  bytes: number;
  limit: number;
  truncated: boolean;
  exitStatus?: WaitForTerminalExitResponse;
  released: boolean;
  waiters: TerminalWaiter[];
}

export const MAX_TERMINAL_OUTPUT_BYTES = 1_000_000;
const DEFAULT_TERMINAL_OUTPUT_BYTES = 200_000;
const MAX_TERMINALS = 32;
const MAX_EXIT_WAITERS = 100;
const MAX_TERMINAL_CREATE_BYTES = 4_000_000;
const MAX_TERMINAL_COMMAND_LENGTH = 16_384;
const MAX_TERMINAL_ARGS = 4_096;
const MAX_TERMINAL_ARG_LENGTH = 65_536;
const MAX_TERMINAL_ENV = 256;
const MAX_TERMINAL_ENV_NAME_LENGTH = 256;
const MAX_TERMINAL_ENV_VALUE_LENGTH = 65_536;

export class TerminalManager {
  private readonly terminals = new Map<string, TerminalState>();

  constructor(
    private readonly fileSystem: WorkspaceFileSystem,
    private readonly onSnapshot: (snapshot: TerminalSnapshot) => void = () => undefined,
  ) {}

  async create(params: CreateTerminalRequest): Promise<CreateTerminalResponse> {
    validateCreateRequest(params);
    if (this.terminals.size >= MAX_TERMINALS) {
      throw new Error(`Terminal limit reached (${MAX_TERMINALS})`);
    }
    const limit = outputLimit(params.outputByteLimit);
    const terminalId = randomUUID();
    const cwd = await this.fileSystem.checkedDirectory(params.cwd);
    const additions = Object.fromEntries(
      (params.env ?? []).map(({ name, value }) => [name, value]),
    );
    const child = spawn(params.command, params.args ?? [], {
      cwd,
      env: { ...process.env, ...additions },
      stdio: ["pipe", "pipe", "pipe"],
    });
    const state: TerminalState = {
      terminalId,
      sessionId: params.sessionId,
      process: child,
      chunks: [],
      bytes: 0,
      limit,
      truncated: false,
      released: false,
      waiters: [],
    };
    this.terminals.set(terminalId, state);
    try {
      await this.attachProcess(state, child);
    } catch (error) {
      this.terminals.delete(terminalId);
      if (!shouldRetryAsShellCommand(params, error)) {
        throw terminalSpawnError(error);
      }

      // ACP v1 models `command` plus `args` as an argv launch. Goose 1.48.0
      // currently sends its developer-shell source as one command string. Keep
      // the strict launch first so valid executable paths (including spaces)
      // retain argv semantics, then interoperate with compound shell commands
      // only when that launch failed with ENOENT and no args were supplied.
      const shellChild = spawn(params.command, [], {
        cwd,
        env: { ...process.env, ...additions },
        stdio: ["pipe", "pipe", "pipe"],
        shell: true,
      });
      state.process = shellChild;
      this.terminals.set(terminalId, state);
      try {
        await this.attachProcess(state, shellChild);
      } catch (shellError) {
        this.terminals.delete(terminalId);
        throw terminalSpawnError(shellError);
      }
    }
    this.emitSnapshot(state);

    return { terminalId };
  }

  private attachProcess(
    state: TerminalState,
    child: ChildProcessWithoutNullStreams,
  ): Promise<void> {
    const append = (chunk: Buffer) => this.append(state, chunk);
    child.stdout.on("data", append);
    child.stderr.on("data", append);
    let failedToSpawn = false;
    let didSpawn = false;
    const spawned = new Promise<void>((resolve, reject) => {
      child.once("spawn", () => {
        didSpawn = true;
        resolve();
      });
      child.once("error", (error) => {
        if (didSpawn) {
          append(Buffer.from(`${error.message}\n`));
          return;
        }
        failedToSpawn = true;
        reject(error);
      });
    });
    child.on("close", (exitCode, signal) => {
      if (failedToSpawn) return;
      const status = normalizeExitStatus(exitCode, signal);
      state.exitStatus = status;
      this.emitSnapshot(state);
      for (const waiter of state.waiters.splice(0)) {
        waiter.removeAbort?.();
        waiter.resolve(status);
      }
    });
    return spawned;
  }

  output(params: TerminalOutputRequest): TerminalOutputResponse {
    const state = this.require(params.terminalId, params.sessionId);
    return {
      output: Buffer.concat(state.chunks, state.bytes).toString("utf8"),
      truncated: state.truncated,
      exitStatus: state.exitStatus ?? null,
    };
  }

  waitForExit(
    params: WaitForTerminalExitRequest,
    signal?: AbortSignal,
  ): Promise<WaitForTerminalExitResponse> {
    const state = this.require(params.terminalId, params.sessionId);
    if (state.exitStatus) return Promise.resolve(state.exitStatus);
    if (signal?.aborted) return Promise.reject(RequestError.requestCancelled());
    if (state.waiters.length >= MAX_EXIT_WAITERS) {
      throw new Error(`Too many wait_for_exit requests for terminal ${params.terminalId}`);
    }
    return new Promise((resolve, reject) => {
      const waiter: TerminalWaiter = { resolve, reject };
      state.waiters.push(waiter);
      if (signal) {
        const onAbort = () => {
          const index = state.waiters.indexOf(waiter);
          if (index < 0) return;
          state.waiters.splice(index, 1);
          waiter.removeAbort?.();
          reject(RequestError.requestCancelled());
        };
        signal.addEventListener("abort", onAbort, { once: true });
        waiter.removeAbort = () => signal.removeEventListener("abort", onAbort);
        if (signal.aborted) onAbort();
      }
    });
  }

  kill(params: KillTerminalRequest): KillTerminalResponse {
    this.require(params.terminalId, params.sessionId).process.kill();
    return {};
  }

  release(params: ReleaseTerminalRequest): ReleaseTerminalResponse {
    const state = this.require(params.terminalId, params.sessionId);
    state.released = true;
    if (state.exitStatus == null) state.process.kill();
    this.emitSnapshot(state);
    this.terminals.delete(params.terminalId);
    return {};
  }

  assertReference(terminalId: string, sessionId: string): void {
    this.require(terminalId, sessionId);
  }

  releaseSession(sessionId: string): void {
    for (const [terminalId, state] of this.terminals) {
      if (state.sessionId !== sessionId) continue;
      state.released = true;
      if (state.exitStatus == null) state.process.kill();
      this.emitSnapshot(state);
      this.terminals.delete(terminalId);
    }
  }

  close(): void {
    for (const state of this.terminals.values()) {
      state.released = true;
      if (state.exitStatus == null) state.process.kill();
      this.emitSnapshot(state);
    }
    this.terminals.clear();
  }

  private require(terminalId: string, sessionId: string): TerminalState {
    const terminal = this.terminals.get(terminalId);
    if (!terminal) throw new Error(`Unknown terminal: ${terminalId}`);
    if (terminal.sessionId !== sessionId) {
      throw new Error(`Terminal ${terminalId} does not belong to session ${sessionId}`);
    }
    return terminal;
  }

  private append(state: TerminalState, chunk: Buffer): void {
    state.chunks.push(chunk);
    state.bytes += chunk.byteLength;
    while (state.bytes > state.limit && state.chunks.length > 0) {
      const first = state.chunks[0];
      const overflow = state.bytes - state.limit;
      if (first.byteLength <= overflow) {
        state.chunks.shift();
        state.bytes -= first.byteLength;
      } else {
        state.chunks[0] = first.subarray(overflow);
        state.bytes -= overflow;
      }
      state.truncated = true;
    }
    // ACP requires truncation at a character boundary. If the byte cut landed in
    // the middle of a UTF-8 sequence, discard its remaining continuation bytes.
    while (
      state.chunks.length > 0 &&
      state.chunks[0].length > 0 &&
      (state.chunks[0][0] & 0xc0) === 0x80
    ) {
      state.chunks[0] = state.chunks[0].subarray(1);
      state.bytes -= 1;
      if (state.chunks[0].length === 0) state.chunks.shift();
    }
    this.emitSnapshot(state);
  }

  private emitSnapshot(state: TerminalState): void {
    this.onSnapshot({
      sessionId: state.sessionId,
      terminalId: state.terminalId,
      output: Buffer.concat(state.chunks, state.bytes).toString("utf8"),
      truncated: state.truncated,
      exitStatus: state.exitStatus ?? null,
      released: state.released,
    });
  }
}

function normalizeExitStatus(
  exitCode: number | null,
  signal: NodeJS.Signals | null,
): WaitForTerminalExitResponse {
  return {
    exitCode:
      exitCode != null &&
      Number.isSafeInteger(exitCode) &&
      exitCode >= 0 &&
      exitCode <= 0xffff_ffff
        ? exitCode
        : null,
    signal,
  };
}

function shouldRetryAsShellCommand(
  params: CreateTerminalRequest,
  error: unknown,
): boolean {
  if ((params.args?.length ?? 0) !== 0) return false;
  if ((error as NodeJS.ErrnoException | null)?.code !== "ENOENT") return false;
  return /\s/u.test(params.command) || [
    "|", "&", ";", "<", ">", "(", ")", "$", "`", "\\", "*", "?", "[", "]", "{", "}",
  ].some((character) => params.command.includes(character));
}

function terminalSpawnError(error: unknown): Error {
  const message = error instanceof Error ? error.message : String(error);
  return new Error(`Failed to start terminal command: ${message}`);
}

function validateCreateRequest(params: CreateTerminalRequest): void {
  if (Buffer.byteLength(JSON.stringify(params), "utf8") > MAX_TERMINAL_CREATE_BYTES) {
    throw new Error(`Terminal create request exceeds ${MAX_TERMINAL_CREATE_BYTES} bytes`);
  }
  if (
    params.command.length === 0 ||
    params.command.length > MAX_TERMINAL_COMMAND_LENGTH ||
    params.command.includes("\0")
  ) {
    throw new Error(
      `Terminal command must contain between 1 and ${MAX_TERMINAL_COMMAND_LENGTH} characters without NUL bytes`,
    );
  }
  const args = params.args ?? [];
  if (args.length > MAX_TERMINAL_ARGS) {
    throw new Error(`Terminal command exceeds ${MAX_TERMINAL_ARGS} arguments`);
  }
  if (args.some((arg) => arg.length > MAX_TERMINAL_ARG_LENGTH || arg.includes("\0"))) {
    throw new Error(
      `Terminal arguments must not exceed ${MAX_TERMINAL_ARG_LENGTH} characters or contain NUL bytes`,
    );
  }
  const env = params.env ?? [];
  if (env.length > MAX_TERMINAL_ENV) {
    throw new Error(`Terminal environment exceeds ${MAX_TERMINAL_ENV} variables`);
  }
  const names = new Set<string>();
  for (const variable of env) {
    if (
      variable.name.length === 0 ||
      variable.name.length > MAX_TERMINAL_ENV_NAME_LENGTH ||
      variable.name.includes("=") ||
      variable.name.includes("\0")
    ) {
      throw new Error("Terminal environment contains an invalid variable name");
    }
    if (
      variable.value.length > MAX_TERMINAL_ENV_VALUE_LENGTH ||
      variable.value.includes("\0")
    ) {
      throw new Error(
        `Terminal environment values must not exceed ${MAX_TERMINAL_ENV_VALUE_LENGTH} characters or contain NUL bytes`,
      );
    }
    if (names.has(variable.name)) {
      throw new Error(`Terminal environment contains duplicate variable: ${variable.name}`);
    }
    names.add(variable.name);
  }
}

function outputLimit(requested: number | null | undefined): number {
  if (requested == null) return DEFAULT_TERMINAL_OUTPUT_BYTES;
  if (!Number.isSafeInteger(requested) || requested < 0) {
    throw new Error("outputByteLimit must be a non-negative safe integer");
  }
  return Math.min(requested, MAX_TERMINAL_OUTPUT_BYTES);
}
