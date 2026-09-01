import { writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { mkdtemp, rm } from "node:fs/promises";
import { afterEach, describe, expect, it } from "vitest";
import { parseMcpConfig, parseOptions } from "../server/options";

const cleanups: string[] = [];

afterEach(async () => {
  await Promise.all(cleanups.splice(0).map((path) => rm(path, { recursive: true, force: true })));
});

describe("attyd static client configuration", () => {
  it("defaults to stdio and accepts ACP HTTP and WebSocket endpoints", () => {
    const local = parseOptions([]);
    expect(local.transport).toBe("stdio");
    expect(local.command).toEqual([join(process.cwd(), "bin/goose"), "acp"]);

    const http = parseOptions(["-t", "http", "--", "https://agent.example/acp"]);
    expect(http.transport).toBe("http");
    expect(http.command).toEqual(["https://agent.example/acp"]);

    const sseAlias = parseOptions([
      "--transport",
      "sse",
      "http://127.0.0.1:3284/acp",
    ]);
    expect(sseAlias.transport).toBe("http");
    expect(sseAlias.command).toEqual(["http://127.0.0.1:3284/acp"]);

    const webSocket = parseOptions(["-t", "ws", "wss://agent.example/acp"]);
    expect(webSocket.transport).toBe("ws");
    expect(webSocket.command).toEqual(["wss://agent.example/acp"]);
  });

  it("validates remote transport targets", () => {
    expect(() => parseOptions(["-t", "http"])).toThrow("requires an ACP endpoint URL");
    expect(() => parseOptions(["-t", "ws", "https://agent.example/acp"]))
      .toThrow("Invalid ws ACP endpoint URL protocol");
    expect(() => parseOptions(["-t", "http", "ws://agent.example/acp"]))
      .toThrow("Invalid http ACP endpoint URL protocol");
    expect(() => parseOptions(["-t", "ws", "ws://agent.example/acp", "extra"]))
      .toThrow("accepts exactly one ACP endpoint URL");
    expect(() => parseOptions([
      "-t", "ws", "--add-dir", "/tmp/shared", "ws://agent.example/acp",
    ])).toThrow("only available with the stdio transport");
    expect(() => parseOptions(["-t", "pipe"])).toThrow("Invalid transport");
  });

  it("parses stdio, HTTP, SSE, and client-provided ACP MCP definitions", () => {
    const servers = parseMcpConfig(JSON.stringify({
      mcpServers: [
        { name: "local", type: "stdio", command: process.execPath, args: ["server.js"], env: [{ name: "A", value: "B" }] },
        { name: "remote", type: "http", url: "https://example.test/mcp", headers: [{ name: "Authorization", value: "secret" }] },
        { name: "events", type: "sse", url: "http://127.0.0.1:9000/sse" },
        { name: "client-tools", type: "acp", serverId: "client-tools-v1", command: process.execPath },
      ],
    }));

    expect(servers).toHaveLength(4);
    expect(servers[0]).toMatchObject({ name: "local", command: process.execPath });
    expect(servers[1]).toMatchObject({ name: "remote", type: "http" });
    expect(servers[2]).toMatchObject({ name: "events", type: "sse", headers: [] });
    expect(servers[3]).toEqual({ name: "client-tools", type: "acp", serverId: "client-tools-v1" });
  });

  it("rejects ambiguous or unsafe MCP definitions", () => {
    expect(() => parseMcpConfig('[{"name":"x","command":"relative"}]')).toThrow("absolute path");
    expect(() => parseMcpConfig('[{"name":"x","type":"http","url":"file:///tmp/x"}]')).toThrow("HTTP(S)");
    expect(() => parseMcpConfig('[{"name":"x","type":"acp","serverId":"x"}]')).toThrow("command");
    expect(() => parseMcpConfig('[{"name":"x","type":"acp","serverId":"x","command":"relative"}]')).toThrow("absolute path");
    expect(() => parseMcpConfig(JSON.stringify([
      { name: "one", type: "acp", serverId: "same", command: process.execPath },
      { name: "two", type: "acp", serverId: "same", command: process.execPath },
    ]))).toThrow("duplicate ACP MCP serverId");
    expect(() => parseMcpConfig(JSON.stringify([
      { name: "same", command: process.execPath },
      { name: "same", command: process.execPath },
    ]))).toThrow("duplicate");
  });

  it("loads repeatable roots and MCP config without treating them as Agent args", async () => {
    const directory = await mkdtemp(join(tmpdir(), "attyd-options-"));
    cleanups.push(directory);
    const config = join(directory, "mcp.json");
    await writeFile(config, JSON.stringify([
      { name: "local", command: process.execPath },
      {
        name: "client-tools",
        type: "acp",
        serverId: "client-tools",
        command: process.execPath,
        env: [{ name: "SECRET", value: "not-for-agent" }],
      },
    ]), "utf8");

    const options = parseOptions([
      "--add-dir", directory,
      "--add-dir", directory,
      "--mcp-config", config,
      "--", "agent", "acp",
    ]);
    expect(options.additionalDirectories).toEqual([directory]);
    expect(options.mcpServers).toHaveLength(2);
    expect(options.mcpServers[1]).toEqual({
      name: "client-tools",
      type: "acp",
      serverId: "client-tools",
    });
    expect(options.acpMcpProviders[0]).toMatchObject({
      serverId: "client-tools",
      command: process.execPath,
      env: [{ name: "SECRET", value: "not-for-agent" }],
    });
    expect(options.command).toEqual(["agent", "acp"]);

    const overflow = join(directory, "overflow.json");
    await writeFile(overflow, JSON.stringify(Array.from({ length: 31 }, (_, index) => ({
      name: `overflow-${index}`,
      command: process.execPath,
    }))), "utf8");
    expect(() => parseOptions([
      "--mcp-config", config,
      "--mcp-config", overflow,
      "--", "agent", "acp",
    ])).toThrow("more than 32 MCP servers");
  });
});
