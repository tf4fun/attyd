import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { randomUUID } from "node:crypto";
import { basename } from "node:path";
import { Readable, Writable } from "node:stream";
import { pathToFileURL } from "node:url";
import * as acp from "@agentclientprotocol/sdk";
import { createHttpStream } from "@agentclientprotocol/sdk/experimental/http-client";
import { createWebSocketStream } from "@agentclientprotocol/sdk/experimental/ws-client";
import { WebSocket as NodeWebSocket, type WebSocket } from "ws";
import {
  MAX_BRIDGE_ERROR_DATA_BYTES,
  MAX_BRIDGE_MESSAGE_BYTES,
  parseClientCommand,
} from "../shared/bridge.js";
import type {
  AgentTransport,
  ClientCommand,
  ElicitationAbortReason,
  ServerEvent,
  TerminalSnapshot,
} from "../shared/bridge.js";
import type { NesDocumentState } from "../shared/bridge.js";
import {
  applyTextEdits,
  assertRange,
  fullOrIncrementalChange,
  offsetAt,
  positionAt,
  unifiedTextDiff,
} from "../shared/nes.js";
import { WorkspaceFileSystem } from "./safe-fs.js";
import { TerminalManager } from "./terminal-manager.js";
import {
  AuthTerminalManager,
  validateTerminalAuthMethod,
} from "./auth-terminal.js";
import {
  validateElicitationRequest,
  validateElicitationResponse,
} from "./elicitation-validation.js";
import {
  McpManager,
  parseConnectMcpRequest,
  parseDisconnectMcpRequest,
  parseMessageMcp,
} from "./mcp-manager.js";
import type { AcpMcpProvider } from "./options.js";
import {
  validateSessionConfigOptions,
  validateSessionControls,
} from "./session-validation.js";
import {
  createSessionUpdateValidationState,
  validateAndTrackSessionUpdate,
  type SessionUpdateValidationState,
} from "./session-update-validation.js";
import { validateSessionListPage } from "./session-list-validation.js";
import { limitNdjsonLineBytes } from "./limited-stream.js";
import { validatePermissionRequest } from "./permission-validation.js";
import { validatePromptResponse } from "./prompt-validation.js";

export interface AcpBridgeOptions {
  /** Defaults to stdio for direct embedders and existing tests. */
  transport?: AgentTransport;
  /** Process command for stdio; a single endpoint URL for HTTP or WebSocket. */
  command: [string, ...string[]];
  cwd: string;
  readOnly: boolean;
  env?: NodeJS.ProcessEnv;
  additionalDirectories?: string[];
  mcpServers?: acp.McpServer[];
  acpMcpProviders?: AcpMcpProvider[];
}

interface PendingPermission {
  sessionId: string;
  request: acp.RequestPermissionRequest;
  resolve: (response: acp.RequestPermissionResponse) => void;
}

interface PendingElicitation {
  request: acp.CreateElicitationRequest;
  resolve: (response: acp.CreateElicitationResponse) => void;
}

interface TrackedUrlElicitation {
  bridgeElicitationId: string;
  sessionId?: string;
  state: "pending" | "accepted";
}

interface TrackedSession {
  cwd: string;
  modes?: acp.SessionModeState | null;
  configOptions: acp.SessionConfigOption[];
  updateValidation: SessionUpdateValidationState;
}

interface PendingCreationReplay {
  updateValidation: SessionUpdateValidationState;
  notifications: acp.SessionNotification[];
  bytes: number;
}

interface TrackedNesDocument extends NesDocumentState {
  visibleRange?: acp.Range;
  lastFocusedMs?: number;
  lastAccessedMs: number;
}

interface PendingNesSuggestion {
  suggestion: acp.NesSuggestion;
  expectedText?: string;
}

interface TrackedNesSession {
  documents: Map<string, TrackedNesDocument>;
  editHistory: acp.NesEditHistoryEntry[];
  suggestions: Map<string, PendingNesSuggestion>;
  userActions: acp.NesUserAction[];
}

const MAX_TRACKED_SESSIONS = 32;
const MAX_PENDING_INTERACTIONS = 100;
const MAX_NES_SESSIONS = 8;
const MAX_NES_DOCUMENTS = 32;
const MAX_NES_SUGGESTIONS = 100;
const MAX_NES_EDITS = 10_000;
const MAX_DOCUMENT_BYTES = 2_000_000;
const MAX_NES_CONTEXT_BYTES = 4_000_000;
const MAX_NES_RESPONSE_BYTES = 3_000_000;
const MAX_NES_CONTEXT_ITEMS = 100;
const MAX_NES_HISTORY_ENTRY_BYTES = 512_000;
const MAX_NES_CONTEXT_FIELD_BYTES = Math.floor(MAX_NES_CONTEXT_BYTES / 2);
const MAX_AGENT_NDJSON_LINE_BYTES = 8_000_000;
const MAX_URL_ELICITATION_IDS = 10_000;
const MAX_BRIDGE_ERROR_DETAIL_CHARS = 4_096;
const MAX_BRIDGE_ERROR_MESSAGE_CHARS = 16_384;
const TERMINAL_SNAPSHOT_INTERVAL_MS = 33;
const MAX_TERMINAL_LIVE_SNAPSHOT_BYTES = 2_000_000;
const MAX_PENDING_CREATION_UPDATES = 10_000;
const MAX_PENDING_CREATION_REPLAY_BYTES = 1_000_000;
const MAX_BROWSER_RELAY_VALUE_BYTES = 4_000_000;
const MAX_AGENT_IDENTIFIER_LENGTH = 1_024;
const MAX_AUTH_METHODS = 100;
const MAX_AUTH_METHOD_NAME_LENGTH = 4_096;
const MAX_AUTH_METHOD_DESCRIPTION_LENGTH = 16_384;

export class AcpBridge {
  private child?: ChildProcessWithoutNullStreams;
  private connection?: acp.ClientConnection;
  private initialized?: acp.InitializeResponse;
  private readonly permissions = new Map<string, PendingPermission>();
  private readonly elicitations = new Map<string, PendingElicitation>();
  private readonly urlElicitations = new Map<string, TrackedUrlElicitation>();
  private readonly seenUrlElicitationIds = new Set<string>();
  private readonly prompts = new Set<string>();
  private readonly sessions = new Map<string, TrackedSession>();
  private readonly listedSessions = new Set<string>();
  private readonly listedSessionInfo = new Map<string, acp.SessionInfo>();
  private readonly listedCursors = new Set<string>();
  private listedNextCursor?: string;
  private listedSessionBytes = 0;
  private sessionListInFlight = false;
  private authOperation?: "authenticate" | "terminal authentication" | "logout";
  private pendingSessionCreations = 0;
  private readonly pendingSessionForks = new Set<string>();
  private readonly pendingSessionClosures = new Set<string>();
  private readonly pendingSessionControls = new Set<string>();
  private readonly pendingSessionDeletions = new Set<string>();
  private readonly pendingSessionAttachments = new Set<string>();
  private readonly pendingSessionUpdates = new Map<string, SessionUpdateValidationState>();
  private readonly pendingCreationReplays = new Map<string, PendingCreationReplay>();
  private pendingCreationReplayCount = 0;
  private pendingCreationReplayBytes = 0;
  private readonly nesSessions = new Map<string, TrackedNesSession>();
  private readonly nesQueues = new Map<string, Promise<void>>();
  private nesStarting = false;
  private readonly fileSystem: WorkspaceFileSystem;
  private readonly terminals: TerminalManager;
  private readonly authTerminal: AuthTerminalManager;
  private readonly pendingTerminalSnapshots = new Map<string, TerminalSnapshot>();
  private readonly terminalSnapshotBytes = new Map<string, number>();
  private terminalSnapshotTimer?: ReturnType<typeof setTimeout>;
  private readonly mcp: McpManager;
  private readonly additionalDirectories: string[];
  private readonly mcpServers: acp.McpServer[];
  private closed = false;

  private get transport(): AgentTransport {
    return this.options.transport ?? "stdio";
  }

  constructor(
    private readonly socket: WebSocket,
    private readonly options: AcpBridgeOptions,
  ) {
    this.additionalDirectories = (this.transport === "stdio"
      ? [...new Set(options.additionalDirectories ?? [])]
      : [])
      .filter((path) => path !== options.cwd);
    this.mcpServers = options.mcpServers ?? [];
    const acpMcpProviders = options.acpMcpProviders ?? [];
    validateAcpMcpProviders(this.mcpServers, acpMcpProviders);
    this.fileSystem = new WorkspaceFileSystem(
      options.cwd,
      options.readOnly,
      this.additionalDirectories,
    );
    this.terminals = new TerminalManager(
      this.fileSystem,
      (snapshot) => this.queueTerminalSnapshot(snapshot),
    );
    this.authTerminal = new AuthTerminalManager(
      (requestId, data) => {
        this.send({ type: "bridge/auth_terminal_output", requestId, data });
      },
      (result) => {
        this.authOperation = undefined;
        this.send({ type: "bridge/auth_terminal_exited", ...result });
      },
    );
    this.mcp = new McpManager({
      cwd: options.cwd,
      providers: acpMcpProviders,
      onServerRequest: (request) => this.requireAgent().request<unknown, acp.MessageMcpRequest>(
        acp.AGENT_METHODS.mcp_message,
        request,
      ),
      onServerNotification: (notification) => this.requireAgent().notify<acp.MessageMcpNotification>(
        acp.AGENT_METHODS.mcp_message,
        notification,
      ),
      onConnection: (action, connection) => {
        this.send({ type: "acp/mcp_connection", action, ...connection });
      },
      onActivity: (activity) => this.send({ type: "acp/mcp_message", ...activity }),
      onStderr: (chunk) => this.send({ type: "bridge/stderr", chunk }),
    });
  }

  async start(): Promise<void> {
    try {
      await this.startAgent();
    } catch (error) {
      if (!this.closed) {
        try {
          this.fail(error);
        } finally {
          this.close();
        }
      }
      throw error;
    }
  }

  private async startAgent(): Promise<void> {
    this.send({
      type: "bridge/hello",
      transport: this.transport,
      command: this.options.command,
      cwd: this.transport === "stdio" ? this.options.cwd : "",
      readOnly: this.options.readOnly,
      additionalDirectories: this.additionalDirectories,
      mcpServers: this.mcpServers.map((server) => ({
        name: server.name,
        type: mcpServerType(server),
      })),
    });
    this.send({ type: "bridge/phase", phase: "starting" });

    const stream = this.createAgentStream();

    const client = acp
      .client({ name: "attyd" })
      .onNotification(acp.methods.client.session.update, ({ params }) => {
        try {
          if (this.observeSessionUpdate(params)) {
            this.send({ type: "acp/session_update", notification: params });
          }
        } catch (error) {
          this.send({
            type: "bridge/error",
            message: `Invalid ACP session/update: ${errorMessage(error)}`,
          });
        }
      })
      .onRequest(acp.methods.client.session.requestPermission, ({ params, signal }) =>
        this.requestPermission(params, signal),
      )
      .onRequest(acp.methods.client.fs.readTextFile, ({ params, signal }) => {
        this.requireLocalWorkspaceCapability("fs/read_text_file");
        this.requireKnownClientSession(params.sessionId);
        return this.fileSystem.read(params, signal);
      })
      .onRequest(acp.methods.client.fs.writeTextFile, ({ params, signal }) => {
        this.requireLocalWorkspaceCapability("fs/write_text_file");
        this.requireKnownClientSession(params.sessionId);
        return this.fileSystem.write(params, signal);
      })
      .onRequest(acp.methods.client.terminal.create, ({ params }) => {
        this.requireLocalWorkspaceCapability("terminal/create");
        this.requireKnownClientSession(params.sessionId);
        return this.terminals.create(params);
      })
      .onRequest(acp.methods.client.terminal.output, ({ params }) => {
        this.requireLocalWorkspaceCapability("terminal/output");
        this.requireKnownClientSession(params.sessionId);
        return this.terminals.output(params);
      })
      .onRequest(acp.methods.client.terminal.waitForExit, ({ params, signal }) => {
        this.requireLocalWorkspaceCapability("terminal/wait_for_exit");
        this.requireKnownClientSession(params.sessionId);
        return this.terminals.waitForExit(params, signal);
      })
      .onRequest(acp.methods.client.terminal.kill, ({ params }) => {
        this.requireLocalWorkspaceCapability("terminal/kill");
        this.requireKnownClientSession(params.sessionId);
        return this.terminals.kill(params);
      })
      .onRequest(acp.methods.client.terminal.release, ({ params }) => {
        this.requireLocalWorkspaceCapability("terminal/release");
        this.requireKnownClientSession(params.sessionId);
        return this.terminals.release(params);
      })
      .onRequest(acp.methods.client.elicitation.create, ({ params, signal }) =>
        this.requestElicitation(params, signal),
      )
      .onNotification(
        acp.methods.client.elicitation.complete,
        ({ params }) => {
          try {
            this.completeUrlElicitation(params);
          } catch (error) {
            this.send({
              type: "bridge/error",
              message: `Invalid ACP elicitation/complete: ${errorMessage(error)}`,
            });
          }
        },
      )
      .onRequest<acp.ConnectMcpRequest, acp.ConnectMcpResponse>(
        acp.CLIENT_METHODS.mcp_connect,
        parseConnectMcpRequest,
        ({ params, signal }) => this.mcp.connect(params, signal),
      )
      .onRequest<acp.MessageMcpRequest, acp.MessageMcpResponse>(
        acp.CLIENT_METHODS.mcp_message,
        parseMessageMcp,
        ({ params, signal }) => this.mcp.message(params, signal),
      )
      .onNotification<acp.MessageMcpNotification>(
        acp.CLIENT_METHODS.mcp_message,
        parseMessageMcp,
        ({ params }) => this.mcp.notify(params),
      )
      .onRequest<acp.DisconnectMcpRequest, acp.DisconnectMcpResponse>(
        acp.CLIENT_METHODS.mcp_disconnect,
        parseDisconnectMcpRequest,
        ({ params }) => this.mcp.disconnect(params),
      );

    this.connection = client.connect(stream);
    void this.connection.closed.then(
      () => {
        if (this.closed || this.transport === "stdio") return;
        this.send({ type: "bridge/error", message: "Remote Agent transport closed" });
        this.send({ type: "bridge/phase", phase: "stopped" });
        this.close();
      },
      (error: unknown) => {
        if (this.closed) return;
        this.fail(error);
        this.close();
      },
    );

    this.send({ type: "bridge/phase", phase: "initializing" });
    const response = await this.connection.agent.request(
      acp.methods.agent.initialize,
      {
        protocolVersion: acp.PROTOCOL_VERSION,
        clientCapabilities: {
          auth: this.transport === "stdio" ? { terminal: true } : {},
          ...(this.transport === "stdio" ? {
            fs: {
              readTextFile: true,
              writeTextFile: !this.options.readOnly,
            },
            terminal: true,
          } : {}),
          session: {
            configOptions: { boolean: {} },
            compaction: {},
          },
          plan: {},
          elicitation: { form: {}, url: {} },
        },
        clientInfo: {
          name: "attyd",
          title: "attyd web client",
          version: "0.1.0",
        },
      },
    );
    validateBrowserRelayValue(response, "Agent initialize response");
    if (response.protocolVersion !== acp.PROTOCOL_VERSION) {
      throw new Error(
        `Unsupported ACP protocol v${response.protocolVersion}; attyd supports v${acp.PROTOCOL_VERSION}`,
      );
    }
    validateAuthMethods(response, this.transport === "stdio");
    this.validateConfiguredCapabilities(response);
    this.initialized = response;
    this.send({ type: "acp/initialized", response });
    this.send({ type: "bridge/phase", phase: "ready" });
  }

  receive(raw: string): void {
    let command: ClientCommand;
    try {
      command = parseClientCommand(raw);
    } catch (error) {
      this.send({ type: "bridge/error", message: errorMessage(error) });
      return;
    }
    const run = async () => {
      try {
        await this.handle(command);
      } catch (error) {
        this.send({
          type: "bridge/error",
          message: errorMessage(error),
          requestId: "requestId" in command ? command.requestId : undefined,
          operation: command.type,
          ...requestErrorFields(error),
        });
      }
    };
    const orderedSession = nesOrderedSession(command);
    if (orderedSession) this.enqueueNes(orderedSession, run);
    else void run();
  }

  close(): void {
    if (this.closed) return;
    this.closed = true;
    for (const pending of this.permissions.values()) {
      pending.resolve({ outcome: { outcome: "cancelled" } });
    }
    this.permissions.clear();
    for (const pending of this.elicitations.values()) {
      pending.resolve({ action: "cancel" });
    }
    this.elicitations.clear();
    this.urlElicitations.clear();
    this.seenUrlElicitationIds.clear();
    this.pendingSessionUpdates.clear();
    this.clearPendingCreationReplays();
    this.authTerminal.close();
    this.terminals.close();
    this.clearTerminalSnapshots();
    this.mcp.close();
    this.connection?.close();
    if (this.child && this.child.exitCode == null) this.child.kill();
  }

  private async handle(command: ClientCommand): Promise<void> {
    if (command.type === "bridge/ping") {
      this.send({ type: "bridge/pong", nonce: command.nonce });
      return;
    }
    const agent = this.requireAgent();

    switch (command.type) {
      case "auth/authenticate": {
        if (this.authOperation) {
          throw new Error(`An Agent ${this.authOperation} request is already running`);
        }
        const method = this.requireOfferedAuthMethod(command.methodId);
        if ("type" in method && method.type === "terminal") {
          throw new Error("Terminal authentication methods must run through auth/terminal_start");
        }
        this.authOperation = "authenticate";
        try {
          const response = await agent.request(acp.methods.agent.authenticate, {
            methodId: command.methodId,
          });
          validateBrowserRelayValue(response, "Agent authenticate response");
          this.send({
            type: "acp/authenticated",
            requestId: command.requestId,
            methodId: command.methodId,
            response,
          });
        } finally {
          this.authOperation = undefined;
        }
        return;
      }
      case "auth/terminal_start": {
        if (this.transport !== "stdio") {
          throw new Error("Terminal authentication is unavailable for remote Agent transports");
        }
        if (this.authOperation) {
          throw new Error(`An Agent ${this.authOperation} request is already running`);
        }
        const method = this.requireOfferedAuthMethod(command.methodId);
        if (!("type" in method) || method.type !== "terminal") {
          throw new Error("Authentication method is handled by the Agent, not a terminal");
        }
        this.authOperation = "terminal authentication";
        try {
          this.authTerminal.start({
            requestId: command.requestId,
            method,
            command: this.options.command,
            cwd: this.options.cwd,
            env: this.options.env,
            cols: command.cols,
            rows: command.rows,
          });
          this.send({
            type: "bridge/auth_terminal_started",
            requestId: command.requestId,
            methodId: command.methodId,
          });
        } catch (error) {
          this.authOperation = undefined;
          throw error;
        }
        return;
      }
      case "auth/terminal_input":
        this.authTerminal.write(command.requestId, command.data);
        return;
      case "auth/terminal_resize":
        this.authTerminal.resize(command.requestId, command.cols, command.rows);
        return;
      case "auth/terminal_cancel":
        this.authTerminal.cancel(command.requestId);
        return;
      case "auth/logout": {
        this.requireCapability(
          (this.initialized?.authMethods?.length ?? 0) > 0 &&
            this.initialized?.agentCapabilities?.auth?.logout != null,
          "logout",
        );
        if (this.authOperation) {
          throw new Error(`An Agent ${this.authOperation} request is already running`);
        }
        this.authOperation = "logout";
        try {
          const response = await agent.request(acp.methods.agent.logout, {});
          validateBrowserRelayValue(response, "Agent logout response");
          this.send({ type: "acp/logged_out", requestId: command.requestId, response });
        } finally {
          this.authOperation = undefined;
        }
        return;
      }
      case "context/search": {
        this.requireLocalWorkspaceCapability("context/search");
        this.requireCapability(
          this.initialized?.agentCapabilities?.promptCapabilities?.embeddedContext === true,
          "context/search",
        );
        const matches = await this.fileSystem.searchContext(command.query);
        this.send({
          type: "bridge/context_search_result",
          requestId: command.requestId,
          query: command.query,
          matches,
        });
        return;
      }
      case "context/read": {
        this.requireLocalWorkspaceCapability("context/read");
        this.requireKnownSession(command.sessionId);
        this.requireCapability(
          this.initialized?.agentCapabilities?.promptCapabilities?.embeddedContext === true,
          "context/read",
        );
        const attachment = await this.fileSystem.readContext(command.path);
        this.assertPromptCapabilities([attachment.block]);
        validateBrowserRelayValue(attachment, "Workspace prompt context");
        this.send({
          type: "bridge/context_attached",
          requestId: command.requestId,
          sessionId: command.sessionId,
          attachment,
        });
        return;
      }
      case "session/new": {
        const cwd = this.resolveNewSessionCwd(command.cwd);
        const release = this.reserveSessionCreation();
        try {
          const response = await agent.request(acp.methods.agent.session.new, {
            cwd,
            additionalDirectories: this.configuredAdditionalDirectories(),
            mcpServers: this.mcpServers,
          });
          let earlyUpdates: acp.SessionNotification[];
          try {
            validateBrowserRelayValue(response, "Agent session/new response");
            earlyUpdates = this.trackSession(response.sessionId, response, cwd);
          } catch (error) {
            this.closeRejectedChatSession(agent, response.sessionId);
            throw error;
          }
          this.send({
            type: "acp/session_created",
            requestId: command.requestId,
            cwd,
            response,
            ...(earlyUpdates.length > 0 ? { earlyUpdates } : {}),
          });
        } finally {
          release();
        }
        return;
      }
      case "session/list": {
        this.requireCapability(
          this.initialized?.agentCapabilities?.sessionCapabilities?.list != null,
          "session/list",
        );
        if (this.sessionListInFlight) throw new Error("A session/list request is already running");
        if (command.cursor !== undefined && command.cursor !== this.listedNextCursor) {
          throw new Error("session/list cursor was not offered by the Agent");
        }
        this.sessionListInFlight = true;
        try {
          const requestedCwd = this.transport === "stdio" ? this.options.cwd : undefined;
          const response = await agent.request(acp.methods.agent.session.list, {
            ...(requestedCwd == null ? {} : { cwd: requestedCwd }),
            cursor: command.cursor,
          });
          const pageBytes = validateSessionListPage(
            response,
            command.cursor,
            requestedCwd,
            {
              listedSessionIds: this.listedSessions,
              listedCursors: this.listedCursors,
              accumulatedBytes: this.listedSessionBytes,
            },
          );
          if (command.cursor === undefined) {
            this.listedSessions.clear();
            this.listedSessionInfo.clear();
            this.listedCursors.clear();
            this.listedSessionBytes = 0;
          }
          for (const session of response.sessions) {
            this.listedSessions.add(session.sessionId);
            this.listedSessionInfo.set(session.sessionId, session);
          }
          this.listedNextCursor = response.nextCursor ?? undefined;
          if (response.nextCursor != null) this.listedCursors.add(response.nextCursor);
          this.listedSessionBytes += pageBytes;
          this.send({
            type: "acp/sessions_listed",
            requestId: command.requestId,
            cursor: command.cursor,
            response,
          });
        } finally {
          this.sessionListInFlight = false;
        }
        return;
      }
      case "session/load": {
        this.requireCapability(
          this.initialized?.agentCapabilities?.loadSession === true,
          "session/load",
        );
        const listedSession = this.requireListedSession(command.sessionId);
        const cwd = listedSession.cwd;
        const release = this.reserveSessionAttachment(command.sessionId);
        try {
          const response = await agent.request(acp.methods.agent.session.load, {
            cwd,
            additionalDirectories: this.configuredAdditionalDirectories(),
            mcpServers: this.mcpServers,
            sessionId: command.sessionId,
          });
          try {
            validateBrowserRelayValue(response, "Agent session/load response");
            this.trackSession(command.sessionId, response, cwd, command.sessionId);
          } catch (error) {
            this.closeRejectedAttachedSession(agent, command.sessionId);
            throw error;
          }
          this.send({
            type: "acp/session_attached",
            requestId: command.requestId,
            method: "load",
            sessionId: command.sessionId,
            cwd,
            response,
          });
        } finally {
          release();
        }
        return;
      }
      case "session/resume": {
        this.requireCapability(
          this.initialized?.agentCapabilities?.sessionCapabilities?.resume != null,
          "session/resume",
        );
        const listedSession = this.requireListedSession(command.sessionId);
        const cwd = listedSession.cwd;
        const release = this.reserveSessionAttachment(command.sessionId);
        try {
          const response = await agent.request(acp.methods.agent.session.resume, {
            cwd,
            additionalDirectories: this.configuredAdditionalDirectories(),
            mcpServers: this.mcpServers,
            sessionId: command.sessionId,
          });
          try {
            validateBrowserRelayValue(response, "Agent session/resume response");
            this.trackSession(command.sessionId, response, cwd, command.sessionId);
          } catch (error) {
            this.closeRejectedAttachedSession(agent, command.sessionId);
            throw error;
          }
          this.send({
            type: "acp/session_attached",
            requestId: command.requestId,
            method: "resume",
            sessionId: command.sessionId,
            cwd,
            response,
          });
        } finally {
          release();
        }
        return;
      }
      case "session/fork": {
        this.requireCapability(
          this.initialized?.agentCapabilities?.sessionCapabilities?.fork != null,
          "session/fork",
        );
        const sourceSession = this.requireKnownSession(command.sessionId);
        this.requireSessionNotClosing(command.sessionId, "fork the session");
        this.requireSessionNotChangingControl(command.sessionId, "fork the session");
        if (this.prompts.has(command.sessionId)) {
          throw new Error("Cancel the running prompt before forking its session");
        }
        if (this.pendingSessionForks.has(command.sessionId)) {
          throw new Error("A session/fork request is already running for this session");
        }
        const release = this.reserveSessionCreation();
        this.pendingSessionForks.add(command.sessionId);
        try {
          const response = await agent.request(acp.methods.agent.session.fork, {
            sessionId: command.sessionId,
            cwd: sourceSession.cwd,
            additionalDirectories: this.configuredAdditionalDirectories(),
            mcpServers: this.mcpServers,
          });
          let earlyUpdates: acp.SessionNotification[];
          try {
            if (response.sessionId === command.sessionId) {
              throw new Error("Agent returned the source session ID for session/fork");
            }
            validateBrowserRelayValue(response, "Agent session/fork response");
            earlyUpdates = this.trackSession(response.sessionId, response, sourceSession.cwd);
          } catch (error) {
            this.closeRejectedChatSession(agent, response.sessionId);
            throw error;
          }
          this.send({
            type: "acp/session_forked",
            requestId: command.requestId,
            sourceSessionId: command.sessionId,
            cwd: sourceSession.cwd,
            response,
            ...(earlyUpdates.length > 0 ? { earlyUpdates } : {}),
          });
        } finally {
          this.pendingSessionForks.delete(command.sessionId);
          release();
        }
        return;
      }
      case "session/close": {
        this.requireCapability(
          this.initialized?.agentCapabilities?.sessionCapabilities?.close != null,
          "session/close",
        );
        this.requireKnownSession(command.sessionId);
        if (this.pendingSessionClosures.has(command.sessionId)) {
          throw new Error("A session/close request is already running for this session");
        }
        this.requireSessionNotForking(command.sessionId, "close the session");
        this.requireSessionNotChangingControl(command.sessionId, "close the session");
        if (this.prompts.has(command.sessionId)) {
          throw new Error("Cancel the running prompt before closing its session");
        }
        this.pendingSessionClosures.add(command.sessionId);
        try {
          await agent.request(acp.methods.agent.session.close, {
            sessionId: command.sessionId,
          });
          this.cancelSessionInteractions(command.sessionId, "session_closed");
          this.terminals.releaseSession(command.sessionId);
          this.sessions.delete(command.sessionId);
          this.send({
            type: "acp/session_closed",
            requestId: command.requestId,
            sessionId: command.sessionId,
          });
        } finally {
          this.pendingSessionClosures.delete(command.sessionId);
        }
        return;
      }
      case "session/delete": {
        this.requireCapability(
          this.initialized?.agentCapabilities?.sessionCapabilities?.delete != null,
          "session/delete",
        );
        this.requireListedSession(command.sessionId);
        if (this.pendingSessionDeletions.has(command.sessionId)) {
          throw new Error("A session/delete request is already running for this session");
        }
        if (this.sessions.has(command.sessionId)) {
          throw new Error("Close the active session before deleting it");
        }
        if (this.pendingSessionAttachments.has(command.sessionId)) {
          throw new Error("Wait for the session attachment before deleting it");
        }
        if (this.pendingSessionDeletions.size >= MAX_TRACKED_SESSIONS) {
          throw new Error(`Pending session deletion limit reached (${MAX_TRACKED_SESSIONS})`);
        }
        this.pendingSessionDeletions.add(command.sessionId);
        try {
          await agent.request(acp.methods.agent.session.delete, {
            sessionId: command.sessionId,
          });
          this.listedSessions.delete(command.sessionId);
          this.listedSessionInfo.delete(command.sessionId);
          this.send({
            type: "acp/session_deleted",
            requestId: command.requestId,
            sessionId: command.sessionId,
          });
        } finally {
          this.pendingSessionDeletions.delete(command.sessionId);
        }
        return;
      }
      case "session/prompt": {
        this.requireKnownSession(command.sessionId);
        this.requireSessionNotForking(command.sessionId, "start a prompt");
        this.requireSessionNotClosing(command.sessionId, "start a prompt");
        this.requireSessionNotChangingControl(command.sessionId, "start a prompt");
        this.assertPromptCapabilities(command.prompt);
        if (this.prompts.has(command.sessionId)) {
          throw new Error("A prompt is already running for this session");
        }
        this.prompts.add(command.sessionId);
        try {
          const response = await agent.request(acp.methods.agent.session.prompt, {
            sessionId: command.sessionId,
            prompt: command.prompt,
          });
          validatePromptResponse(response);
          this.send({
            type: "acp/prompt_complete",
            requestId: command.requestId,
            sessionId: command.sessionId,
            response,
          });
        } finally {
          this.prompts.delete(command.sessionId);
        }
        return;
      }
      case "session/cancel":
        this.requireKnownSession(command.sessionId);
        await agent.notify(acp.methods.agent.session.cancel, {
          sessionId: command.sessionId,
        });
        this.cancelSessionInteractions(command.sessionId, "session_cancelled");
        return;
      case "session/set_mode": {
        this.requireOfferedMode(command.sessionId, command.modeId);
        const release = this.reserveSessionControl(command.sessionId, "change the mode");
        try {
          await agent.request(acp.methods.agent.session.setMode, {
            sessionId: command.sessionId,
            modeId: command.modeId,
          });
          this.updateTrackedMode(command.sessionId, command.modeId);
          this.send({
            type: "acp/mode_changed",
            requestId: command.requestId,
            sessionId: command.sessionId,
            modeId: command.modeId,
          });
        } finally {
          release();
        }
        return;
      }
      case "session/set_config_option": {
        this.requireOfferedConfig(
          command.sessionId,
          command.configId,
          command.value,
        );
        const release = this.reserveSessionControl(command.sessionId, "change configuration");
        try {
          const response = await agent.request(
            acp.methods.agent.session.setConfigOption,
            typeof command.value === "boolean"
              ? {
                  sessionId: command.sessionId,
                  configId: command.configId,
                  type: "boolean",
                  value: command.value,
                }
              : {
                  sessionId: command.sessionId,
                  configId: command.configId,
                  value: command.value,
                },
          );
          validateBrowserRelayValue(
            response,
            "Agent session/set_config_option response",
          );
          validateSessionConfigOptions(response.configOptions);
          this.requireKnownSession(command.sessionId).configOptions = response.configOptions;
          this.send({
            type: "acp/config_changed",
            requestId: command.requestId,
            sessionId: command.sessionId,
            configId: command.configId,
            value: command.value,
            response,
          });
        } finally {
          release();
        }
        return;
      }
      case "nes/start": {
        this.requireLocalWorkspaceCapability("nes/start");
        this.requireCapability(
          this.initialized?.agentCapabilities?.nes != null,
          "nes/start",
        );
        if (this.nesStarting) throw new Error("An NES session is already starting");
        if (this.nesSessions.size > 0) throw new Error("An NES session is already active");
        if (this.nesSessions.size >= MAX_NES_SESSIONS) {
          throw new Error(`NES session limit reached (${MAX_NES_SESSIONS})`);
        }
        const workspacePaths = [this.options.cwd, ...this.additionalDirectories];
        this.nesStarting = true;
        let response: acp.StartNesResponse;
        try {
          response = await agent.request<acp.StartNesResponse, acp.StartNesRequest>(
            acp.methods.agent.nes.start,
            {
              workspaceUri: pathToFileURL(this.options.cwd).href,
              workspaceFolders: workspacePaths.map((path) => ({
                uri: pathToFileURL(path).href,
                name: basename(path),
              })),
            },
          );
        } finally {
          this.nesStarting = false;
        }
        const rejectedSessionId = isValidAgentIdentifier(response.sessionId)
          ? response.sessionId
          : undefined;
        try {
          validateBrowserRelayValue(response, "Agent nes/start response");
          validateAgentSessionId(response.sessionId);
          if (this.sessions.has(response.sessionId) || this.nesSessions.has(response.sessionId)) {
            throw new Error(`Agent returned a duplicate NES session ID: ${response.sessionId}`);
          }
        } catch (error) {
          if (rejectedSessionId != null) {
            void agent.request(acp.methods.agent.nes.close, {
              sessionId: rejectedSessionId,
            }).catch(() => {});
          }
          throw error;
        }
        this.nesSessions.set(response.sessionId, {
          documents: new Map(),
          editHistory: [],
          suggestions: new Map(),
          userActions: [],
        });
        this.send({ type: "acp/nes_started", requestId: command.requestId, response });
        return;
      }
      case "document/open": {
        const session = this.requireNesSession(command.sessionId);
        if (session.documents.size >= MAX_NES_DOCUMENTS) {
          throw new Error(`NES document limit reached (${MAX_NES_DOCUMENTS})`);
        }
        const { content } = await this.fileSystem.read({
          sessionId: command.sessionId,
          path: command.path,
        });
        this.assertDocumentSize(content);
        const uri = pathToFileURL(command.path).href;
        if (session.documents.has(uri)) throw new Error(`Document is already open: ${uri}`);
        const document: TrackedNesDocument = {
          sessionId: command.sessionId,
          path: command.path,
          uri,
          languageId: command.languageId,
          version: 1,
          text: content,
          lastAccessedMs: Date.now(),
        };
        const capabilities = this.documentCapabilities();
        const notification: acp.DidOpenDocumentNotification | undefined =
          capabilities?.didOpen != null
            ? {
                sessionId: command.sessionId,
                uri,
                languageId: command.languageId,
                version: document.version,
                text: document.text,
              }
            : undefined;
        if (notification) {
          await agent.notify(acp.methods.agent.document.didOpen, notification);
        }
        session.documents.set(uri, document);
        this.send({
          type: "acp/document_opened",
          requestId: command.requestId,
          document,
          notification,
        });
        return;
      }
      case "document/change": {
        const session = this.requireNesSession(command.sessionId);
        const document = this.requireNesDocument(command.sessionId, command.uri);
        this.assertDocumentSize(command.text);
        await this.rejectNesSuggestions(
          command.sessionId,
          session,
          ({ suggestion }) => suggestion.uri === command.uri,
          "replaced",
          command.requestId,
        );
        const previousText = document.text;
        const syncKind = this.documentCapabilities()?.didChange?.syncKind;
        const notification: acp.DidChangeDocumentNotification | undefined = syncKind
          ? {
              sessionId: command.sessionId,
              uri: command.uri,
              version: document.version + 1,
              contentChanges: [
                fullOrIncrementalChange(document.text, command.text, syncKind),
              ],
            }
          : undefined;
        if (notification) {
          await agent.notify(acp.methods.agent.document.didChange, notification);
        }
        document.version += 1;
        document.text = command.text;
        document.lastAccessedMs = Date.now();
        this.recordNesEdit(session, document, previousText, command.text);
        this.send({
          type: "acp/document_changed",
          requestId: command.requestId,
          document,
          notification,
        });
        return;
      }
      case "document/save": {
        const document = this.requireNesDocument(command.sessionId, command.uri);
        await this.fileSystem.write({
          sessionId: command.sessionId,
          path: document.path,
          content: document.text,
        });
        const notification: acp.DidSaveDocumentNotification | undefined =
          this.documentCapabilities()?.didSave != null
            ? { sessionId: command.sessionId, uri: command.uri }
            : undefined;
        if (notification) {
          await agent.notify(acp.methods.agent.document.didSave, notification);
        }
        document.lastAccessedMs = Date.now();
        this.send({
          type: "acp/document_saved",
          requestId: command.requestId,
          sessionId: command.sessionId,
          uri: command.uri,
          notification,
        });
        return;
      }
      case "document/focus": {
        const session = this.requireNesSession(command.sessionId);
        const document = this.requireNesDocument(command.sessionId, command.uri);
        offsetAt(document.text, command.position);
        assertRange(document.text, command.visibleRange);
        const focusedAt = Date.now();
        const notification: acp.DidFocusDocumentNotification | undefined =
          this.documentCapabilities()?.didFocus != null
            ? {
                sessionId: command.sessionId,
                uri: command.uri,
                version: document.version,
                position: command.position,
                visibleRange: command.visibleRange,
              }
            : undefined;
        if (notification) {
          await agent.notify(acp.methods.agent.document.didFocus, notification);
        }
        document.visibleRange = command.visibleRange;
        document.lastFocusedMs = focusedAt;
        document.lastAccessedMs = focusedAt;
        this.recordNesUserAction(session, {
          action: "cursorMovement",
          uri: command.uri,
          position: command.position,
          timestampMs: focusedAt,
        });
        if (notification) this.send({ type: "acp/document_focused", notification });
        return;
      }
      case "document/close": {
        const session = this.requireNesSession(command.sessionId);
        this.requireNesDocument(command.sessionId, command.uri);
        await this.rejectNesSuggestions(
          command.sessionId,
          session,
          ({ suggestion }) => suggestion.uri === command.uri,
          "cancelled",
          command.requestId,
        );
        const notification: acp.DidCloseDocumentNotification | undefined =
          this.documentCapabilities()?.didClose != null
            ? { sessionId: command.sessionId, uri: command.uri }
            : undefined;
        if (notification) {
          await agent.notify(acp.methods.agent.document.didClose, notification);
        }
        session.documents.delete(command.uri);
        for (const [id, pending] of session.suggestions) {
          if (pending.suggestion.uri === command.uri) session.suggestions.delete(id);
        }
        this.send({
          type: "acp/document_closed",
          requestId: command.requestId,
          sessionId: command.sessionId,
          uri: command.uri,
          notification,
        });
        return;
      }
      case "nes/suggest": {
        const session = this.requireNesSession(command.sessionId);
        const document = this.requireNesDocument(command.sessionId, command.uri);
        offsetAt(document.text, command.position);
        if (command.selection) assertRange(document.text, command.selection);
        for (const [id] of session.suggestions) {
          await agent.notify(acp.methods.agent.nes.reject, {
            sessionId: command.sessionId,
            id,
            reason: "replaced",
          });
        }
        session.suggestions.clear();
        const response = await agent.request<acp.SuggestNesResponse, acp.SuggestNesRequest>(
          acp.methods.agent.nes.suggest,
          {
          sessionId: command.sessionId,
          uri: command.uri,
          version: document.version,
          position: command.position,
          selection: command.selection,
          triggerKind: command.triggerKind,
          context: this.buildNesContext(session),
          },
        );
        if (response.suggestions.length > MAX_NES_SUGGESTIONS) {
          throw new Error(`Agent returned more than ${MAX_NES_SUGGESTIONS} NES suggestions`);
        }
        if (Buffer.byteLength(JSON.stringify(response), "utf8") > MAX_NES_RESPONSE_BYTES) {
          throw new Error(`Agent NES response exceeds the ${MAX_NES_RESPONSE_BYTES} byte limit`);
        }
        const validated = new Map<string, {
          suggestion: acp.NesSuggestion;
          expectedText?: string;
        }>();
        for (const suggestion of response.suggestions) {
          if (validated.has(suggestion.id)) {
            throw new Error(`Agent returned duplicate NES suggestion ID: ${suggestion.id}`);
          }
          const expectedText = this.validateNesSuggestion(session, suggestion);
          validated.set(suggestion.id, { suggestion, expectedText });
        }
        session.suggestions = validated;
        this.send({
          type: "acp/nes_suggestions",
          requestId: command.requestId,
          sessionId: command.sessionId,
          uri: command.uri,
          response,
        });
        return;
      }
      case "nes/accept": {
        const session = this.requireNesSession(command.sessionId);
        const pending = session.suggestions.get(command.suggestionId);
        if (!pending) throw new Error("NES suggestion is no longer pending");
        let acceptedEdit: {
          document: TrackedNesDocument;
          previousText: string;
          nextText: string;
          notification?: acp.DidChangeDocumentNotification;
        } | undefined;
        if (pending.expectedText !== undefined) {
          const document = session.documents.get(pending.suggestion.uri);
          if (!document) throw new Error("NES suggestion document is no longer open");
          if (command.text !== pending.expectedText) {
            throw new Error("Accepted NES text does not match the offered edit");
          }
          await this.rejectNesSuggestions(
            command.sessionId,
            session,
            ({ suggestion }, id) =>
              id !== command.suggestionId && suggestion.uri === pending.suggestion.uri,
            "replaced",
            command.requestId,
          );
          const syncKind = this.documentCapabilities()?.didChange?.syncKind;
          const notification: acp.DidChangeDocumentNotification | undefined = syncKind
            ? {
                sessionId: command.sessionId,
                uri: document.uri,
                version: document.version + 1,
                contentChanges: [
                  fullOrIncrementalChange(document.text, command.text, syncKind),
                ],
              }
            : undefined;
          acceptedEdit = {
            document,
            previousText: document.text,
            nextText: command.text,
            notification,
          };
        }
        await agent.notify(acp.methods.agent.nes.accept, {
          sessionId: command.sessionId,
          id: command.suggestionId,
        });
        if (acceptedEdit) {
          const { document, previousText, nextText, notification } = acceptedEdit;
          if (notification) {
            await agent.notify(acp.methods.agent.document.didChange, notification);
          }
          document.version += 1;
          document.text = nextText;
          document.lastAccessedMs = Date.now();
          this.recordNesEdit(session, document, previousText, nextText);
          this.send({
            type: "acp/document_changed",
            requestId: command.requestId,
            document,
            notification,
          });
        }
        session.suggestions.delete(command.suggestionId);
        this.send({
          type: "acp/nes_suggestion_resolved",
          requestId: command.requestId,
          sessionId: command.sessionId,
          suggestionId: command.suggestionId,
          outcome: "accepted",
        });
        return;
      }
      case "nes/reject": {
        const session = this.requireNesSession(command.sessionId);
        if (!session.suggestions.has(command.suggestionId)) {
          throw new Error("NES suggestion is no longer pending");
        }
        await agent.notify(acp.methods.agent.nes.reject, {
          sessionId: command.sessionId,
          id: command.suggestionId,
          reason: command.reason,
        });
        session.suggestions.delete(command.suggestionId);
        this.send({
          type: "acp/nes_suggestion_resolved",
          requestId: command.requestId,
          sessionId: command.sessionId,
          suggestionId: command.suggestionId,
          outcome: "rejected",
          reason: command.reason,
        });
        return;
      }
      case "nes/close": {
        this.requireNesSession(command.sessionId);
        await agent.request(acp.methods.agent.nes.close, {
          sessionId: command.sessionId,
        });
        this.cancelSessionInteractions(command.sessionId, "nes_closed");
        this.terminals.releaseSession(command.sessionId);
        this.nesSessions.delete(command.sessionId);
        this.send({
          type: "acp/nes_closed",
          requestId: command.requestId,
          sessionId: command.sessionId,
        });
        return;
      }
      case "permission/respond": {
        const pending = this.permissions.get(command.permissionId);
        if (!pending) throw new Error("Permission request is no longer pending");
        const selectedOptionId = command.outcome.outcome === "selected"
          ? command.outcome.optionId
          : undefined;
        if (
          selectedOptionId != null &&
          !pending.request.options.some(
            ({ optionId }) => optionId === selectedOptionId,
          )
        ) {
          throw new Error("Permission option was not offered by the agent");
        }
        this.settlePermission(
          command.permissionId,
          { outcome: command.outcome },
          true,
          command.requestId,
        );
        return;
      }
      case "elicitation/respond": {
        const pending = this.elicitations.get(command.elicitationId);
        if (!pending) throw new Error("Elicitation request is no longer pending");
        validateElicitationResponse(pending.request, command.response);
        this.recordUrlElicitationResponse(
          pending.request,
          command.elicitationId,
          command.response,
        );
        this.settleElicitation(
          command.elicitationId,
          command.response,
          true,
          command.requestId,
        );
      }
    }
  }

  private createAgentStream(): acp.Stream {
    if (this.transport === "http") {
      return createHttpStream(this.options.command[0]);
    }
    if (this.transport === "ws") {
      return createWebSocketStream(this.options.command[0], {
        WebSocket: NodeWebSocket,
      });
    }

    const [command, ...args] = this.options.command;
    const child = spawn(command, args, {
      cwd: this.options.cwd,
      env: this.options.env ?? process.env,
      stdio: ["pipe", "pipe", "pipe"],
    });
    this.child = child;

    child.stderr.setEncoding("utf8");
    child.stderr.on("data", (chunk: string) => {
      this.send({ type: "bridge/stderr", chunk });
    });
    child.once("error", (error) => {
      if (this.closed) return;
      this.fail(error);
      this.close();
    });
    child.once("exit", (code, signal) => {
      if (!this.closed) {
        this.send({
          type: "bridge/error",
          message: `Agent exited (${code ?? signal ?? "unknown"})`,
        });
        this.send({ type: "bridge/phase", phase: "stopped" });
        this.close();
      }
    });

    const output = Writable.toWeb(child.stdin);
    const input = limitNdjsonLineBytes(
      Readable.toWeb(child.stdout) as ReadableStream<Uint8Array>,
      MAX_AGENT_NDJSON_LINE_BYTES,
    );
    return acp.ndJsonStream(output, input);
  }

  private requestPermission(
    request: acp.RequestPermissionRequest,
    signal: AbortSignal,
  ): Promise<acp.RequestPermissionResponse> {
    const session = this.requireKnownSession(request.sessionId);
    validatePermissionRequest(request, session.updateValidation, {
      assertTerminalReference: (terminalId) => {
        this.terminals.assertReference(terminalId, request.sessionId);
      },
    });
    if (this.permissions.size >= MAX_PENDING_INTERACTIONS) {
      throw new Error("Too many pending permission requests");
    }
    if ([...this.permissions.values()].some((pending) =>
      pending.sessionId === request.sessionId &&
      pending.request.toolCall.toolCallId === request.toolCall.toolCallId
    )) {
      throw new Error(
        `A permission request is already pending for tool call: ${request.toolCall.toolCallId}`,
      );
    }
    const permissionId = randomUUID();
    return new Promise((resolve) => {
      const abort = () => {
        this.settlePermission(
          permissionId,
          { outcome: { outcome: "cancelled" } },
          true,
        );
      };
      this.permissions.set(permissionId, {
        sessionId: request.sessionId,
        request,
        resolve: (response) => {
          signal.removeEventListener("abort", abort);
          resolve(response);
        },
      });
      signal.addEventListener("abort", abort, { once: true });
      if (signal.aborted) {
        this.settlePermission(
          permissionId,
          { outcome: { outcome: "cancelled" } },
          false,
        );
        return;
      }
      this.send({ type: "acp/permission_request", permissionId, request });
    });
  }

  private settlePermission(
    permissionId: string,
    response: acp.RequestPermissionResponse,
    notifyUi = true,
    requestId?: string,
  ): boolean {
    const pending = this.permissions.get(permissionId);
    if (!pending) return false;
    this.permissions.delete(permissionId);
    pending.resolve(response);
    if (notifyUi) {
      this.send({ type: "acp/permission_resolved", permissionId, requestId });
    }
    return true;
  }

  private cancelSessionInteractions(
    sessionId: string,
    reason: ElicitationAbortReason,
  ): void {
    for (const [id, pending] of this.permissions) {
      if (pending.sessionId !== sessionId) continue;
      this.settlePermission(id, { outcome: { outcome: "cancelled" } });
    }
    for (const [id, pending] of this.elicitations) {
      if (!("sessionId" in pending.request) || pending.request.sessionId !== sessionId) continue;
      this.removePendingUrlElicitation(pending.request, id);
      this.settleElicitation(id, { action: "cancel" });
    }
    for (const [elicitationId, tracked] of this.urlElicitations) {
      if (tracked.sessionId !== sessionId) continue;
      this.urlElicitations.delete(elicitationId);
      if (tracked.state === "accepted") {
        this.send({
          type: "acp/elicitation_aborted",
          elicitationId,
          sessionId,
          reason,
        });
      }
    }
  }

  private requestElicitation(
    request: acp.CreateElicitationRequest,
    signal: AbortSignal,
  ): Promise<acp.CreateElicitationResponse> {
    validateElicitationRequest(request);
    if (
      "sessionId" in request &&
      typeof request.sessionId === "string"
    ) {
      this.requireKnownClientSession(request.sessionId);
    }
    if (this.elicitations.size >= MAX_PENDING_INTERACTIONS) {
      throw new Error("Too many pending elicitation requests");
    }
    const elicitationId = randomUUID();
    const urlElicitationId = getUrlElicitationId(request);
    if (urlElicitationId != null) {
      if (urlElicitationId.length === 0 || urlElicitationId.length > 1_024) {
        throw new Error("Agent returned an invalid URL elicitation ID");
      }
      if (this.seenUrlElicitationIds.has(urlElicitationId)) {
        throw new Error(`Agent reused a URL elicitation ID: ${urlElicitationId}`);
      }
      if (this.seenUrlElicitationIds.size >= MAX_URL_ELICITATION_IDS) {
        throw new Error(`Agent exceeded ${MAX_URL_ELICITATION_IDS} URL elicitation IDs`);
      }
      this.seenUrlElicitationIds.add(urlElicitationId);
      this.urlElicitations.set(urlElicitationId, {
        bridgeElicitationId: elicitationId,
        sessionId:
          "sessionId" in request && typeof request.sessionId === "string"
            ? request.sessionId
            : undefined,
        state: "pending",
      });
    }
    return new Promise((resolve) => {
      const abort = () => {
        this.removePendingUrlElicitation(request, elicitationId);
        this.settleElicitation(elicitationId, { action: "cancel" }, true);
      };
      this.elicitations.set(elicitationId, {
        request,
        resolve: (response) => {
          signal.removeEventListener("abort", abort);
          resolve(response);
        },
      });
      signal.addEventListener("abort", abort, { once: true });
      if (signal.aborted) {
        this.removePendingUrlElicitation(request, elicitationId);
        this.settleElicitation(elicitationId, { action: "cancel" }, false);
        return;
      }
      this.send({ type: "acp/elicitation_request", elicitationId, request });
    });
  }

  private settleElicitation(
    elicitationId: string,
    response: acp.CreateElicitationResponse,
    notifyUi = true,
    requestId?: string,
  ): boolean {
    const pending = this.elicitations.get(elicitationId);
    if (!pending) return false;
    this.elicitations.delete(elicitationId);
    pending.resolve(response);
    if (notifyUi) {
      this.send({
        type: "acp/elicitation_resolved",
        elicitationId,
        response,
        requestId,
      });
    }
    return true;
  }

  private recordUrlElicitationResponse(
    request: acp.CreateElicitationRequest,
    bridgeElicitationId: string,
    response: acp.CreateElicitationResponse,
  ): void {
    const urlElicitationId = getUrlElicitationId(request);
    if (urlElicitationId == null) return;
    const tracked = this.urlElicitations.get(urlElicitationId);
    if (!tracked || tracked.bridgeElicitationId !== bridgeElicitationId) {
      throw new Error(`URL elicitation is no longer tracked: ${urlElicitationId}`);
    }
    if (response.action === "accept") tracked.state = "accepted";
    else this.urlElicitations.delete(urlElicitationId);
  }

  private removePendingUrlElicitation(
    request: acp.CreateElicitationRequest,
    bridgeElicitationId: string,
  ): void {
    const urlElicitationId = getUrlElicitationId(request);
    if (urlElicitationId == null) return;
    const tracked = this.urlElicitations.get(urlElicitationId);
    if (tracked?.bridgeElicitationId === bridgeElicitationId) {
      this.urlElicitations.delete(urlElicitationId);
    }
  }

  private completeUrlElicitation(
    notification: acp.CompleteElicitationNotification,
  ): void {
    validateBrowserRelayValue(notification, "Agent elicitation/complete notification");
    const tracked = this.urlElicitations.get(notification.elicitationId);
    if (!tracked || tracked.state !== "accepted") {
      throw new Error(
        `URL elicitation was not accepted or is no longer active: ${notification.elicitationId}`,
      );
    }
    this.urlElicitations.delete(notification.elicitationId);
    this.send({ type: "acp/elicitation_complete", notification });
  }

  private requireAgent(): acp.ClientContext {
    if (!this.connection || !this.initialized) {
      throw new Error("ACP agent is not initialized yet");
    }
    return this.connection.agent;
  }

  private closeRejectedChatSession(
    agent: acp.ClientContext,
    sessionId: unknown,
  ): void {
    if (
      this.initialized?.agentCapabilities?.sessionCapabilities?.close == null ||
      !isValidAgentIdentifier(sessionId) ||
      this.pendingSessionCreations !== 1 ||
      this.sessions.has(sessionId) ||
      this.pendingSessionAttachments.has(sessionId) ||
      this.pendingSessionDeletions.has(sessionId)
    ) return;
    void agent.request(acp.methods.agent.session.close, { sessionId }).catch(() => {});
  }

  private closeRejectedAttachedSession(
    agent: acp.ClientContext,
    sessionId: string,
  ): void {
    if (
      this.initialized?.agentCapabilities?.sessionCapabilities?.close == null ||
      !isValidAgentIdentifier(sessionId) ||
      !this.pendingSessionAttachments.has(sessionId) ||
      this.sessions.has(sessionId) ||
      this.pendingSessionDeletions.has(sessionId)
    ) return;
    void agent.request(acp.methods.agent.session.close, { sessionId }).catch(() => {});
  }

  private enqueueNes(sessionId: string, operation: () => Promise<void>): void {
    const previous = this.nesQueues.get(sessionId) ?? Promise.resolve();
    const current = previous.then(operation, operation);
    this.nesQueues.set(sessionId, current);
    void current.finally(() => {
      if (this.nesQueues.get(sessionId) === current) this.nesQueues.delete(sessionId);
    });
  }

  private requireCapability(supported: boolean, method: string): void {
    if (!supported) {
      throw new Error(`Agent did not advertise ${method}`);
    }
  }

  private requireKnownSession(sessionId: string): TrackedSession {
    const session = this.sessions.get(sessionId);
    if (!session) throw new Error(`Unknown or inactive session: ${sessionId}`);
    return session;
  }

  private requireKnownClientSession(sessionId: string): void {
    if (!this.sessions.has(sessionId) && !this.nesSessions.has(sessionId)) {
      throw new Error(`Unknown or inactive session: ${sessionId}`);
    }
  }

  private requireSessionNotForking(sessionId: string, operation: string): void {
    if (this.pendingSessionForks.has(sessionId)) {
      throw new Error(`Cannot ${operation} while session/fork is running`);
    }
  }

  private requireSessionNotClosing(sessionId: string, operation: string): void {
    if (this.pendingSessionClosures.has(sessionId)) {
      throw new Error(`Cannot ${operation} while session/close is running`);
    }
  }

  private requireSessionNotChangingControl(sessionId: string, operation: string): void {
    if (this.pendingSessionControls.has(sessionId)) {
      throw new Error(`Cannot ${operation} while a session control change is running`);
    }
  }

  private reserveSessionControl(sessionId: string, operation: string): () => void {
    this.requireKnownSession(sessionId);
    this.requireSessionNotForking(sessionId, operation);
    this.requireSessionNotClosing(sessionId, operation);
    if (this.prompts.has(sessionId)) {
      throw new Error("Wait for the running prompt before changing session controls");
    }
    if (this.pendingSessionControls.has(sessionId)) {
      throw new Error("A session control change is already running for this session");
    }
    this.pendingSessionControls.add(sessionId);
    let released = false;
    return () => {
      if (released) return;
      released = true;
      this.pendingSessionControls.delete(sessionId);
    };
  }

  private requireNesSession(sessionId: string): TrackedNesSession {
    const session = this.nesSessions.get(sessionId);
    if (!session) throw new Error(`Unknown or inactive NES session: ${sessionId}`);
    return session;
  }

  private requireNesDocument(
    sessionId: string,
    uri: string,
  ): TrackedNesDocument {
    const document = this.requireNesSession(sessionId).documents.get(uri);
    if (!document) throw new Error(`Document is not open in NES session: ${uri}`);
    return document;
  }

  private async rejectNesSuggestions(
    sessionId: string,
    session: TrackedNesSession,
    predicate: (pending: PendingNesSuggestion, id: string) => boolean,
    reason: acp.NesRejectReason,
    requestId: string,
  ): Promise<void> {
    const agent = this.requireAgent();
    for (const [id, pending] of [...session.suggestions]) {
      if (!predicate(pending, id)) continue;
      await agent.notify(acp.methods.agent.nes.reject, {
        sessionId,
        id,
        reason,
      });
      session.suggestions.delete(id);
      this.send({
        type: "acp/nes_suggestion_resolved",
        requestId,
        sessionId,
        suggestionId: id,
        outcome: "rejected",
        reason,
      });
    }
  }

  private documentCapabilities(): acp.NesDocumentEventCapabilities | undefined {
    return this.initialized?.agentCapabilities?.nes?.events?.document ?? undefined;
  }

  private assertDocumentSize(text: string): void {
    if (Buffer.byteLength(text, "utf8") > MAX_DOCUMENT_BYTES) {
      throw new Error(`Document exceeds the ${MAX_DOCUMENT_BYTES} byte limit`);
    }
  }

  private buildNesContext(session: TrackedNesSession): acp.NesSuggestContext | undefined {
    const capabilities = this.initialized?.agentCapabilities?.nes?.context;
    if (!capabilities) return undefined;
    const documents = [...session.documents.values()];
    const context: acp.NesSuggestContext = {};
    if (capabilities.recentFiles != null) {
      const maximum = boundedContextCount(
        capabilities.recentFiles.maxCount,
        documents.length,
        MAX_NES_DOCUMENTS,
      );
      const recent = boundedRecentDocuments(documents, maximum);
      attachNesContextItems(context, "recentFiles", recent.map((document) => ({
        uri: document.uri,
        languageId: document.languageId,
        text: document.text,
      })));
    }
    if (capabilities.editHistory != null) {
      const maximum = boundedContextCount(
        capabilities.editHistory.maxCount,
        session.editHistory.length,
        MAX_NES_CONTEXT_ITEMS,
      );
      attachNesContextItems(context, "editHistory", recentItems(session.editHistory, maximum));
    }
    if (capabilities.userActions != null) {
      const maximum = boundedContextCount(
        capabilities.userActions.maxCount,
        session.userActions.length,
        MAX_NES_CONTEXT_ITEMS,
      );
      attachNesContextItems(context, "userActions", recentItems(session.userActions, maximum));
    }
    if (capabilities.openFiles != null) {
      attachNesContextItems(context, "openFiles", documents.map((document) => ({
        uri: document.uri,
        languageId: document.languageId,
        visibleRange: document.visibleRange,
        lastFocusedMs: document.lastFocusedMs,
      })));
    }
    return Object.keys(context).length > 0 ? context : undefined;
  }

  private recordNesEdit(
    session: TrackedNesSession,
    document: TrackedNesDocument,
    previous: string,
    next: string,
  ): void {
    if (previous === next) return;
    const capabilities = this.initialized?.agentCapabilities?.nes?.context;
    if (capabilities?.editHistory != null) {
      const diff = unifiedTextDiff(document.uri, previous, next);
      if (Buffer.byteLength(diff, "utf8") <= MAX_NES_HISTORY_ENTRY_BYTES) {
        pushBounded(
          session.editHistory,
          { uri: document.uri, diff },
          boundedContextCount(
            capabilities.editHistory.maxCount,
            MAX_NES_CONTEXT_ITEMS,
            MAX_NES_CONTEXT_ITEMS,
          ),
        );
      }
    }

    const change = fullOrIncrementalChange(previous, next, "incremental");
    if (change.range == null) return;
    const start = offsetAt(previous, change.range.start);
    const end = offsetAt(previous, change.range.end);
    const action = start === end
      ? change.text.length === 1 ? "insertChar" : "insertText"
      : change.text.length === 0 ? "delete" : "replace";
    this.recordNesUserAction(session, {
      action,
      uri: document.uri,
      position: positionAt(next, start + change.text.length),
      timestampMs: document.lastAccessedMs,
    });
  }

  private recordNesUserAction(
    session: TrackedNesSession,
    action: acp.NesUserAction,
  ): void {
    const capabilities = this.initialized?.agentCapabilities?.nes?.context?.userActions;
    if (capabilities == null) return;
    pushBounded(
      session.userActions,
      action,
      boundedContextCount(
        capabilities.maxCount,
        MAX_NES_CONTEXT_ITEMS,
        MAX_NES_CONTEXT_ITEMS,
      ),
    );
  }

  private validateNesSuggestion(
    session: TrackedNesSession,
    suggestion: acp.NesSuggestion,
  ): string | undefined {
    validateAgentIdentifier(suggestion.id, "NES suggestion ID");
    const document = session.documents.get(suggestion.uri);
    if (!document) {
      throw new Error(`NES suggestion targets a document that is not open: ${suggestion.uri}`);
    }
    switch (suggestion.kind) {
      case "edit":
        if (suggestion.edits.length > MAX_NES_EDITS) {
          throw new Error(`NES suggestion exceeds the ${MAX_NES_EDITS} edit limit`);
        }
        const expected = applyTextEdits(document.text, suggestion.edits);
        this.assertDocumentSize(expected);
        if (suggestion.cursorPosition != null) {
          offsetAt(expected, suggestion.cursorPosition);
        }
        return expected;
      case "jump":
        offsetAt(document.text, suggestion.position);
        return undefined;
      case "rename":
      case "searchAndReplace":
        throw new Error(`Agent returned unadvertised NES suggestion kind: ${suggestion.kind}`);
    }
  }

  private requireListedSession(sessionId: string): acp.SessionInfo {
    const session = this.listedSessionInfo.get(sessionId);
    if (!this.listedSessions.has(sessionId) || session == null) {
      throw new Error(`Session was not returned by session/list: ${sessionId}`);
    }
    return session;
  }

  private reserveSessionCreation(): () => void {
    if (
      this.sessions.size +
      this.pendingSessionCreations +
      this.pendingSessionAttachments.size >= MAX_TRACKED_SESSIONS
    ) {
      throw new Error(`Active session limit reached (${MAX_TRACKED_SESSIONS})`);
    }
    this.pendingSessionCreations += 1;
    let released = false;
    return () => {
      if (released) return;
      released = true;
      this.pendingSessionCreations -= 1;
      if (this.pendingSessionCreations === 0) this.clearPendingCreationReplays();
    };
  }

  private reserveSessionAttachment(sessionId: string): () => void {
    if (this.sessions.has(sessionId)) throw new Error(`Session is already active: ${sessionId}`);
    if (this.pendingSessionDeletions.has(sessionId)) {
      throw new Error(`Session is being deleted: ${sessionId}`);
    }
    if (this.pendingSessionAttachments.has(sessionId)) {
      throw new Error(`Session attachment is already running: ${sessionId}`);
    }
    if (
      this.sessions.size +
      this.pendingSessionCreations +
      this.pendingSessionAttachments.size >= MAX_TRACKED_SESSIONS
    ) {
      throw new Error(`Active session limit reached (${MAX_TRACKED_SESSIONS})`);
    }
    this.pendingSessionAttachments.add(sessionId);
    this.pendingSessionUpdates.set(sessionId, createSessionUpdateValidationState());
    let released = false;
    return () => {
      if (released) return;
      released = true;
      this.pendingSessionAttachments.delete(sessionId);
      this.pendingSessionUpdates.delete(sessionId);
    };
  }

  private trackSession(
    sessionId: string,
    response:
      | acp.NewSessionResponse
      | acp.LoadSessionResponse
      | acp.ResumeSessionResponse
      | acp.ForkSessionResponse,
    cwd: string,
    allowedPendingAttachment?: string,
  ): acp.SessionNotification[] {
    validateAgentSessionId(sessionId);
    if (
      this.sessions.has(sessionId) ||
      this.nesSessions.has(sessionId) ||
      this.pendingSessionDeletions.has(sessionId) ||
      (
        this.pendingSessionAttachments.has(sessionId) &&
        sessionId !== allowedPendingAttachment
      )
    ) {
      throw new Error(`Agent returned a duplicate active session ID: ${sessionId}`);
    }
    validateSessionControls(response.modes, response.configOptions);
    const pendingCreation = allowedPendingAttachment == null
      ? this.pendingCreationReplays.get(sessionId)
      : undefined;
    const updateValidation = allowedPendingAttachment == null
      ? pendingCreation?.updateValidation ?? createSessionUpdateValidationState()
      : this.pendingSessionUpdates.get(allowedPendingAttachment)
        ?? createSessionUpdateValidationState();
    if (updateValidation.invalidReason != null) {
      throw new Error(`Agent session replay was invalid: ${updateValidation.invalidReason}`);
    }
    this.sessions.set(sessionId, {
      cwd,
      modes: response.modes,
      configOptions: response.configOptions ?? [],
      updateValidation,
    });
    if (!pendingCreation) return [];
    this.pendingCreationReplays.delete(sessionId);
    this.pendingCreationReplayCount -= pendingCreation.notifications.length;
    this.pendingCreationReplayBytes -= pendingCreation.bytes;
    return pendingCreation.notifications;
  }

  private requireOfferedMode(sessionId: string, modeId: string): void {
    const session = this.requireKnownSession(sessionId);
    if (!session.modes?.availableModes.some(({ id }) => id === modeId)) {
      throw new Error(`Mode was not offered by the Agent: ${modeId}`);
    }
  }

  private requireOfferedConfig(
    sessionId: string,
    configId: string,
    value: string | boolean,
  ): void {
    const session = this.requireKnownSession(sessionId);
    const option = session.configOptions.find(({ id }) => id === configId);
    if (!option) throw new Error(`Config option was not offered by the Agent: ${configId}`);
    if (option.type === "boolean") {
      if (typeof value !== "boolean") {
        throw new Error(`Config option ${configId} requires a boolean value`);
      }
      return;
    }
    if (typeof value !== "string" || !flattenConfigValues(option.options).includes(value)) {
      throw new Error(`Config option ${configId} value was not offered by the Agent`);
    }
  }

  private updateTrackedMode(sessionId: string, modeId: string): void {
    const session = this.requireKnownSession(sessionId);
    if (session.modes) session.modes = { ...session.modes, currentModeId: modeId };
  }

  private observeSessionUpdate(notification: acp.SessionNotification): boolean {
    validateBrowserRelayValue(notification, "Agent session/update notification");
    const session = this.sessions.get(notification.sessionId);
    let pendingCreation = this.pendingCreationReplays.get(notification.sessionId);
    let updateValidation = session?.updateValidation
      ?? this.pendingSessionUpdates.get(notification.sessionId)
      ?? pendingCreation?.updateValidation;
    if (!updateValidation && this.pendingSessionCreations > 0) {
      validateAgentSessionId(notification.sessionId);
      if (this.pendingCreationReplays.size >= this.pendingSessionCreations) {
        throw new Error(
          "Agent sent updates for more session IDs than pending session creations",
        );
      }
      pendingCreation = {
        updateValidation: createSessionUpdateValidationState(),
        notifications: [],
        bytes: 0,
      };
      this.pendingCreationReplays.set(notification.sessionId, pendingCreation);
      updateValidation = pendingCreation.updateValidation;
    }
    if (!updateValidation) {
      // A late notification may race a session close or a failed load. Zed
      // drops these because there is no thread to apply them to; do the same
      // instead of leaking the event into whichever thread is visible now.
      return false;
    }
    const update = notification.update;
    try {
      validateAndTrackSessionUpdate(updateValidation, update, {
        assertTerminalReference: (terminalId) => {
          this.terminals.assertReference(terminalId, notification.sessionId);
        },
      });
    } catch (error) {
      if (!session) updateValidation.invalidReason ??= errorMessage(error);
      throw error;
    }
    if (pendingCreation) {
      const bytes = Buffer.byteLength(JSON.stringify(notification), "utf8");
      if (this.pendingCreationReplayCount >= MAX_PENDING_CREATION_UPDATES) {
        const error = new Error(
          `Agent exceeded ${MAX_PENDING_CREATION_UPDATES} updates before completing session creation`,
        );
        updateValidation.invalidReason ??= error.message;
        throw error;
      }
      if (
        this.pendingCreationReplayBytes + bytes >
        MAX_PENDING_CREATION_REPLAY_BYTES
      ) {
        const error = new Error(
          `Agent session creation replay exceeds ${MAX_PENDING_CREATION_REPLAY_BYTES} bytes`,
        );
        updateValidation.invalidReason ??= error.message;
        throw error;
      }
      pendingCreation.notifications.push(notification);
      pendingCreation.bytes += bytes;
      this.pendingCreationReplayCount += 1;
      this.pendingCreationReplayBytes += bytes;
      return false;
    }
    if (!session) return true;
    if (update.sessionUpdate === "current_mode_update") {
      this.updateTrackedMode(notification.sessionId, update.currentModeId);
    } else if (update.sessionUpdate === "config_option_update") {
      session.configOptions = update.configOptions;
    }
    return true;
  }

  private clearPendingCreationReplays(): void {
    this.pendingCreationReplays.clear();
    this.pendingCreationReplayCount = 0;
    this.pendingCreationReplayBytes = 0;
  }

  private configuredAdditionalDirectories(): string[] | undefined {
    return this.transport === "stdio" && this.additionalDirectories.length > 0
      ? this.additionalDirectories
      : undefined;
  }

  private resolveNewSessionCwd(requestedCwd: string | undefined): string {
    if (requestedCwd != null) return requestedCwd;
    if (this.transport === "stdio") return this.options.cwd;
    throw new Error("session/new requires an absolute Agent workspace for remote transports");
  }

  private requireLocalWorkspaceCapability(method: string): void {
    if (this.transport !== "stdio") {
      throw new Error(`${method} is unavailable for remote Agent transports`);
    }
  }

  private validateConfiguredCapabilities(response: acp.InitializeResponse): void {
    const capabilities = response.agentCapabilities;
    if (
      this.additionalDirectories.length > 0 &&
      capabilities?.sessionCapabilities?.additionalDirectories == null
    ) {
      throw new Error(
        "Additional directories were configured but the Agent did not advertise session additionalDirectories",
      );
    }
    for (const server of this.mcpServers) {
      const type = mcpServerType(server);
      if (type === "http" && capabilities?.mcpCapabilities?.http !== true) {
        throw new Error(
          `MCP server ${server.name} uses HTTP but the Agent did not advertise mcpCapabilities.http`,
        );
      }
      if (type === "sse" && capabilities?.mcpCapabilities?.sse !== true) {
        throw new Error(
          `MCP server ${server.name} uses SSE but the Agent did not advertise mcpCapabilities.sse`,
        );
      }
      if (type === "acp" && capabilities?.mcpCapabilities?.acp !== true) {
        throw new Error(
          `MCP server ${server.name} uses ACP transport but the Agent did not advertise mcpCapabilities.acp`,
        );
      }
    }
  }

  private requireOfferedAuthMethod(methodId: string): acp.AuthMethod {
    const method = this.initialized?.authMethods?.find(({ id }) => id === methodId);
    if (!method) throw new Error("Authentication method was not offered by the Agent");
    return method;
  }

  private assertPromptCapabilities(prompt: acp.ContentBlock[]): void {
    const capabilities = this.initialized?.agentCapabilities?.promptCapabilities;
    for (const block of prompt) {
      if (block.type === "image" && capabilities?.image !== true) {
        throw new Error("Agent did not advertise image prompt support");
      }
      if (block.type === "audio" && capabilities?.audio !== true) {
        throw new Error("Agent did not advertise audio prompt support");
      }
      if (block.type === "resource" && capabilities?.embeddedContext !== true) {
        throw new Error("Agent did not advertise embedded context support");
      }
    }
  }

  private send(event: ServerEvent): void {
    if (this.closed) return;
    if (this.socket.readyState === this.socket.OPEN) {
      const serialized = JSON.stringify(event);
      const bytes = Buffer.byteLength(serialized, "utf8");
      if (bytes > MAX_BRIDGE_MESSAGE_BYTES) {
        throw new Error(
          `Browser bridge event ${event.type} exceeds ${MAX_BRIDGE_MESSAGE_BYTES} bytes`,
        );
      }
      this.socket.send(serialized);
    }
  }

  private queueTerminalSnapshot(snapshot: TerminalSnapshot): void {
    if (this.closed) return;
    this.pendingTerminalSnapshots.set(snapshot.terminalId, snapshot);
    if (snapshot.released || snapshot.exitStatus != null) {
      this.flushTerminalSnapshots();
      return;
    }
    this.terminalSnapshotTimer ??= setTimeout(
      () => this.flushTerminalSnapshots(),
      TERMINAL_SNAPSHOT_INTERVAL_MS,
    );
  }

  private flushTerminalSnapshots(): void {
    if (this.terminalSnapshotTimer != null) {
      clearTimeout(this.terminalSnapshotTimer);
      this.terminalSnapshotTimer = undefined;
    }
    const snapshots = [...this.pendingTerminalSnapshots.values()];
    this.pendingTerminalSnapshots.clear();
    for (const terminal of snapshots) {
      const terminalFrame = terminal.released || terminal.exitStatus != null;
      const outputBytes = Buffer.byteLength(terminal.output, "utf8");
      const sentBytes = this.terminalSnapshotBytes.get(terminal.terminalId) ?? 0;
      if (!terminalFrame && sentBytes + outputBytes > MAX_TERMINAL_LIVE_SNAPSHOT_BYTES) {
        continue;
      }
      this.send({ type: "acp/terminal_state", terminal });
      if (terminal.released) this.terminalSnapshotBytes.delete(terminal.terminalId);
      else this.terminalSnapshotBytes.set(terminal.terminalId, sentBytes + outputBytes);
    }
  }

  private clearTerminalSnapshots(): void {
    if (this.terminalSnapshotTimer != null) clearTimeout(this.terminalSnapshotTimer);
    this.terminalSnapshotTimer = undefined;
    this.pendingTerminalSnapshots.clear();
    this.terminalSnapshotBytes.clear();
  }

  private fail(error: unknown): void {
    if (this.closed) return;
    this.send({
      type: "bridge/error",
      message: errorMessage(error),
      ...requestErrorFields(error),
    });
    this.send({ type: "bridge/phase", phase: "error" });
  }
}

function validateAuthMethods(
  response: acp.InitializeResponse,
  terminalAuthSupported: boolean,
): void {
  const methods = response.authMethods ?? [];
  if (methods.length > MAX_AUTH_METHODS) {
    throw new Error(`Agent advertised more than ${MAX_AUTH_METHODS} authentication methods`);
  }
  const ids = new Set<string>();
  for (const method of methods) {
    validateAgentIdentifier(method.id, "Agent authentication method ID");
    if (ids.has(method.id)) {
      throw new Error(`Agent advertised duplicate authentication method ID: ${method.id}`);
    }
    ids.add(method.id);
    if (method.name.length === 0 || method.name.length > MAX_AUTH_METHOD_NAME_LENGTH) {
      throw new Error(
        `Agent authentication method name must contain between 1 and ${MAX_AUTH_METHOD_NAME_LENGTH} characters`,
      );
    }
    if (
      method.description != null &&
      method.description.length > MAX_AUTH_METHOD_DESCRIPTION_LENGTH
    ) {
      throw new Error(
        `Agent authentication method description exceeds ${MAX_AUTH_METHOD_DESCRIPTION_LENGTH} characters`,
      );
    }
    if ("type" in method && method.type === "terminal") {
      if (!terminalAuthSupported) {
        throw new Error(
          "Agent advertised terminal authentication although the remote transport cannot reproduce its invocation",
        );
      }
      validateTerminalAuthMethod(method);
    }
  }
  if (methods.length === 0 && response.agentCapabilities?.auth?.logout != null) {
    throw new Error("Agent advertised logout without any authentication methods");
  }
}

function errorMessage(error: unknown): string {
  let message: string;
  if (error instanceof acp.RequestError) {
    const detail = formatErrorDetail(error.data);
    message = `ACP error ${error.code}: ${error.message}${detail ? ` — ${detail}` : ""}`;
  } else {
    message = error instanceof Error ? error.message : String(error);
  }
  return message.length > MAX_BRIDGE_ERROR_MESSAGE_CHARS
    ? `${message.slice(0, MAX_BRIDGE_ERROR_MESSAGE_CHARS)}…`
    : message;
}

function requestErrorFields(error: unknown): Pick<
  Extract<ServerEvent, { type: "bridge/error" }>,
  "code" | "data" | "dataTruncated" | "dataBytes"
> {
  if (!(error instanceof acp.RequestError)) return {};
  if (error.data === undefined) return { code: error.code };
  try {
    const serialized = JSON.stringify(error.data);
    if (serialized === undefined) {
      return { code: error.code, dataTruncated: true };
    }
    const dataBytes = Buffer.byteLength(serialized, "utf8");
    if (dataBytes > MAX_BRIDGE_ERROR_DATA_BYTES) {
      return { code: error.code, dataTruncated: true, dataBytes };
    }
    return { code: error.code, data: error.data, dataBytes };
  } catch {
    return { code: error.code, dataTruncated: true };
  }
}

function validateAgentSessionId(sessionId: string): void {
  validateAgentIdentifier(sessionId, "Agent session ID");
}

function validateAgentIdentifier(value: string, label: string): void {
  if (!isValidAgentIdentifier(value)) {
    throw new Error(
      `${label} must contain between 1 and ${MAX_AGENT_IDENTIFIER_LENGTH} characters`,
    );
  }
}

function isValidAgentIdentifier(value: unknown): value is string {
  return typeof value === "string" &&
    value.length > 0 &&
    value.length <= MAX_AGENT_IDENTIFIER_LENGTH;
}

function validateBrowserRelayValue(value: unknown, label: string): void {
  const bytes = Buffer.byteLength(JSON.stringify(value), "utf8");
  if (bytes > MAX_BROWSER_RELAY_VALUE_BYTES) {
    throw new Error(
      `${label} exceeds ${MAX_BROWSER_RELAY_VALUE_BYTES} browser relay bytes`,
    );
  }
}

function formatErrorDetail(data: unknown): string | undefined {
  if (data == null) return undefined;
  let detail: string;
  if (typeof data === "string") {
    detail = data;
  } else {
    try {
      detail = JSON.stringify(data);
    } catch {
      detail = String(data);
    }
  }
  return detail.length > MAX_BRIDGE_ERROR_DETAIL_CHARS
    ? `${detail.slice(0, MAX_BRIDGE_ERROR_DETAIL_CHARS)}…`
    : detail;
}

function getUrlElicitationId(
  request: acp.CreateElicitationRequest,
): string | undefined {
  return request.mode === "url" &&
    "elicitationId" in request &&
    typeof request.elicitationId === "string"
    ? request.elicitationId
    : undefined;
}

function mcpServerType(server: acp.McpServer): "stdio" | "http" | "sse" | "acp" {
  if (!("type" in server)) return "stdio";
  if (server.type === "http" || server.type === "sse" || server.type === "acp") {
    return server.type;
  }
  throw new Error("Unsupported MCP server transport");
}

function validateAcpMcpProviders(
  servers: acp.McpServer[],
  providers: AcpMcpProvider[],
): void {
  const declared = new Map<string, string>();
  for (const server of servers) {
    if (!("type" in server) || server.type !== "acp") continue;
    if (declared.has(server.serverId)) {
      throw new Error(`Duplicate declared ACP MCP serverId: ${server.serverId}`);
    }
    declared.set(server.serverId, server.name);
  }
  for (const provider of providers) {
    const declaredName = declared.get(provider.serverId);
    if (declaredName == null) {
      throw new Error(`ACP MCP provider ${provider.serverId} was not declared to the Agent`);
    }
    if (declaredName !== provider.name) {
      throw new Error(
        `ACP MCP provider ${provider.serverId} name does not match its declared server`,
      );
    }
  }
  for (const [serverId] of declared) {
    if (!providers.some((provider) => provider.serverId === serverId)) {
      throw new Error(`Declared ACP MCP server ${serverId} has no client-side provider`);
    }
  }
}

function flattenConfigValues(options: acp.SessionConfigSelectOptions): string[] {
  return options.flatMap((option) =>
    "options" in option
      ? option.options.map(({ value }) => value)
      : [option.value],
  );
}

function boundedContextCount(
  requested: number | null | undefined,
  available: number,
  maximum: number,
): number {
  if (requested == null) return Math.min(available, maximum);
  if (!Number.isSafeInteger(requested) || requested <= 0) return 0;
  return Math.min(requested, available, maximum);
}

function boundedRecentDocuments(
  documents: TrackedNesDocument[],
  maximum: number,
): TrackedNesDocument[] {
  if (maximum <= 0) return [];
  return [...documents]
    .sort((left, right) => left.lastAccessedMs - right.lastAccessedMs)
    .slice(-maximum);
}

function recentItems<T>(items: T[], maximum: number): T[] {
  return maximum <= 0 ? [] : items.slice(-maximum);
}

function attachNesContextItems(
  context: acp.NesSuggestContext,
  key: "recentFiles" | "editHistory" | "userActions" | "openFiles",
  items: unknown[],
): void {
  const accepted: unknown[] = [];
  for (let index = items.length - 1; index >= 0; index -= 1) {
    accepted.unshift(items[index]);
    const record = context as Record<string, unknown>;
    record[key] = accepted;
    if (
      Buffer.byteLength(JSON.stringify(accepted), "utf8") > MAX_NES_CONTEXT_FIELD_BYTES ||
      Buffer.byteLength(JSON.stringify(context), "utf8") > MAX_NES_CONTEXT_BYTES
    ) {
      accepted.shift();
      if (accepted.length === 0) delete record[key];
      else record[key] = accepted;
    }
  }
}

function pushBounded<T>(items: T[], item: T, maximum: number): void {
  if (maximum <= 0) return;
  items.push(item);
  if (items.length > maximum) items.splice(0, items.length - maximum);
}

function nesOrderedSession(command: ClientCommand): string | undefined {
  switch (command.type) {
    case "nes/suggest":
    case "nes/accept":
    case "nes/reject":
    case "nes/close":
    case "document/open":
    case "document/change":
    case "document/save":
    case "document/focus":
    case "document/close":
      return command.sessionId;
    default:
      return undefined;
  }
}
