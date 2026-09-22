import { expect, test as baseTest } from "@playwright/test";
import { join } from "node:path";
import { startRustTestServer } from "../../scripts/rust-test-server";

const test = baseTest.extend<{ fixtureUrl: string }>({
  fixtureUrl: async ({}, use) => {
    const server = await startRustTestServer({
      command: [process.execPath, "--import", "tsx", join(process.cwd(), "tests/fixtures/lazy-process-agent.ts")],
    });
    try { await use(`http://127.0.0.1:${server.port}`); }
    finally { await server.close(); }
  },
  baseURL: async ({ fixtureUrl }, use) => use(fixtureUrl),
});

for (const theme of ["light", "dark"] as const) {
  for (const width of [390, 1280]) {
    test(`keeps skeleton and conversation columns aligned (${theme}, ${width})`, async ({ page }, testInfo) => {
      await page.setViewportSize({ width, height: 844 });
      if (theme === "dark") await page.emulateMedia({ reducedMotion: "reduce" });
      await page.addInitScript((theme) => {
        localStorage.setItem("attyd.language", "zh-CN");
        localStorage.setItem("attyd.theme", theme);
      }, theme);
      let release!: () => void;
      const gate = new Promise<void>((resolve) => { release = resolve; });
      await page.route("**/api/v1/sessions/lazy-process-session?*", async (route) => {
        await gate;
        await route.continue();
      });
      await page.goto("/sessions/lazy-process-session");
      const opening = page.locator(".session-opening-panel");
      await expect(opening.getByRole("status")).toContainText("正在加载会话");
      await expect(opening.locator('[aria-busy="true"]')).toBeVisible();
      await expect(page.locator('textarea[role="combobox"]')).toHaveCount(0);
      await expect(page.locator(".project-browser")).toHaveCount(0);
      await expect(opening.getByRole("button")).toHaveCount(1);
      await expect(opening.locator('.session-skeleton[aria-hidden="true"]')).toBeVisible();
      if (theme === "dark") expect(await opening.locator(".skeleton").first().evaluate((element) =>
        getComputedStyle(element).animationName)).toBe("none");
      const columns = [".session-header", ".conversation-wrap", ".input-inner"];
      const before = await Promise.all(columns.map((selector) => opening.locator(selector).boundingBox()));
      expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBeLessThanOrEqual(width);
      await page.screenshot({ path: testInfo.outputPath(`session-loading-${theme}.png`), animations: "disabled" });
      release();
      await expect(page.getByText("The final answer is available without loading execution details.", { exact: true })).toBeVisible();
      await expect(opening).toHaveCount(0);
      await expect(page.locator('textarea[role="combobox"]')).toBeEnabled();
      for (const [index, selector] of columns.entries()) {
        const after = await page.locator(selector).boundingBox();
        expect(before[index]).not.toBeNull();
        expect(after).not.toBeNull();
        expect(after!.x).toBeCloseTo(before[index]!.x, 0);
        expect(after!.width).toBeCloseTo(before[index]!.width, 0);
      }
    });
  }
}

test("replaces loading with a timeout message and manually retries the session read", async ({ page }) => {
  let fail = true;
  let reads = 0;
  await page.route("**/api/v1/sessions/lazy-process-session?*", async (route) => {
    reads += 1;
    if (fail) {
      await route.fulfill({ status: 504, contentType: "application/json", body: JSON.stringify({
        code: "request_timeout", timeoutMs: 30000,
      }) });
    } else await route.continue();
  });
  await page.goto("/sessions/lazy-process-session");
  const opening = page.locator(".session-opening-panel");
  await expect(opening.getByRole("alert")).toContainText("timed out");
  await expect(opening.locator('[aria-busy="false"]')).toBeVisible();
  await expect(opening.getByRole("status")).toHaveCount(0);
  await expect(opening.locator(".skeleton")).toHaveCount(0);
  const readsBeforeRetry = reads;
  fail = false;
  await opening.getByRole("button", { name: "Retry loading", exact: true }).click();
  await expect(page.getByText("The final answer is available without loading execution details.", { exact: true })).toBeVisible();
  expect(reads).toBeGreaterThan(readsBeforeRetry);
  await expect(opening).toHaveCount(0);
});
