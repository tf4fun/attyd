import { expect, test as baseTest, type Locator, type Page } from "@playwright/test";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { startRustTestServer } from "../../scripts/rust-test-server";
import { trackApi } from "./api-requests";

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

// `turn-collapse-flow` streams a message of twenty "Process paragraph"s, a thought and a tool call,
// then the final answer as a message of its own. The commit shows the final answer and folds
// everything before it (the first message, the thought, the tool call) behind the disclosure,
// which keeps it in the thread; no process page exists to fetch it. While the user is reading
// above the bottom, the process stays open and folds when they return.

test("shows the live process, and after completion at the bottom the final answer with the process folded", async ({ page, collapseFixture }, testInfo) => {
  await page.setViewportSize({ width: 900, height: 420 });
  const api = trackApi(page);
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
  await expectCommitted(turn);
  await expect(turn.locator(".message-user")).toContainText("turn-collapse-flow");
  await expectBottom(thread);
  expect(api.streams.length).toBeGreaterThan(0);
  expect(api.process).toEqual([]);
  expect(api.v1).toEqual([]);
  await page.setViewportSize({ width: 1280, height: 900 });
  await page.screenshot({ path: testInfo.outputPath("turn-process-desktop-collapsed.png"), animations: "disabled" });
});

test("keeps the committed reply after five idle minutes without fetching process pages", async ({ page, collapseFixture }) => {
  await page.clock.install({ time: new Date("2026-01-01T00:00:00Z") });
  const api = trackApi(page);
  const { turn } = await startTurn(page);
  // Process layout is checked while the turn still runs; the commit folds it away.
  const running = await processGeometry(turn);
  expect(running.agentThoughtGap).toBeCloseTo(9, 0);
  expect(running.thoughtToolGap).toBeCloseTo(18, 0);
  await collapseFixture.advance("append");
  await expect(turn.getByText("The final answer is ready.", { exact: true })).toBeVisible();
  await collapseFixture.advance("finish");
  await expectCommitted(turn);
  await page.clock.fastForward("05:00");
  await expectCommitted(turn);
  expect(api.streams.length).toBeGreaterThan(0);
  expect(api.process).toEqual([]);
  expect(api.v1).toEqual([]);
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
    // The user is reading above the bottom: the process is not folded away under them.
    await expectCommittedUnfolded(turn);
    // The range does not change while it stays open; if it did, the browser would clamp.
    // Within the remaining range the offset stays put.
    const maximum = await thread.evaluate((element) => element.scrollHeight - element.clientHeight);
    const expectedPosition = Math.min(readingPosition, maximum);
    expect(await thread.evaluate((element) => element.scrollTop)).toBe(expectedPosition);
    expect(await anchor.evaluate((element) => element.getBoundingClientRect().top))
      .toBe(anchorTop + readingPosition - expectedPosition);

    if (expectedPosition < maximum) {
      await page.getByRole("button", { name: "Jump to bottom of thread", exact: true }).click();
    }
    await expectBottom(thread);
    // Back at the bottom, the turn folds its process.
    await expectCommitted(turn);
  });
}

test("copies the final answer, and searches the process only once it is expanded", async ({ page, context, collapseFixture }) => {
  await context.grantPermissions(["clipboard-read", "clipboard-write"]);
  const { turn } = await startTurn(page);
  await collapseFixture.advance("append");
  await expect(turn.getByText("The final answer is ready.", { exact: true })).toBeVisible();
  await collapseFixture.advance("finish");
  await expectCommitted(turn);
  const copies = turn.getByRole("button", { name: "Copy agent response", exact: true });
  await copies.last().click();
  await expect.poll(() => page.evaluate(() => navigator.clipboard.readText())).toContain("The final answer is ready.");
  expect(await page.evaluate(() => navigator.clipboard.readText())).not.toContain("Process paragraph 18:");

  // Search indexes what is visible: the folded process is not searched until it is expanded.
  await page.getByRole("button", { name: "Search Agent thread", exact: true }).click();
  const query = page.getByRole("searchbox", { name: "Search this thread", exact: true });
  const counter = page.locator(".thread-search-navigation output");
  await query.fill("Process paragraph 18:");
  await expect(counter).toHaveText("0/0");
  await turn.getByRole("button", { name: "Expand execution process", exact: true }).click();
  await expect(counter).toHaveText("1/1");
  await expect(turn.locator('[data-thread-search-active="true"]')).toBeInViewport();
  await query.fill("Inspect collapse fixture");
  await expect(counter).toHaveText("1/1");
  await query.press("Escape");
});

test("keeps the committed reply after reload and fits mobile in both themes", async ({ page, collapseFixture }, testInfo) => {
  await page.setViewportSize({ width: 390, height: 844 });
  let { turn } = await startTurn(page);
  await collapseFixture.advance("append");
  await expect(turn.getByText("The final answer is ready.", { exact: true })).toBeVisible();
  await collapseFixture.advance("finish");
  await expectCommitted(turn);
  await page.reload();
  await expect(page.locator('textarea[role="combobox"]')).toBeEnabled();
  turn = page.locator(".conversation-turn").filter({ has: page.locator(".message-user", { hasText: "turn-collapse-flow" }) });
  await expectCommitted(turn);

  for (const theme of ["light", "dark"]) {
    await page.getByRole("button", { name: "Interface settings", exact: true }).click();
    await page.getByRole("combobox", { name: "Appearance", exact: true }).selectOption(theme);
    await page.keyboard.press("Escape");
    expect(await page.evaluate(() => Math.max(
      document.documentElement.scrollWidth - window.innerWidth,
      document.body.scrollWidth - window.innerWidth,
    ))).toBeLessThanOrEqual(1);
    const bounds = await turn.boundingBox();
    expect(bounds).not.toBeNull();
    expect(bounds!.x).toBeGreaterThanOrEqual(0);
    expect(bounds!.x + bounds!.width).toBeLessThanOrEqual(391);
    await page.screenshot({ path: testInfo.outputPath(`turn-reply-mobile-${theme}.png`), animations: "disabled" });
  }
  await page.getByRole("button", { name: "Interface settings", exact: true }).click();
  await page.getByRole("combobox", { name: "Language", exact: true }).selectOption("zh-CN");
  await page.keyboard.press("Escape");
  await expect(page.locator("html")).toHaveAttribute("lang", "zh-CN");
  await expectCommitted(turn);
  await page.screenshot({ path: testInfo.outputPath("turn-reply-mobile-chinese.png"), animations: "disabled" });
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

/** The committed turn: the final answer, and everything before it folded behind the disclosure. */
async function expectCommitted(turn: Locator) {
  const visible = turn.page().locator(":visible");
  await expect(turn.locator(".message-content").last()).toContainText("The final answer is ready.");
  await expect(turn.getByText("Process paragraph 1:", { exact: false }).and(visible)).toHaveCount(0);
  await expect(turn.getByText("Process paragraph 20:", { exact: false }).and(visible)).toHaveCount(0);
  await expect(turn.locator(".thinking-block").and(visible)).toHaveCount(0);
  await expect(turn.getByText("Inspect collapse fixture", { exact: true }).and(visible)).toHaveCount(0);
  await expect(turn.locator(".tool-card").and(visible)).toHaveCount(0);
  await expect(turn.getByRole("button", { name: "Expand execution process", exact: true })).toHaveCount(1);
  // Nothing is lost: the folded process is in the thread, and opening it shows it.
  await expect(turn.getByText("Process paragraph 20:", { exact: false })).not.toHaveCount(0);
}

/** The committed turn while the user reads above the bottom: the final answer, and the process still open. */
async function expectCommittedUnfolded(turn: Locator) {
  await expect(turn.locator(".message-content").last()).toContainText("The final answer is ready.");
  await expect(turn.getByText("Process paragraph 20:", { exact: false })).toBeVisible();
  await expect(turn.getByRole("button", { name: "Expand execution process", exact: true })).toHaveCount(0);
}

async function expectBottom(thread: Locator) {
  await expect.poll(() => thread.evaluate((element) =>
    element.scrollHeight - element.clientHeight - element.scrollTop
  )).toBeLessThan(3);
}

async function processGeometry(turn: Locator) {
  return turn.evaluate((content) => {
    const agent = content.querySelector(".assistant-chunk")!.getBoundingClientRect();
    const thought = content.querySelector(".thinking-block")!.getBoundingClientRect();
    const tool = content.querySelector(".tool-card")!.getBoundingClientRect();
    return {
      agentThoughtGap: thought.top - agent.bottom,
      thoughtToolGap: tool.top - thought.bottom,
      entries: [agent, thought, tool].map(({ x, width }) => ({ x, width })),
    };
  });
}
