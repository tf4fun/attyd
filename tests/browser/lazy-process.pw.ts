import { expect, test as baseTest, type Locator, type Page } from "@playwright/test";
import { join } from "node:path";
import { startRustTestServer } from "../../scripts/rust-test-server";

const SESSION_PATH = "/api/v1/sessions/lazy-process-session";
const FINAL_ANSWER = "The final answer is available without loading execution details.";
const test = baseTest.extend<{ fixtureUrl: string }>({
  fixtureUrl: async ({}, use) => {
    const server = await startRustTestServer({
      command: [process.execPath, "--import", "tsx", join(process.cwd(), "tests/fixtures/lazy-process-agent.ts")],
    });
    try {
      await use(`http://127.0.0.1:${server.port}`);
    } finally {
      await server.close();
    }
  },
  baseURL: async ({ fixtureUrl }, use) => use(fixtureUrl),
});

test("transmits the final answer first and fetches execution details only in requested pages of ten", async ({ page }) => {
  const processRequests: string[] = [];
  page.on("request", (request) => {
    if (isProcessUrl(request.url())) processRequests.push(request.url());
  });
  const initialResponse = page.waitForResponse((response) => {
    const url = new URL(response.url());
    return url.pathname === SESSION_PATH && url.searchParams.get("presentation") === "compact" && response.ok();
  });
  await page.goto("/sessions/lazy-process-session");
  const response = await initialResponse;
  const raw = await response.text();
  expect(raw).toContain(FINAL_ANSWER);
  expect(raw).not.toContain("HIDDEN_PROCESS_PAYLOAD_");
  expect(raw).not.toContain("Inspect process item");
  const snapshot = JSON.parse(raw);
  expect(snapshot.collapsedTurns).toHaveLength(1);
  expect(snapshot.collapsedTurns[0].processCount).toBe(25);
  const turn = page.locator(".conversation-turn").filter({ hasText: FINAL_ANSWER });
  await expect(turn.getByText(FINAL_ANSWER, { exact: true })).toBeVisible();
  await expect(turn.locator(".turn-process-count")).toHaveText("25 items");
  expect(processRequests).toEqual([]);
  await expect(turn.locator(".tool-card")).toHaveCount(0);

  const firstPage = await requestPage(page, () => turn.getByRole("button", { name: "Expand execution process", exact: true }).click());
  expect(firstPage).toMatchObject({ offset: 0, total: 25, nextOffset: 10 });
  expect(firstPage.items).toHaveLength(10);
  await expect(turn.locator(".tool-card")).toHaveCount(10);
  await expect(turn.getByText("Inspect process item 01", { exact: true })).toBeVisible();
  await expect(turn.getByText("Inspect process item 11", { exact: true })).toHaveCount(0);
  await expect(turn.getByText("10 of 25 loaded", { exact: true })).toBeVisible();
  await expectToolSpacing(turn.locator(".tool-card").nth(0), turn.locator(".tool-card").nth(1));
  expect(processRequests.map((url) => new URL(url).searchParams.get("offset"))).toEqual(["0"]);

  const secondPage = await requestPage(page, () => turn.getByRole("button", { name: "Load 10 more", exact: true }).click());
  expect(secondPage).toMatchObject({ offset: 10, total: 25, nextOffset: 20 });
  expect(secondPage.items).toHaveLength(10);
  await expect(turn.locator(".tool-card")).toHaveCount(20);
  await expect(turn.getByText("Inspect process item 21", { exact: true })).toHaveCount(0);
  await expectToolSpacing(turn.locator(".tool-card").nth(9), turn.locator(".tool-card").nth(10));

  const finalPage = await requestPage(page, () => turn.getByRole("button", { name: "Load 10 more", exact: true }).click());
  expect(finalPage).toMatchObject({ offset: 20, total: 25, nextOffset: null });
  expect(finalPage.items).toHaveLength(5);
  await expect(turn.locator(".tool-card")).toHaveCount(25);
  await expect(turn.getByText("25 of 25 loaded", { exact: true })).toBeVisible();
  await expect(turn.getByRole("button", { name: "Load 10 more", exact: true })).toHaveCount(0);
  expect(processRequests.map((url) => new URL(url).searchParams.get("offset"))).toEqual(["0", "10", "20"]);

  await turn.getByRole("button", { name: "Collapse execution process", exact: true }).click();
  await turn.getByRole("button", { name: "Expand execution process", exact: true }).click();
  await expect(turn.locator(".tool-card")).toHaveCount(25);
  expect(processRequests).toHaveLength(3);
  await expect(turn.getByText(FINAL_ANSWER, { exact: true })).toBeVisible();
});

test("releases pages after five minutes folded and refetches only the first ten entries on reopen", async ({ page }) => {
  await page.clock.install({ time: new Date("2026-01-01T00:00:00Z") });
  const processRequests: string[] = [];
  page.on("request", (request) => {
    if (isProcessUrl(request.url())) processRequests.push(request.url());
  });
  await page.goto("/sessions/lazy-process-session");
  const turn = page.locator(".conversation-turn").filter({ hasText: FINAL_ANSWER });
  await expect(turn.getByText(FINAL_ANSWER, { exact: true })).toBeVisible();
  await requestPage(page, () => turn.getByRole("button", { name: "Expand execution process", exact: true }).click());
  await requestPage(page, () => turn.getByRole("button", { name: "Load 10 more", exact: true }).click());
  await expect(turn.locator(".tool-card")).toHaveCount(20);
  await page.clock.pauseAt(new Date("2026-01-01T00:01:00Z"));
  await turn.getByRole("button", { name: "Collapse execution process", exact: true }).click();

  await page.clock.fastForward("04:59");
  await expect(turn.locator(".turn-process-content")).toBeHidden();
  await expect(turn.locator(".tool-card")).toHaveCount(20);
  await page.clock.fastForward("00:01");
  await expect(turn.locator(".tool-card")).toHaveCount(0);
  await expect(turn.locator(".turn-process-count")).toHaveText("25 items");
  await expect(turn.getByText(FINAL_ANSWER, { exact: true })).toBeVisible();
  expect(processRequests.map((url) => new URL(url).searchParams.get("offset"))).toEqual(["0", "10"]);

  const reloaded = await requestPage(page, () => turn.getByRole("button", { name: "Expand execution process", exact: true }).click());
  expect(reloaded).toMatchObject({ offset: 0, total: 25, nextOffset: 10 });
  expect(reloaded.items).toHaveLength(10);
  await expect(turn.locator(".tool-card")).toHaveCount(10);
  await expect(turn.getByText("10 of 25 loaded", { exact: true })).toBeVisible();
  await expect(turn.getByText("Inspect process item 11", { exact: true })).toHaveCount(0);
  expect(processRequests.map((url) => new URL(url).searchParams.get("offset"))).toEqual(["0", "10", "0"]);
});

for (const language of ["en", "zh-CN"] as const) {
  const copy = language === "en" ? {
    expand: "Expand execution process",
    loadMore: "Load 10 more",
    loading: "Loading execution details…",
    loaded: "10 of 25 loaded",
    timeout: "Loading execution details timed out. Retry when ready.",
    retry: "Retry loading",
  } : {
    expand: "展开执行过程",
    loadMore: "再加载 10 条",
    loading: "正在加载执行过程…",
    loaded: "已加载 10 / 25 条",
    timeout: "执行过程加载超时，请点击重试。",
    retry: "重试加载",
  };
  for (const theme of ["light", "dark"] as const) {
    test(`keeps the mobile process footer readable through loading, timeout and manual retry (${language}, ${theme})`, async ({ page }, testInfo) => {
      await page.setViewportSize({ width: 390, height: 844 });
      await page.addInitScript(({ language, theme }) => {
        localStorage.setItem("attyd.language", language);
        localStorage.setItem("attyd.theme", theme);
      }, { language, theme });
      await page.clock.install({ time: new Date("2026-01-01T00:00:00Z") });
      const processRequests: string[] = [];
      page.on("request", (request) => {
        if (isProcessUrl(request.url())) processRequests.push(request.url());
      });
      await page.goto("/sessions/lazy-process-session");
      await expect(page.locator("html")).toHaveAttribute("data-theme", theme);
      const turn = page.locator(".conversation-turn").filter({ hasText: FINAL_ANSWER });
      await expect(turn.getByText(FINAL_ANSWER, { exact: true })).toBeVisible();
      await requestPage(page, () => turn.getByRole("button", { name: copy.expand, exact: true }).click());
      await expect(turn.locator(".tool-card")).toHaveCount(10);
      await expect(turn.getByText(copy.loaded, { exact: true })).toBeVisible();
      await expectMobileFooterLayout(page, turn);
      await page.screenshot({ path: testInfo.outputPath("process-footer-ready.png"), animations: "disabled" });
      await page.clock.pauseAt(new Date("2026-01-01T00:01:00Z"));
      // Hold a real browser fetch before headers. Subsequent requests pass through.
      let holdNext = true;
      await page.route((url) => isProcessUrl(url.href), async (route) => {
        if (holdNext) { holdNext = false; return; }
        await route.continue();
      });
      const stalled = page.waitForRequest((request) => isProcessUrl(request.url()));
      await turn.getByRole("button", { name: copy.loadMore, exact: true }).click();
      await stalled;
      await expect(turn.getByRole("status")).toHaveText(copy.loading);
      await expect(turn.getByRole("button", { name: copy.loadMore, exact: true })).toBeDisabled();
      await expectMobileFooterLayout(page, turn);
      await page.screenshot({ path: testInfo.outputPath("process-footer-loading.png"), animations: "disabled" });
      await page.clock.fastForward("00:30");
      await expect(turn.getByRole("alert")).toHaveText(copy.timeout);
      await expect(turn.locator(".tool-card")).toHaveCount(10);
      await expect(turn.getByText(FINAL_ANSWER, { exact: true })).toBeVisible();
      await page.clock.fastForward("01:00");
      expect(processRequests.map((url) => new URL(url).searchParams.get("offset"))).toEqual(["0", "10"]);
      await expectMobileFooterLayout(page, turn);
      await page.screenshot({ path: testInfo.outputPath("process-footer-timeout.png"), animations: "disabled" });
      const retried = await requestPage(page, () => turn.getByRole("button", { name: copy.retry, exact: true }).click());
      expect(retried).toMatchObject({ offset: 10, nextOffset: 20 });
      await expect(turn.locator(".tool-card")).toHaveCount(20);
      await expect(turn.getByRole("alert")).toHaveCount(0);
      expect(processRequests.map((url) => new URL(url).searchParams.get("offset"))).toEqual(["0", "10", "10"]);
    });
  }
}

test("keeps task progress on the left and recovers composer space when the mobile plan is collapsed", async ({ page }, testInfo) => {
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto("/sessions/lazy-process-session");
  const composer = page.locator('textarea[role="combobox"]');
  await expect(composer).toBeEnabled();
  await composer.fill("Show the live task plan");
  await composer.press("Enter");
  const plan = page.locator(".input-inner > .plan-card");
  await expect(plan.locator("li")).toHaveCount(5);
  await expect(plan.locator(".plan-progress")).toHaveText("1/5");
  const before = await plan.boundingBox();
  expect(before).not.toBeNull();
  const title = await plan.locator(".plan-disclosure strong").boundingBox();
  const count = await plan.locator(".plan-progress").boundingBox();
  const button = plan.getByRole("button", { name: "Collapse plan details", exact: true });
  const toggle = await button.boundingBox();
  expect(title).not.toBeNull();
  expect(count).not.toBeNull();
  expect(toggle).not.toBeNull();
  expect(count!.x).toBeGreaterThan(title!.x + title!.width);
  expect(count!.x - (title!.x + title!.width)).toBeLessThan(12);
  expect(toggle!.x).toBeGreaterThan(count!.x + count!.width);
  expect(toggle!.x + toggle!.width).toBeLessThanOrEqual(390);
  await page.screenshot({ path: testInfo.outputPath("mobile-plan-expanded.png"), animations: "disabled" });

  await button.click();
  await expect(plan.locator(".plan-body")).toBeHidden();
  await expect(plan.getByRole("button", { name: "Expand plan details", exact: true })).toHaveAttribute("aria-expanded", "false");
  await expect(plan.locator(".plan-progress")).toBeVisible();
  const after = await plan.boundingBox();
  expect(after).not.toBeNull();
  expect(before!.height - after!.height).toBeGreaterThan(100);
  expect(await page.evaluate(() => Math.max(
    document.documentElement.scrollWidth - innerWidth,
    document.body.scrollWidth - innerWidth,
  ))).toBeLessThanOrEqual(1);
  await expect(composer).toBeInViewport();
  await page.screenshot({ path: testInfo.outputPath("mobile-plan-collapsed.png"), animations: "disabled" });
  await plan.getByRole("button", { name: "Expand plan details", exact: true }).click();
  await expect(plan.locator(".plan-body")).toBeVisible();
  await page.getByRole("button", { name: "Stop current turn", exact: true }).click();
});

function isProcessUrl(url: string): boolean {
  const path = new URL(url).pathname;
  return path.startsWith(`${SESSION_PATH}/turns/`) && path.endsWith("/process");
}

async function expectToolSpacing(before: Locator, after: Locator) {
  const first = await before.boundingBox();
  const second = await after.boundingBox();
  expect(first).not.toBeNull();
  expect(second).not.toBeNull();
  expect(second!.y - first!.y - first!.height).toBeCloseTo(18, 0);
  expect(second!.x).toBeCloseTo(first!.x, 0);
  expect(second!.width).toBeCloseTo(first!.width, 0);
}

async function expectMobileFooterLayout(page: Page, turn: Locator) {
  const footer = turn.locator(".turn-process-pagination");
  await footer.scrollIntoViewIfNeeded();
  await expect(footer).toBeInViewport();
  const layout = await footer.evaluate((element) => {
    const { x, y, width, height } = element.getBoundingClientRect();
    return {
      bounds: { x, y, width, height },
      overflow: element.scrollWidth - element.clientWidth,
      items: Array.from(element.querySelectorAll("button, [role=status], [role=alert], .turn-process-loaded"))
        .map((item) => {
          const { x, y, width, height } = item.getBoundingClientRect();
          return { text: item.textContent, x, y, width, height };
        }),
    };
  });
  expect(layout.overflow).toBeLessThanOrEqual(1);
  for (const item of layout.items) {
    expect(item.width, item.text ?? "footer item").toBeGreaterThan(0);
    expect(item.x).toBeGreaterThanOrEqual(layout.bounds.x - 1);
    expect(item.x + item.width).toBeLessThanOrEqual(layout.bounds.x + layout.bounds.width + 1);
    expect(item.y).toBeGreaterThanOrEqual(layout.bounds.y - 1);
    expect(item.y + item.height).toBeLessThanOrEqual(layout.bounds.y + layout.bounds.height + 1);
    expect(item.x).toBeGreaterThanOrEqual(0);
    expect(item.x + item.width).toBeLessThanOrEqual(390);
  }
  for (let index = 0; index < layout.items.length; index++) {
    const first = layout.items[index];
    for (const second of layout.items.slice(index + 1)) {
      const overlapWidth = Math.min(first.x + first.width, second.x + second.width) - Math.max(first.x, second.x);
      const overlapHeight = Math.min(first.y + first.height, second.y + second.height) - Math.max(first.y, second.y);
      expect(overlapWidth > 1 && overlapHeight > 1, `${first.text} overlaps ${second.text}`).toBe(false);
    }
  }
  expect(await page.evaluate(() => Math.max(
    document.documentElement.scrollWidth - innerWidth,
    document.body.scrollWidth - innerWidth,
  ))).toBeLessThanOrEqual(1);
}

async function requestPage(page: Page, trigger: () => Promise<void>) {
  const response = page.waitForResponse((candidate) => isProcessUrl(candidate.url()));
  await trigger();
  const result = await response;
  expect(result.ok()).toBe(true);
  return result.json();
}
