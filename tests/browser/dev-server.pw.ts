import { expect, test } from "@playwright/test";
import { cp, mkdtemp, readFile, rm, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { createServer as createTcpServer } from "node:net";
import { join } from "node:path";
import { createServer, type ViteDevServer } from "vite";
import { startRustTestServer, type RustTestServer } from "../../scripts/rust-test-server";

test("proxies Vite and Fast Refresh through the host while preserving the session and draft", async ({ page, request }) => {
  test.setTimeout(90_000);
  const workspace = process.cwd();
  const directory = await mkdtemp(join(tmpdir(), "attyd-dev-browser-"));
  let vite: ViteDevServer | undefined;
  let host: RustTestServer | undefined;
  try {
    // Exercise real source modules without touching a developer's working files.
    await Promise.all([
      cp(join(workspace, "web"), join(directory, "web"), { recursive: true }),
      cp(join(workspace, "shared"), join(directory, "shared"), { recursive: true }),
      symlink(join(workspace, "node_modules"), join(directory, "node_modules"), "junction"),
    ]);
    const source = join(directory, "web/src");
    const probePath = join(source, "dev-probe.tsx");
    const cssPath = join(source, "dev-probe.css");
    const probe = `import { useState } from "react";
import "./dev-probe.css";
export default function DevProbe() {
  const [count, setCount] = useState(0);
  return <button id="dev-probe" onClick={() => setCount(count + 1)}>Revision one: {count}</button>;
}
`;
    await writeFile(probePath, probe);
    await writeFile(cssPath, "#dev-probe { color: rgb(1, 2, 3); position: fixed; z-index: 999; top: 0; left: 0; }\n");
    const mainPath = join(source, "main.tsx");
    const main = await readFile(mainPath, "utf8");
    await writeFile(mainPath, `import DevProbe from "./dev-probe";\n${main.replace("<App />", "<App /><DevProbe />")}`);

    vite = await createServer({
      configFile: join(workspace, "vite.config.ts"),
      root: join(directory, "web"),
      cacheDir: join(directory, "vite-cache"),
      logLevel: "error",
      server: {
        host: "127.0.0.1",
        port: await availablePort(),
        strictPort: true,
        fs: { allow: [directory, workspace] },
        watch: { usePolling: true, interval: 100 },
      },
    });
    await vite.listen();
    const address = vite.httpServer!.address();
    if (address == null || typeof address === "string") throw new Error("Vite did not bind a TCP port");
    host = await startRustTestServer({
      command: [process.execPath, "--import", "tsx", join(workspace, "tests/fixtures/fake-agent.ts")],
      args: ["--dev-server", `http://127.0.0.1:${address.port}`],
    });
    const origin = `http://127.0.0.1:${host.port}`;
    const sockets: string[] = [];
    page.on("websocket", (socket) => sockets.push(socket.url()));
    const stream = page.waitForResponse((response) =>
      response.url().includes("/api/v1/sessions/saved-session/events") && response.status() === 200
    );
    const response = await page.goto(`${origin}/sessions/saved-session`);
    expect(await response!.text()).toContain("/@vite/client");
    await stream;
    const composer = page.locator('textarea[role="combobox"]');
    await expect(composer).toBeEnabled();
    await composer.fill("stream-follow-flow");
    await composer.press("Enter");
    await expect(page.getByText("Stream follow complete.", { exact: true })).toBeVisible();
    await composer.fill("Keep this unsent draft");
    const sessionUrl = page.url();
    await page.evaluate(() => { document.body.dataset.devDocument = "original"; });
    const button = page.locator("#dev-probe");
    await button.click();
    await expect(button).toHaveText("Revision one: 1");
    await expect(button).toHaveCSS("color", "rgb(1, 2, 3)");

    await writeFile(cssPath, "#dev-probe { color: rgb(4, 5, 6); position: fixed; z-index: 999; top: 0; left: 0; }\n");
    await expect(button).toHaveCSS("color", "rgb(4, 5, 6)");
    await writeFile(probePath, probe.replace("Revision one", "Revision two"));
    await expect(button).toHaveText("Revision two: 1");
    await expect(composer).toHaveValue("Keep this unsent draft");
    await expect(page.getByText("Stream follow complete.", { exact: true })).toBeVisible();
    expect(page.url()).toBe(sessionUrl);
    expect(await page.evaluate(() => document.body.dataset.devDocument)).toBe("original");
    expect(sockets.length).toBeGreaterThan(0);
    expect(sockets.every((socket) => new URL(socket).host === new URL(origin).host)).toBe(true);

    expect((await request.get(`${origin}/api/not-a-route`)).status()).toBe(404);
    expect((await request.get(`${origin}/@vite/client`, { headers: { Origin: "https://untrusted.example" } })).status()).toBe(403);
    await vite.close();
    vite = undefined;
    expect((await request.get(`${origin}/`)).status()).toBe(502);
    expect(await (await request.get(`${origin}/api/health`)).json()).toEqual({
      ok: true, protocol: "acp/v1", backend: "rust",
    });
  } finally {
    await page.close();
    await host?.close();
    await vite?.close();
    await rm(directory, { recursive: true, force: true });
  }
});

function availablePort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const listener = createTcpServer();
    listener.once("error", reject);
    listener.listen(0, "127.0.0.1", () => {
      const address = listener.address();
      if (address == null || typeof address === "string") {
        listener.close();
        reject(new Error("Could not reserve a Vite test port"));
        return;
      }
      listener.close((error) => error ? reject(error) : resolve(address.port));
    });
  });
}
