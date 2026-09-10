import { expect, test as baseTest, type Locator, type Page } from "@playwright/test";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { startRustTestServer } from "../../scripts/rust-test-server";

interface CollapseFixture {
  url: string;
  advance(stage: "append" | "finish"): Promise<void>;
}

const test = baseTest.extend<{ collapseFixture: CollapseFixture }>({
  collapseFixture: async ({}, use) => {
    const directory = await mkdtemp(join(tmpdir(), "attyd-turn-collapse-"));
    const gate = join(directory, "stage");
    const server = await startRustTestServer({
      command: [process.execPath, "--import", "tsx", join(process.cwd(), "tests/fixtures/fake-agent.ts")],
      env: { ...process.env, ATTYD_FAKE_TURN_COLLAPSE_GATE: gate },
    });
    try {
      await use({
        url: `http://127.0.0.1:${server.port}`,
        advance: (stage) => writeFile(`${gate}.${stage}`, "ready\n"),
      });
    } finally {
      await server.close();
      await rm(directory, { recursive: true, force: true });
    }
  },
  baseURL: async ({ collapseFixture }, use) => use(collapseFixture.url),
});

test("shows the live process and keeps only the final answer after completion at the bottom", async ({ page, collapseFixture }, testInfo) => {
  await page.setViewportSize({ width: 900, height: 420 });
  const { turn, thread } = await startTurn(page);
  await expect(turn.locator(".message-content").getByText("Process paragraph 20:", { exact: false })).toBeVisible();
  await expect(turn.locator(".thinking-block")).toBeVisible();
  await expect(turn.getByText("Inspect collapse fixture", { exact: true })).toBeVisible();
  await expect(turn.getByRole("button", { name: "Expand execution process", exact: true })).toHaveCount(0);
  await expectBottom(thread);

  await collapseFixture.advance("append");
  await expect(turn.getByText("The final answer is ready.", { exact: true })).toBeVisible();
  await expect(page.getByRole("button", { name: "Stop current turn", exact: true })).toBeVisible();
  await expect(turn.locator(".message-content").getByText("Process paragraph 20:", { exact: false })).toBeVisible();
  await expectBottom(thread);

  await collapseFixture.advance("finish");
  await expectFolded(turn);
  await expect(turn.locator('[data-stop-reason="end_turn"]')).toBeVisible();
  await expect(turn.locator(".message-user")).toContainText("turn-collapse-flow");
  await expectBottom(thread);
  await page.setViewportSize({ width: 1280, height: 900 });
  await page.screenshot({ path: testInfo.outputPath("turn-process-desktop-collapsed.png"), animations: "disabled" });
});

for (const distance of [24, 500]) {
  test(`preserves the reading position ${distance}px above the bottom until the user returns`, async ({ page, collapseFixture }) => {
    await page.setViewportSize({ width: 900, height: 420 });
    const { turn, thread } = await startTurn(page);
    await expectBottom(thread);
    await thread.evaluate((element, offset) => {
      element.dispatchEvent(new WheelEvent("wheel", { bubbles: true, deltaY: -offset }));
      element.scrollTop = element.scrollHeight - element.clientHeight - offset;
    }, distance);
    const readingPosition = await thread.evaluate((element) => element.scrollTop);
    const anchor = turn.locator(".message-content").getByText("Process paragraph 18:", { exact: false });
    const anchorTop = await anchor.evaluate((element) => element.getBoundingClientRect().top);

    await collapseFixture.advance("append");
    await expect(turn.getByText("The final answer is ready.", { exact: true })).toBeVisible();
    expect(await thread.evaluate((element) => element.scrollTop)).toBe(readingPosition);
    expect(await anchor.evaluate((element) => element.getBoundingClientRect().top)).toBe(anchorTop);
    await collapseFixture.advance("finish");
    await expect(page.getByRole("button", { name: "Send prompt", exact: true })).toBeVisible();
    await expect(turn.locator('[data-stop-reason="end_turn"]')).toBeVisible();
    await expect(turn.locator(".turn-process-content")).not.toHaveAttribute("hidden", "");
    expect(await thread.evaluate((element) => element.scrollTop)).toBe(readingPosition);
    expect(await anchor.evaluate((element) => element.getBoundingClientRect().top)).toBe(anchorTop);

    await page.getByRole("button", { name: "Jump to bottom of thread", exact: true }).click();
    await expectFolded(turn);
    await expectBottom(thread);
  });
}

test("can expand the process for search while final response copying stays focused", async ({ page, context, collapseFixture }) => {
  await context.grantPermissions(["clipboard-read", "clipboard-write"]);
  const { turn } = await startTurn(page);
  await collapseFixture.advance("append");
  await expect(turn.getByText("The final answer is ready.", { exact: true })).toBeVisible();
  await collapseFixture.advance("finish");
  await expectFolded(turn);
  await turn.getByRole("button", { name: "Copy agent response", exact: true }).click();
  await expect.poll(() => page.evaluate(() => navigator.clipboard.readText())).toBe("The final answer is ready.");

  await page.getByRole("button", { name: "Search Agent thread", exact: true }).click();
  const query = page.getByRole("searchbox", { name: "Search this thread", exact: true });
  await query.fill("Process paragraph 18:");
  const counter = page.locator(".thread-search-navigation output");
  await expect(counter).toHaveText("0/0");
  const expand = turn.getByRole("button", { name: "Expand execution process", exact: true });
  await expand.focus();
  await expand.press("Enter");
  await expect(turn.locator(".turn-process-content")).not.toHaveAttribute("hidden", "");
  await expect(counter).toHaveText("1/1");
  await expect(turn.locator('[data-thread-search-active="true"]')).toBeInViewport();
  await expect(turn.getByText("Inspect collapse fixture", { exact: true })).toBeVisible();
  await query.press("Escape");
  const collapse = turn.getByRole("button", { name: "Collapse execution process", exact: true });
  await collapse.focus();
  await collapse.press("Enter");
  await expectFolded(turn);
  await expect(turn.getByRole("button", { name: "Expand execution process", exact: true })).toBeFocused();
});

test("keeps completed history folded after reload and fits mobile in both themes", async ({ page, collapseFixture }, testInfo) => {
  await page.setViewportSize({ width: 390, height: 844 });
  let { turn } = await startTurn(page);
  await collapseFixture.advance("append");
  await expect(turn.getByText("The final answer is ready.", { exact: true })).toBeVisible();
  await collapseFixture.advance("finish");
  await expectFolded(turn);
  await turn.getByRole("button", { name: "Expand execution process", exact: true }).click();
  await expect(turn.locator(".message-content").getByText("Process paragraph 20:", { exact: false })).toBeVisible();
  await page.reload();
  await expect(page.locator('textarea[role="combobox"]')).toBeEnabled();
  turn = page.locator(".conversation-turn").filter({ has: page.locator(".message-user", { hasText: "turn-collapse-flow" }) });
  await expectFolded(turn);

  for (const theme of ["light", "dark"]) {
    await page.getByRole("button", { name: "Interface settings", exact: true }).click();
    await page.getByRole("combobox", { name: "Appearance", exact: true }).selectOption(theme);
    await page.keyboard.press("Escape");
    for (const expanded of [false, true]) {
      if (expanded) await turn.getByRole("button", { name: "Expand execution process", exact: true }).click();
      expect(await page.evaluate(() => Math.max(
        document.documentElement.scrollWidth - window.innerWidth,
        document.body.scrollWidth - window.innerWidth,
      ))).toBeLessThanOrEqual(1);
      const process = turn.locator(".turn-process");
      const bounds = await process.boundingBox();
      expect(bounds).not.toBeNull();
      expect(bounds!.x).toBeGreaterThanOrEqual(0);
      expect(bounds!.x + bounds!.width).toBeLessThanOrEqual(391);
      await page.screenshot({ path: testInfo.outputPath(`turn-process-mobile-${theme}-${expanded ? "expanded" : "collapsed"}.png`), animations: "disabled" });
      if (expanded) await turn.getByRole("button", { name: "Collapse execution process", exact: true }).click();
    }
  }
  await page.getByRole("button", { name: "Interface settings", exact: true }).click();
  await page.getByRole("combobox", { name: "Language", exact: true }).selectOption("zh-CN");
  await page.keyboard.press("Escape");
  await expect(turn.getByRole("button", { name: "展开执行过程", exact: true }).locator("strong")).toHaveText("执行过程");
  await page.screenshot({ path: testInfo.outputPath("turn-process-mobile-chinese.png"), animations: "disabled" });
});

async function startTurn(page: Page) {
  await page.goto("/sessions/saved-session");
  const composer = page.locator('textarea[role="combobox"]');
  await expect(composer).toBeEnabled();
  await composer.fill("turn-collapse-flow");
  await composer.press("Enter");
  const turn = page.locator(".conversation-turn").filter({ has: page.locator(".message-user", { hasText: "turn-collapse-flow" }) });
  await expect(turn.getByText("Inspect collapse fixture", { exact: true })).toBeVisible();
  return { turn, thread: page.getByRole("region", { name: "Conversation thread", exact: true }) };
}

async function expectFolded(turn: Locator) {
  const button = turn.getByRole("button", { name: "Expand execution process", exact: true });
  await expect(button).toHaveAttribute("aria-expanded", "false");
  await expect(turn.locator(".turn-process-content")).toHaveAttribute("hidden", "");
  await expect(turn.locator(".message-content").getByText("Process paragraph 20:", { exact: false })).toBeHidden();
  await expect(turn.getByText("Inspect collapse fixture", { exact: true })).toBeHidden();
  await expect(turn.getByText("The final answer is ready.", { exact: true })).toBeVisible();
}

async function expectBottom(thread: Locator) {
  await expect.poll(() => thread.evaluate((element) =>
    element.scrollHeight - element.clientHeight - element.scrollTop
  )).toBeLessThan(3);
}
