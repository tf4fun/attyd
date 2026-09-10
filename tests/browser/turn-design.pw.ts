import { expect, test as baseTest, type Locator, type Page } from "@playwright/test";
import { join } from "node:path";
import { startRustTestServer } from "../../scripts/rust-test-server";

const test = baseTest.extend<{ turnDesignUrl: string }>({
  turnDesignUrl: async ({}, use) => {
    const server = await startRustTestServer({
      command: [process.execPath, "--import", "tsx", join(process.cwd(), "tests/fixtures/fake-agent.ts")],
      env: { ...process.env, ATTYD_FAKE_TURN_DESIGN: "1" },
    });
    try {
      await use(`http://127.0.0.1:${server.port}`);
    } finally {
      await server.close();
    }
  },
  baseURL: async ({ turnDesignUrl }, use) => use(turnDesignUrl),
});

const prompts = [
  "How should we introduce this project?",
  "Add the `goose` launch example:\n\n```sh\nattyd -- goose acp\n```\n\nKeep the explanation brief.",
  "Make the copy easier to scan on a phone.",
];

test("distinguishes historical prompts, code, and turn boundaries in both desktop themes", async ({ page }, testInfo) => {
  await page.setViewportSize({ width: 1280, height: 960 });
  const errors: string[] = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await createConversation(page);
  await page.reload();
  const turns = page.locator(".conversation-turn");
  await expect(turns).toHaveCount(3);
  await expect(turns.locator(".turn-heading")).toHaveText(["Turn 1", "Turn 2", "Turn 3"]);

  for (const theme of ["light", "dark"] as const) {
    await setTheme(page, theme);
    for (const turn of await turns.all()) await expectTurnHierarchy(turn, "You", "Agent");
    await expectDistinctCode(turns.nth(1));
    await alignTurn(page, turns.nth(1));
    await page.screenshot({ path: testInfo.outputPath(`turn-design-desktop-${theme}.png`), animations: "disabled" });
    const process = turns.nth(1).getByRole("button", { name: "Expand execution process", exact: true });
    await process.click();
    await expect(turns.nth(1).locator(".turn-process-trigger")).toHaveAttribute("aria-expanded", "true");
    await expect(turns.nth(1).locator(".turn-process-action")).toHaveText("Hide");
    await expect(turns.nth(1).getByText("Read quick-start instructions", { exact: true })).toBeVisible();
    await turns.nth(1).getByRole("button", { name: "Collapse execution process", exact: true }).click();
    await expect(turns.nth(1).locator(".turn-process-action")).toHaveText("Show");
  }
  expect(errors).toEqual([]);
});

test("keeps turn landmarks and process actions readable on a phone in Chinese", async ({ page }, testInfo) => {
  await page.setViewportSize({ width: 390, height: 844 });
  await createConversation(page);
  const turns = page.locator(".conversation-turn");
  await page.getByRole("button", { name: "Interface settings", exact: true }).click();
  await page.getByRole("combobox", { name: "Language", exact: true }).selectOption("zh-CN");
  await page.keyboard.press("Escape");
  await expect(turns.locator(".turn-heading")).toHaveText(["第 1 轮", "第 2 轮", "第 3 轮"]);

  for (const theme of ["light", "dark"] as const) {
    await setTheme(page, theme, true);
    for (const turn of await turns.all()) await expectTurnHierarchy(turn, "你", "Agent");
    for (const index of [0, 2]) {
      const prompt = turns.nth(index).locator(".message-user");
      expect(await prompt.evaluate((element) => element.getBoundingClientRect().height))
        .toBeLessThanOrEqual(70);
    }
    await expectDistinctCode(turns.nth(1));
    expect(await page.evaluate(() => Math.max(
      document.documentElement.scrollWidth - window.innerWidth,
      document.body.scrollWidth - window.innerWidth,
    ))).toBeLessThanOrEqual(1);
    await alignTurn(page, turns.nth(1));
    await page.screenshot({ path: testInfo.outputPath(`turn-design-mobile-${theme}.png`), animations: "disabled" });
    const process = turns.nth(1).getByRole("button", { name: "展开执行过程", exact: true });
    await process.click();
    await expect(turns.nth(1).getByRole("button", { name: "收起执行过程", exact: true })).toHaveAttribute("aria-expanded", "true");
    await expect(turns.nth(1).getByText("Read quick-start instructions", { exact: true })).toBeVisible();
    await page.screenshot({ path: testInfo.outputPath(`turn-design-mobile-${theme}-expanded.png`), animations: "disabled" });
    await turns.nth(1).getByRole("button", { name: "收起执行过程", exact: true }).click();
  }

  const prompt = turns.nth(1).locator(".message-user");
  const info = prompt.getByRole("button", { name: "消息信息", exact: true });
  await info.click();
  const debug = prompt.getByRole("region", { name: "消息调试信息", exact: true });
  await expect(debug).toBeVisible();
  expect(await prompt.evaluate((element) => {
    const content = element.querySelector(".message-content")!.getBoundingClientRect();
    const panel = element.querySelector(".debug-info-panel")!;
    const panelRect = panel.getBoundingClientRect();
    const promptRect = element.getBoundingClientRect();
    return {
      belowContent: panelRect.top >= content.bottom,
      withinPrompt: panelRect.left >= promptRect.left && panelRect.right <= promptRect.right + 1,
      position: getComputedStyle(panel).position,
    };
  })).toEqual({ belowContent: true, withinPrompt: true, position: "static" });
  await info.click();
  await expect(debug).toBeHidden();
  await prompt.getByRole("button", { name: "编辑并重新发送用户消息", exact: true }).click();
  const composer = page.locator('textarea[role="combobox"]');
  await expect(composer).toHaveValue(prompts[1]!);
  await expect(composer).toBeFocused();
});

async function createConversation(page: Page) {
  await page.goto("/");
  await page.getByRole("button", { name: "New project", exact: true }).click();
  const dialog = page.getByRole("dialog", { name: "New project", exact: true });
  await dialog.getByLabel("Project working directory", { exact: true }).fill(process.cwd());
  await dialog.getByRole("button", { name: "Create project", exact: true }).click();
  const composer = page.locator('textarea[role="combobox"]');
  for (const [index, prompt] of prompts.entries()) {
    await expect(composer).toBeEnabled();
    await composer.fill(prompt);
    await composer.press("Enter");
    const turn = page.locator(".conversation-turn").nth(index);
    await expect(turn.getByRole("button", { name: "Expand execution process", exact: true })).toHaveAttribute("aria-expanded", "false");
    await expect(page.getByRole("button", { name: "Send prompt", exact: true })).toBeVisible();
  }
}

async function setTheme(page: Page, theme: "light" | "dark", chinese = false) {
  await page.getByRole("button", { name: chinese ? "界面设置" : "Interface settings", exact: true }).click();
  await page.getByRole("combobox", { name: chinese ? "外观" : "Appearance", exact: true }).selectOption(theme);
  await page.keyboard.press("Escape");
  await expect(page.locator("html")).toHaveAttribute("data-theme", theme);
}

async function expectTurnHierarchy(turn: Locator, user: string, agent: string) {
  await expect(turn.locator(".message-user .message-role")).toHaveText(user);
  await expect(turn.locator(".turn-agent-label")).toHaveText(agent);
  const layout = await turn.evaluate((element) => {
    const prompt = element.querySelector(".message-user")!;
    const trigger = element.querySelector(".turn-process-trigger")!;
    const heading = element.querySelector(".turn-heading")!;
    const header = prompt.querySelector(".message-header")!;
    const roleRect = header.querySelector(".message-role")!.getBoundingClientRect();
    const actions = header.querySelector(".message-meta-actions")!;
    const actionsRect = actions.getBoundingClientRect();
    const content = prompt.querySelector(".message-content")!;
    const contentRect = content.getBoundingClientRect();
    const answer = element.querySelector(".message-agent .message-content")!;
    const promptStyle = getComputedStyle(prompt);
    const contentStyle = getComputedStyle(content);
    const answerStyle = getComputedStyle(answer);
    const turnRect = element.getBoundingClientRect();
    const promptRect = prompt.getBoundingClientRect();
    const triggerRect = trigger.getBoundingClientRect();
    const triggerStyle = getComputedStyle(trigger);
    const ruleStyle = getComputedStyle(heading, "::after");
    const action = trigger.querySelector(".turn-process-action")!;
    return {
      promptRatio: promptRect.width / turnRect.width,
      promptLeft: promptRect.left,
      turnLeft: turnRect.left,
      promptBorder: promptStyle.borderTopWidth,
      promptBackground: promptStyle.backgroundColor,
      sharedFont: contentStyle.fontFamily === answerStyle.fontFamily,
      sharedSize: contentStyle.fontSize === answerStyle.fontSize,
      sharedLeading: contentStyle.lineHeight === answerStyle.lineHeight,
      proseFontSize: Number.parseFloat(contentStyle.fontSize),
      headerAligned: Math.abs((roleRect.top + roleRect.bottom) / 2 - (actionsRect.top + actionsRect.bottom) / 2) <= 1,
      actionsAboveContent: actionsRect.bottom <= contentRect.top,
      actionsWithinPrompt: actionsRect.right <= promptRect.right && actionsRect.left >= roleRect.right,
      actionsOpacity: getComputedStyle(actions).opacity,
      targetSizes: [...actions.querySelectorAll("button")].map((button) => {
        const rect = button.getBoundingClientRect();
        return Math.min(rect.width, rect.height);
      }),
      triggerHeight: triggerRect.height,
      triggerRatio: triggerRect.width / turnRect.width,
      triggerBorder: triggerStyle.borderTopWidth,
      triggerBorderColor: triggerStyle.borderTopColor,
      triggerBackground: triggerStyle.backgroundColor,
      actionOpacity: getComputedStyle(action).opacity,
      separator: ruleStyle.content,
    };
  });
  expect(layout.promptRatio).toBeCloseTo(1, 2);
  expect(Math.abs(layout.promptLeft - layout.turnLeft)).toBeLessThanOrEqual(1);
  expect(Number.parseFloat(layout.promptBorder)).toBe(0);
  expect(layout.promptBackground).toBe("rgba(0, 0, 0, 0)");
  expect(layout.sharedFont).toBe(true);
  expect(layout.sharedSize).toBe(true);
  expect(layout.sharedLeading).toBe(true);
  expect(layout.proseFontSize).toBeGreaterThanOrEqual(14);
  expect(layout.headerAligned).toBe(true);
  expect(layout.actionsAboveContent).toBe(true);
  expect(layout.actionsWithinPrompt).toBe(true);
  expect(layout.actionsOpacity).toBe("1");
  expect(layout.targetSizes.every((size) => size >= 26)).toBe(true);
  expect(layout.triggerHeight).toBeGreaterThanOrEqual(44);
  expect(layout.triggerRatio).toBeGreaterThan(0.98);
  expect(Number.parseFloat(layout.triggerBorder)).toBeGreaterThan(0);
  expect(layout.triggerBorderColor).not.toBe("rgba(0, 0, 0, 0)");
  expect(layout.triggerBackground).not.toBe("rgba(0, 0, 0, 0)");
  expect(layout.actionOpacity).toBe("1");
  expect(layout.separator).not.toBe("none");
  await expect(turn.locator(".turn-process-count")).toHaveText(/[1-9]/u);
  await expect(turn.locator(".turn-process-action")).toBeVisible();
}

async function expectDistinctCode(turn: Locator) {
  const styles = await turn.evaluate((element) => {
    const prose = element.querySelector(".message-user .markdown p")!;
    const code = element.querySelector(".message-agent .markdown pre code")!;
    const userCode = element.querySelector(".message-user .markdown pre code")!;
    return {
      proseFont: getComputedStyle(prose).fontFamily,
      codeFont: getComputedStyle(code).fontFamily,
      userCodeFont: getComputedStyle(userCode).fontFamily,
      promptBackground: getComputedStyle(element.querySelector(".message-user")!).backgroundColor,
      codeBackground: getComputedStyle(code.closest("pre")!).backgroundColor,
    };
  });
  expect(styles.proseFont).not.toBe(styles.codeFont);
  expect(styles.proseFont).not.toContain("monospace");
  expect(styles.codeFont).toContain("monospace");
  expect(styles.userCodeFont).toBe(styles.codeFont);
  expect(styles.promptBackground).not.toBe(styles.codeBackground);
}

async function alignTurn(page: Page, turn: Locator) {
  await turn.evaluate((element) => {
    const thread = element.closest<HTMLElement>('[role="region"]')!;
    thread.scrollTop += element.getBoundingClientRect().top - thread.getBoundingClientRect().top - 16;
  });
  await expect(turn.locator(".turn-heading")).toBeInViewport();
  await page.evaluate(() => document.fonts.ready);
}
