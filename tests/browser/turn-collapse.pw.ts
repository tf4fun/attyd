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

// `turn-collapse-flow` streams twenty "Process paragraph" Agent messages, a
// thought and a tool call, then the final answer. The paragraphs are
// `agent_message_chunk`s: they are part of the reply, so the log keeps them with
// the final answer. Only the thought and the tool call are process, which the
// commit removes (protocol S2); no process page exists to fetch it back.

test("shows the live process and keeps every Agent message, without process, after completion at the bottom", async ({ page, collapseFixture }, testInfo) => {
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
  // Process layout is checked while the turn still runs; nothing of it survives the commit.
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
    await expectCommitted(turn);
    // Removing process can shorten the scroll range past the old offset. The
    // browser must clamp there; within the remaining range the offset stays put.
    const maximum = await thread.evaluate((element) => element.scrollHeight - element.clientHeight);
    const expectedPosition = Math.min(readingPosition, maximum);
    expect(await thread.evaluate((element) => element.scrollTop)).toBe(expectedPosition);
    expect(await anchor.evaluate((element) => element.getBoundingClientRect().top))
      .toBe(anchorTop + readingPosition - expectedPosition);

    if (expectedPosition < maximum) {
      await page.getByRole("button", { name: "Jump to bottom of thread", exact: true }).click();
    }
    await expectBottom(thread);
  });
}

test("searches and copies the whole committed reply, including the earlier Agent messages", async ({ page, context, collapseFixture }) => {
  await context.grantPermissions(["clipboard-read", "clipboard-write"]);
  const { turn } = await startTurn(page);
  await collapseFixture.advance("append");
  await expect(turn.getByText("The final answer is ready.", { exact: true })).toBeVisible();
  await collapseFixture.advance("finish");
  await expectCommitted(turn);
  const copies = turn.getByRole("button", { name: "Copy agent response", exact: true });
  await copies.last().click();
  await expect.poll(() => page.evaluate(() => navigator.clipboard.readText())).toContain("Process paragraph 18:");
  expect(await page.evaluate(() => navigator.clipboard.readText())).toMatch(/The final answer is ready\.$/u);

  await page.getByRole("button", { name: "Search Agent thread", exact: true }).click();
  const query = page.getByRole("searchbox", { name: "Search this thread", exact: true });
  const counter = page.locator(".thread-search-navigation output");
  await query.fill("Process paragraph 18:");
  await expect(counter).toHaveText("1/1");
  await expect(turn.locator('[data-thread-search-active="true"]')).toBeInViewport();
  await query.fill("Inspect collapse fixture");
  await expect(counter).toHaveText("0/0");
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

/** The committed turn: every Agent message of the reply, and no process. */
async function expectCommitted(turn: Locator) {
  await expect(turn.locator(".message-content").getByText("Process paragraph 1:", { exact: false })).toBeVisible();
  await expect(turn.locator(".message-content").getByText("Process paragraph 20:", { exact: false })).toBeVisible();
  // canon merges the paragraphs and the final answer into one text block, so
  // the committed reply is checked by content, not by message boundaries.
  await expect(turn.locator(".message-content").last()).toContainText("The final answer is ready.");
  await expect(turn.locator(".thinking-block")).toHaveCount(0);
  await expect(turn.getByText("Inspect collapse fixture", { exact: true })).toHaveCount(0);
  await expect(turn.locator(".tool-card")).toHaveCount(0);
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
