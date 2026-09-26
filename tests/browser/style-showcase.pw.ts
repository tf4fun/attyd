import { expect, test } from "@playwright/test";
import { join } from "node:path";
import { createServer } from "vite";
import { collectBrowserErrors } from "./browser-errors";

test("shows ACP approval, choice descriptions and cancellation states from static JSON", async ({ page }, testInfo) => {
  test.setTimeout(90_000);
  const vite = await createServer({
    configFile: join(process.cwd(), "vite.config.ts"),
    logLevel: "error",
    server: { host: "127.0.0.1", port: 0, strictPort: false },
  });
  try {
    await vite.listen();
    const address = vite.httpServer!.address();
    if (address == null || typeof address === "string") throw new Error("Vite did not bind a port");
    const browserErrors = collectBrowserErrors(page);
    const apiRequests: string[] = [];
    page.on("request", (request) => {
      if (new URL(request.url()).pathname.startsWith("/api/")) apiRequests.push(request.url());
    });
    await page.goto(`http://127.0.0.1:${address.port}/style-showcase.html`);
    await page.getByRole("button", { name: "审批、设置与状态" }).click();
    const scenario = page.getByLabel("检查场景");
    await expect(page.locator(".permission-subject .markdown")).toContainText("实施计划");
    await expect(page.locator(".permission-warning")).toHaveCount(0);

    for (const width of [1280, 360]) {
      await page.setViewportSize({ width, height: 900 });
      await scenario.selectOption("permission-1");
      await expect(page.locator(".permission-subject .raw-json")).toContainText("/workspace/config.ts");
      await expect(page.locator(".permission-content .terminal-embed")).toContainText("你好，世界");
      await page.locator(".permission-content .diff-card > summary").click();
      await expect(page.locator(".permission-warning")).toHaveCount(0);
      await expectNoHorizontalOverflow(page);
      await page.screenshot({ path: testInfo.outputPath(`approval-${width}.png`) });

      await scenario.selectOption("settings");
      await page.locator('[aria-controls="config-options-showcase-model"]').click();
      await expectConfigMenuPosition(page, "showcase-model", width);
      await expect(page.getByRole("group", { name: "服务商 A" })).toBeVisible();
      await expect(page.getByRole("group", { name: "服务商 B" })).toBeVisible();
      await page.locator(".config-search input").fill("服务商 B");
      await expect(page.locator('.config-option-list [role="option"]')).toHaveCount(4);
      await expectNoHorizontalOverflow(page);
      await page.screenshot({ path: testInfo.outputPath(`grouped-options-${width}.png`) });
      await page.locator('.config-option-list [role="option"]').first().click();
      await expect(page.locator('[aria-controls="config-options-showcase-model"]')).toContainText("服务商 B · Fast");
      await page.locator('[aria-controls="config-options-legacy-mode"]').click();
      await expectConfigMenuPosition(page, "legacy-mode", width);
      await expect(page.locator('.config-option-list [role="option"]').first()).toContainText("修改文件前请求许可");
      await page.keyboard.press("Escape");
      if (width > 760) {
        // Exercise the same control near the viewport edge and during resize.
        await page.locator(".showcase-settings").evaluate((element) => {
          Object.assign(element.style, { position: "fixed", right: "16px", top: "300px" });
        });
        await page.locator('[aria-controls="config-options-legacy-mode"]').click();
        for (const edgeWidth of [width, 900]) {
          await page.setViewportSize({ width: edgeWidth, height: 900 });
          await expect.poll(async () => {
            const menu = await page.locator(".config-popover").boundingBox();
            return menu!.x + menu!.width;
          }).toBeLessThanOrEqual(edgeWidth - 8);
        }
        await page.keyboard.press("Escape");
        await page.setViewportSize({ width, height: 900 });
      }
      await expect(page.locator(".context-usage-trigger")).toBeVisible();

      await scenario.selectOption("form-0");
      await expect(page.locator(".elicitation-fields")).toContainText("每次修改前展示计划");
      await page.locator(".elicitation-fields select").selectOption("batch");
      await expect(page.locator(".elicitation-fields")).toContainText("一次执行已批准的修改");
      await expect(page.locator(".check-choice").first()).toContainText("验证组件行为和状态变化");
      await expectNoHorizontalOverflow(page);
      await page.screenshot({ path: testInfo.outputPath(`form-descriptions-${width}.png`) });
    }

    await scenario.selectOption("cancellation");
    await expect(page.locator('[data-tool-status="in_progress"]')).toHaveCount(1);
    await page.getByRole("button", { name: "Stop current turn", exact: true }).click();
    await expect(page.getByRole("button", { name: "Cancelling the current turn…", exact: true })).toBeDisabled();
    await expect(page.locator('[data-tool-status="cancelled"]')).toHaveCount(2);
    await page.getByRole("button", { name: "模拟工具返回结果" }).click();
    await expect(page.locator('[data-tool-status="completed"]')).toHaveCount(2);
    await expect(page.locator('[data-tool-status="cancelled"]')).toHaveCount(1);
    await page.getByRole("button", { name: "恢复运行" }).click();
    await expect(page.getByRole("button", { name: "Stop current turn", exact: true })).toBeEnabled();
    await expect(page.locator('[data-tool-status="cancelled"]')).toHaveCount(0);
    await expectNoHorizontalOverflow(page);
    expect(apiRequests).toEqual([]);
    expect(browserErrors).toEqual([]);
  } finally {
    await vite.close();
  }
});

async function expectNoHorizontalOverflow(page: import("@playwright/test").Page) {
  expect(await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth)).toBeLessThanOrEqual(1);
}

async function expectConfigMenuPosition(page: import("@playwright/test").Page, id: string, viewportWidth: number) {
  const trigger = await page.locator(`[aria-controls="config-options-${id}"]`).boundingBox();
  const menu = await page.locator(".config-popover").boundingBox();
  expect(trigger).not.toBeNull();
  expect(menu).not.toBeNull();
  expect(menu!.x).toBeGreaterThanOrEqual(0);
  expect(menu!.x + menu!.width).toBeLessThanOrEqual(viewportWidth);
  if (viewportWidth > 760) expect(Math.abs(menu!.x - trigger!.x)).toBeLessThanOrEqual(1);
}
