import { expect, test } from "@playwright/test";
import { join } from "node:path";
import { createServer } from "vite";
import { collectBrowserErrors } from "./browser-errors";

test("folds prompts only when their rendered height exceeds the preview and follows layout changes", async ({ page }, testInfo) => {
  const vite = await createServer({
    configFile: join(process.cwd(), "vite.config.ts"), logLevel: "error",
    server: { host: "127.0.0.1", port: 0, strictPort: false },
  });
  let releaseImage!: () => void;
  const imageReady = new Promise<void>((resolve) => { releaseImage = resolve; });
  try {
    await vite.listen();
    const address = vite.httpServer!.address();
    if (address == null || typeof address === "string") throw new Error("Vite did not bind a port");
    const browserErrors = collectBrowserErrors(page);
    const samples = [
      `Blank source lines${"\n".repeat(40)}Still a short message.`,
      `[Long URL, short label](https://example.com/${"path/".repeat(180)})`,
      Array.from({ length: 9 }, (_, index) => `Nine rendered lines ${index + 1}`).join("  \n"),
      Array.from({ length: 10 }, (_, index) => `Ten rendered lines ${index + 1}`).join("  \n"),
      Array.from({ length: 6 }, (_, index) => `# Tall heading ${index + 1}`).join("\n\n"),
      `Responsive paragraph：${"这段文字随窗口宽度自动换行。".repeat(20)}`,
      "Short code block\n\n```sh\nattyd -- goose acp\n```",
      `Tall table\n\n| File |\n| --- |\n${"| example.ts |\n".repeat(12)}`,
      "Delayed image\n\n![A growing image](/prompt-height.svg)",
      `${"Keyboard access  \n".repeat(12)}\n[Hidden link](https://example.com/guide)`,
    ];
    await page.route("**/dev/style-session.json", async (route) => {
      const response = await route.fetch();
      const session = await response.json();
      session.updates = samples.flatMap((text, index) => [
        { sessionUpdate: "user_message_chunk", messageId: `prompt-height-${index}`, content: { type: "text", text } },
        { sessionUpdate: "agent_message_chunk", messageId: `answer-height-${index}`, content: { type: "text", text: "Ready." } },
      ]);
      await route.fulfill({ json: session });
    });
    await page.route("**/prompt-height.svg", async (route) => {
      await imageReady;
      await route.fulfill({ contentType: "image/svg+xml", body: '<svg xmlns="http://www.w3.org/2000/svg" width="280" height="320"><rect width="280" height="320" fill="lightblue"/></svg>' });
    });
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto(`http://127.0.0.1:${address.port}/style-showcase.html`, { waitUntil: "domcontentloaded" });
    const prompts = page.locator(".message-user");
    await expect(prompts).toHaveCount(samples.length);
    const prompt = (label: string) => prompts.filter({ hasText: label });
    for (const label of ["Blank source lines", "Long URL, short label", "Nine rendered lines", "Responsive paragraph", "Short code block", "Delayed image"]) {
      await expect(prompt(label).locator(".prompt-text")).toHaveAttribute("data-collapsible", "false");
      await expect(prompt(label).locator(".prompt-text-toggle")).toHaveCount(0);
      await expect(prompt(label).locator(".prompt-text-body")).toHaveCSS("mask-image", "none");
    }
    for (const label of ["Ten rendered lines", "Tall heading", "Tall table", "Keyboard access"]) {
      await expect(prompt(label).getByRole("button", { name: "Show full message", exact: true })).toHaveAttribute("aria-expanded", "false");
    }

    const responsive = prompt("Responsive paragraph");
    await page.setViewportSize({ width: 390, height: 844 });
    await expect(responsive.getByRole("button", { name: "Show full message", exact: true })).toBeVisible();
    const collapsedHeight = await responsive.locator(".prompt-text-body").evaluate((element) => element.getBoundingClientRect().height);
    await responsive.scrollIntoViewIfNeeded();
    await responsive.screenshot({ path: testInfo.outputPath("responsive-prompt-collapsed.png") });
    await responsive.getByRole("button", { name: "Show full message", exact: true }).click();
    await expect(responsive.locator(".prompt-text-body")).toHaveCSS("mask-image", "none");
    expect(await responsive.locator(".prompt-text-body").evaluate((element) => element.getBoundingClientRect().height)).toBeGreaterThan(collapsedHeight);
    await responsive.getByRole("button", { name: "Show less", exact: true }).click();
    await expect(responsive.locator(".prompt-text")).toHaveAttribute("data-expanded", "false");
    await page.setViewportSize({ width: 1280, height: 900 });
    await expect(responsive.locator(".prompt-text-toggle")).toHaveCount(0);
    await expect(responsive.locator(".prompt-text-body")).toHaveCSS("mask-image", "none");

    releaseImage();
    await expect(prompt("Delayed image").getByRole("button", { name: "Show full message", exact: true })).toBeVisible();
    const keyboard = prompt("Keyboard access");
    await keyboard.getByRole("link", { name: "Hidden link", exact: true }).focus();
    await expect(keyboard.getByRole("button", { name: "Show less", exact: true })).toHaveAttribute("aria-expanded", "true");
    await expect(keyboard.getByRole("link", { name: "Hidden link", exact: true })).toBeInViewport();
    expect(browserErrors).toEqual([]);
  } finally {
    releaseImage();
    await vite.close();
  }
});
