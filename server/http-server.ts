import { createReadStream } from "node:fs";
import { stat } from "node:fs/promises";
import { createServer, type IncomingMessage, type ServerResponse } from "node:http";
import type { AddressInfo } from "node:net";
import { extname, join, normalize } from "node:path";
import { fileURLToPath } from "node:url";
import { WebSocketServer } from "ws";
import { MAX_BRIDGE_MESSAGE_BYTES } from "../shared/bridge.js";
import { AcpBridge } from "./acp-bridge.js";
import type { ServerOptions } from "./options.js";

const mimeTypes: Record<string, string> = {
  ".css": "text/css; charset=utf-8",
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".json": "application/json; charset=utf-8",
  ".map": "application/json; charset=utf-8",
  ".svg": "image/svg+xml",
  ".woff2": "font/woff2",
};

export interface RunningHttpServer {
  port: number;
  close: () => Promise<void>;
}

export async function startHttpServer(options: ServerOptions): Promise<RunningHttpServer> {
  const vite = options.dev
    ? await import("vite").then(({ createServer }) =>
        createServer({ server: { middlewareMode: true }, appType: "spa" }),
      )
    : undefined;

  const server = createServer((request, response) => {
    if (request.url === "/api/health") {
      response.writeHead(200, { "content-type": "application/json" });
      response.end(JSON.stringify({ ok: true, protocol: "acp/v1" }));
      return;
    }

    if (vite) {
      vite.middlewares(request, response, () => {
        response.writeHead(404).end("Not found");
      });
      return;
    }

    void serveStatic(request, response);
  });

  const webSockets = new WebSocketServer({ noServer: true, maxPayload: MAX_BRIDGE_MESSAGE_BYTES });
  server.on("upgrade", (request, socket, head) => {
    if (request.url !== "/ws") {
      socket.destroy();
      return;
    }
    webSockets.handleUpgrade(request, socket, head, (webSocket) => {
      webSockets.emit("connection", webSocket, request);
    });
  });

  webSockets.on("connection", (socket) => {
    const bridge = new AcpBridge(socket, options);
    socket.on("message", (data) => bridge.receive(data.toString()));
    socket.once("close", () => bridge.close());
    socket.once("error", () => bridge.close());
    void bridge.start().catch((error: unknown) => {
      console.error("ACP bridge failed:", error);
    });
  });

  await new Promise<void>((resolve, reject) => {
    server.once("error", reject);
    server.listen(options.port, options.host, () => resolve());
  });

  const address = server.address() as AddressInfo;

  console.log(`attyd listening on http://${options.host}:${address.port}`);
  console.log(`agent (${options.transport}): ${options.command.join(" ")}`);
  if (options.transport === "stdio") {
    console.log(`default Agent workspace: ${options.cwd}${options.readOnly ? " (read-only)" : ""}`);
  } else {
    console.log("Agent workspace: selected per new thread in the web UI");
  }
  if (options.additionalDirectories.length > 0) {
    console.log(`additional workspace roots: ${options.additionalDirectories.length}`);
  }
  if (options.mcpServers.length > 0) {
    console.log(`MCP servers: ${options.mcpServers.map(({ name }) => name).join(", ")}`);
  }
  return {
    port: address.port,
    close: async () => {
      for (const client of webSockets.clients) client.terminate();
      await new Promise<void>((resolve, reject) => {
        server.close((error) => error ? reject(error) : resolve());
      });
      await vite?.close();
    },
  };
}

async function serveStatic(
  request: IncomingMessage,
  response: ServerResponse,
): Promise<void> {
  const clientRoot = await findClientRoot();
  const url = new URL(request.url ?? "/", "http://localhost");
  let pathname: string;
  try {
    pathname = decodeURIComponent(url.pathname);
  } catch {
    response.writeHead(400).end("Malformed URL path");
    return;
  }
  const relativePath = normalize(pathname).replace(
    /^(\.\.(\/|\\|$))+|^[\\/]+/,
    "",
  );
  let file = join(clientRoot, relativePath || "index.html");

  try {
    if (!(await stat(file)).isFile()) file = join(clientRoot, "index.html");
  } catch {
    file = join(clientRoot, "index.html");
  }

  try {
    const info = await stat(file);
    response.writeHead(200, {
      "content-type": mimeTypes[extname(file)] ?? "application/octet-stream",
      "content-length": info.size,
    });
    createReadStream(file).pipe(response);
  } catch {
    response.writeHead(404).end("Not found");
  }
}

async function findClientRoot(): Promise<string> {
  const candidates = [
    fileURLToPath(new URL("../../client/", import.meta.url)),
    fileURLToPath(new URL("../dist/client/", import.meta.url)),
  ];
  for (const candidate of candidates) {
    try {
      if ((await stat(candidate)).isDirectory()) return candidate;
    } catch {
      // Try the source-tree fixture location next.
    }
  }
  return candidates[0];
}
