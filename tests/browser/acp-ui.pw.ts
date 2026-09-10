import {
  expect,
  test as baseTest,
  type Locator,
  type Page,
} from "@playwright/test";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { startRustTestServer } from "../../scripts/rust-test-server";
import { projectPath, sessionPath } from "../../web/src/lib/session-route";

const test = baseTest.extend<{ isolatedAttydUrl: string }>({
  isolatedAttydUrl: async ({}, use) => {
    const cwd = process.cwd();
    const server = await startRustTestServer({
      cwd,
      command: [
        process.execPath,
        "--import",
        "tsx",
        join(cwd, "tests/fixtures/fake-agent.ts"),
        "--early-new-updates",
        "--early-fork-updates",
      ],
    });
    try {
      await use(`http://127.0.0.1:${server.port}`);
    } finally {
      await server.close();
    }
  },
  baseURL: async ({ isolatedAttydUrl }, use) => use(isolatedAttydUrl),
});

test.describe("interface localization", () => {
  test.use({ locale: "zh-CN" });

  test("detects Chinese and remembers an explicit language across reloads", async ({ page }) => {
    const browserErrors = collectBrowserErrors(page);
    await page.goto("/");
    await expect(page.getByRole("heading", { name: "项目", exact: true })).toBeVisible();
    await expect(page.locator("html")).toHaveAttribute("lang", "zh-CN");
    await page.getByRole("button", { name: "界面设置", exact: true }).click();
    await page.getByRole("combobox", { name: "界面语言", exact: true }).selectOption("en");
    await expect(page.getByRole("heading", { name: "Projects", exact: true })).toBeVisible();
    await page.reload();
    await expect(page.locator("html")).toHaveAttribute("lang", "en");
    await page.getByRole("button", { name: "Interface settings", exact: true }).click();
    await expect(page.getByRole("combobox", { name: "Language", exact: true })).toHaveValue("en");
    await page.getByRole("combobox", { name: "Language", exact: true }).selectOption("system");
    await expect(page.getByRole("heading", { name: "项目", exact: true })).toBeVisible();
    expect(await page.evaluate(() => localStorage.getItem("attyd.language"))).toBeNull();
    expect(browserErrors).toEqual([]);
  });

  test("switches language on mobile without reloading the session or losing its draft", async ({ page }, testInfo) => {
    await page.setViewportSize({ width: 390, height: 844 });
    const browserErrors = collectBrowserErrors(page);
    await page.goto("/sessions/saved-session");
    const composer = page.locator('textarea[role="combobox"]');
    await expect(composer).toBeEnabled();
    await expect(page.getByText("Loaded history.", { exact: true })).toBeVisible();
    await composer.fill("保留我的草稿 — keep this draft");
    const originalUrl = page.url();
    let sessionRequests = 0;
    page.on("request", (request) => {
      if (new URL(request.url()).pathname.startsWith("/api/v1/sessions/")) sessionRequests += 1;
    });
    await page.getByRole("button", { name: "界面设置", exact: true }).click();
    await page.getByRole("combobox", { name: "界面语言", exact: true }).selectOption("en");
    await expect(page.locator("html")).toHaveAttribute("lang", "en");
    await expect(composer).toHaveValue("保留我的草稿 — keep this draft");
    await expect(page.getByText("Loaded history.", { exact: true })).toBeVisible();
    await expect(page.getByRole("heading", { name: "Saved ACP session", exact: true })).toBeVisible();
    await page.getByRole("combobox", { name: "Language", exact: true }).selectOption("zh-CN");
    await expect(composer).toHaveValue("保留我的草稿 — keep this draft");
    expect(page.url()).toBe(originalUrl);
    expect(sessionRequests).toBe(0);
    expect(await horizontalOverflow(page)).toBeLessThanOrEqual(1);
    await page.screenshot({ path: testInfo.outputPath("i18n-mobile.png") });
    expect(browserErrors).toEqual([]);
  });
});

test.describe("interface appearance", () => {
  test("separates interface preferences from Agent settings and follows the selected theme", async ({ page }) => {
    const browserErrors = collectBrowserErrors(page);
    await page.emulateMedia({ colorScheme: "light" });
    await page.goto("/");
    await expect(page.getByRole("heading", { name: "Projects", exact: true })).toBeVisible();
    await expect(page.locator("html")).toHaveAttribute("data-theme", "light");

    const interfaceSettings = page.getByRole("button", { name: "Interface settings", exact: true });
    const agentSettings = page.getByRole("button", { name: "Agent settings", exact: true });
    const interfacePanel = page.locator(".interface-settings-body");
    const appearance = interfacePanel.getByRole("combobox", { name: "Appearance", exact: true });
    await agentSettings.click();
    await expect(page.locator(".agent-details-body")).toBeVisible();
    await expect(page.locator(".agent-details-body").getByRole("combobox", { name: "Language", exact: true }))
      .toHaveCount(0);
    await expect(page.locator(".agent-details-body").getByRole("combobox", { name: "Appearance", exact: true }))
      .toHaveCount(0);
    await interfaceSettings.click();
    await expect(page.locator(".agent-details")).not.toHaveAttribute("open", "");
    await expect(interfacePanel.getByRole("combobox", { name: "Language", exact: true })).toBeVisible();
    await expect(appearance).toHaveValue("system");
    expect(await page.evaluate(() => localStorage.getItem("attyd.theme"))).toBeNull();

    const lightBackground = await page.locator("html").evaluate((element) => getComputedStyle(element).backgroundColor);
    await appearance.selectOption("dark");
    await expect(page.locator("html")).toHaveAttribute("data-theme", "dark");
    await expect.poll(() => page.locator("html").evaluate((element) => getComputedStyle(element).backgroundColor))
      .not.toBe(lightBackground);
    expect(await page.evaluate(() => localStorage.getItem("attyd.theme"))).toBe("dark");
    await page.reload();
    await expect(page.locator("html")).toHaveAttribute("data-theme", "dark");
    await interfaceSettings.click();
    await expect(appearance).toHaveValue("dark");

    await appearance.selectOption("light");
    await page.emulateMedia({ colorScheme: "dark" });
    await expect(page.locator("html")).toHaveAttribute("data-theme", "light");
    expect(await page.evaluate(() => localStorage.getItem("attyd.theme"))).toBe("light");
    await appearance.selectOption("system");
    await expect(page.locator("html")).toHaveAttribute("data-theme", "dark");
    expect(await page.evaluate(() => localStorage.getItem("attyd.theme"))).toBeNull();
    await page.emulateMedia({ colorScheme: "light" });
    await expect(page.locator("html")).toHaveAttribute("data-theme", "light");

    await agentSettings.click();
    await expect(page.locator(".interface-settings")).not.toHaveAttribute("open", "");
    await expect(page.locator(".agent-details-body")).toBeVisible();
    expect(browserErrors).toEqual([]);
  });

  test("keeps interface settings visible on desktop and mobile and restores keyboard focus", async ({ page }, testInfo) => {
    const browserErrors = collectBrowserErrors(page);
    await page.goto("/sessions/saved-session");
    await expect(page.locator('textarea[role="combobox"]')).toBeEnabled();
    const trigger = page.getByRole("button", { name: "Interface settings", exact: true });
    const settings = page.locator(".interface-settings");
    const panel = page.locator(".interface-settings-body");
    for (const viewport of [
      { width: 1280, height: 900 },
      { width: 390, height: 844 },
      { width: 390, height: 400 },
      { width: 667, height: 375 },
      { width: 850, height: 300 },
    ]) {
      await page.setViewportSize(viewport);
      await trigger.focus();
      await trigger.press("Enter");
      await expect(settings).toHaveAttribute("open", "");
      await expectWithinViewport(panel, viewport.width);
      await expect.poll(() => panel.evaluate((element) => {
        const bounds = element.getBoundingClientRect();
        return bounds.top >= 0 && bounds.bottom <= window.innerHeight + 1 && bounds.height > 0;
      })).toBe(true);
      const appearance = panel.getByRole("combobox", { name: "Appearance", exact: true });
      await appearance.scrollIntoViewIfNeeded();
      await expect(appearance).toBeInViewport({ ratio: 0.98 });
      if (viewport.height >= 800) {
        for (const theme of ["light", "dark"]) {
          await appearance.selectOption(theme);
          await expect(page.locator("html")).toHaveAttribute("data-theme", theme);
          const device = viewport.width < 600 ? "mobile" : "desktop";
          await page.screenshot({ path: testInfo.outputPath(`interface-${device}-${theme}.png`) });
        }
      }
      await appearance.focus();
      await page.keyboard.press("Escape");
      await expect(settings).not.toHaveAttribute("open", "");
      await expect(trigger).toBeFocused();
      expect(await horizontalOverflow(page)).toBeLessThanOrEqual(1);
    }
    expect(browserErrors).toEqual([]);
  });

  test("preserves the session, draft and reading position when changing theme", async ({ page }, testInfo) => {
    const browserErrors = collectBrowserErrors(page);
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto("/sessions/saved-session");
    const composer = page.locator('textarea[role="combobox"]');
    const thread = page.getByRole("region", { name: "Conversation thread" });
    await expect(composer).toBeEnabled();
    await composer.fill("stream-follow-flow");
    await composer.press("Enter");
    await expect(page.getByText("Stream follow complete.", { exact: true })).toBeVisible();
    await expect(page.getByRole("button", { name: "Send prompt", exact: true })).toBeVisible();
    await composer.fill("Keep this unsent draft — 保留草稿");
    const composerNode = await composer.elementHandle();
    const originalUrl = page.url();
    let sessionRequests = 0;
    page.on("request", (request) => {
      if (new URL(request.url()).pathname.startsWith("/api/v1/sessions/")) sessionRequests += 1;
    });

    for (const viewport of [
      { width: 1280, height: 900 },
      { width: 390, height: 844 },
    ]) {
      await page.setViewportSize(viewport);
      const jumpToBottom = page.getByRole("button", { name: "Jump to bottom of thread" });
      if (await jumpToBottom.isEnabled()) await jumpToBottom.click();
      await expect.poll(() => thread.evaluate((element) =>
        element.scrollHeight - element.clientHeight - element.scrollTop
      )).toBeLessThan(3);
      await page.getByRole("button", { name: "Interface settings", exact: true }).click();
      const appearance = page.getByRole("combobox", { name: "Appearance", exact: true });
      for (const theme of ["light", "dark"]) {
        await appearance.selectOption(theme);
        await expect(page.locator("html")).toHaveAttribute("data-theme", theme);
        await expect.poll(() => thread.evaluate((element) =>
          element.scrollHeight - element.clientHeight - element.scrollTop
        )).toBeLessThan(3);
      }
      await page.keyboard.press("Escape");
      await thread.evaluate((element) => {
        element.dispatchEvent(new WheelEvent("wheel", { bubbles: true, deltaY: -300 }));
        element.scrollTop = Math.max(0, (element.scrollHeight - element.clientHeight) / 2);
      });
      await expect(jumpToBottom).toBeEnabled();
      const readingTop = await thread.evaluate((element) => element.scrollTop);
      await page.getByRole("button", { name: "Interface settings", exact: true }).click();
      for (const theme of ["light", "dark"]) {
        await appearance.selectOption(theme);
        await expect(page.locator("html")).toHaveAttribute("data-theme", theme);
        await expect.poll(() => thread.evaluate((element) => element.scrollTop))
          .toBeCloseTo(readingTop, 0);
        await expect(composer).toHaveValue("Keep this unsent draft — 保留草稿");
        expect(await composerNode!.evaluate((element) => element.isConnected)).toBe(true);
        const device = viewport.width < 600 ? "mobile" : "desktop";
        await page.screenshot({ path: testInfo.outputPath(`reading-${device}-${theme}.png`) });
      }
      await page.keyboard.press("Escape");
      expect(await horizontalOverflow(page)).toBeLessThanOrEqual(1);
    }
    await expect(page.getByText("Loaded history.", { exact: true })).toHaveCount(1);
    expect(page.url()).toBe(originalUrl);
    expect(sessionRequests).toBe(0);
    expect(browserErrors).toEqual([]);
  });
});

test("drives permission and form ACP interactions with real focus restoration", async ({ page }) => {
  const browserErrors = collectBrowserErrors(page);
  await page.goto("/sessions/saved-session");

  const composer = page.locator('textarea[role="combobox"]');
  await expect(composer).toBeEnabled();
  await composer.fill("browser permission flow");
  await composer.press("Enter");

  const permission = page.getByRole("alertdialog", { name: "Agent permission request" });
  const allow = permission.getByRole("button", { name: "Allow once" });
  await expect(permission).toBeVisible();
  await expect(allow).toBeFocused();
  await expect(permission.getByText("read_file", { exact: true })).toBeVisible();
  await expect(permission.getByText("/workspace/fixture.ts:0", { exact: true })).toBeVisible();
  const toolInput = permission.locator(".permission-subject .raw-json");
  await expect(toolInput).toHaveAttribute("open", "");
  await expect(toolInput.locator("pre")).toContainText('"lineEnd": 40');
  await allow.click();
  await expect(permission).toBeHidden();
  await expect(composer).toBeFocused();
  await expect(page.getByText("ACP works.", { exact: true })).toBeVisible();

  await page.getByRole("button", { name: "Edit and resend user message" }).click();
  await expect(composer).toHaveValue("browser permission flow");
  await expect(composer).toBeFocused();

  await composer.fill("form-flow");
  await composer.press("Enter");
  const form = page.getByRole("dialog", { name: "Agent input request" });
  const name = form.getByLabel(/Name/);
  await expect(form).toBeVisible();
  await expect(name).toBeFocused();
  await name.fill("Ada Lovelace");
  await form.getByLabel(/Count/).fill("2");
  await form.getByLabel(/Channel/).selectOption("stable");
  await form.getByLabel("fast").check();
  await form.getByLabel(/Starts at/).fill("2026-08-31T00:00:00Z");
  await expect(form.getByLabel(/Confirmed/)).toBeChecked();
  await form.getByRole("button", { name: "Submit" }).click();

  await expect(form).toBeHidden();
  await expect(page.getByText("Form accept.", { exact: true })).toBeVisible();
  await expect(composer).toBeFocused();
  await expect(page.getByRole("alert")).toHaveCount(0);
  expect(await horizontalOverflow(page)).toBeLessThanOrEqual(1);
  expect(browserErrors).toEqual([]);
});

test("queues ACP follow-ups and uses session cancel for Send now", async ({ page }) => {
  const browserErrors = collectBrowserErrors(page);
  await page.goto("/sessions/saved-session");

  const composer = page.locator('textarea[role="combobox"]');
  await expect(composer).toBeEnabled();
  await composer.fill("browser permission flow");
  await composer.press("Enter");
  const permission = page.getByRole("alertdialog", { name: "Agent permission request" });
  await expect(permission).toBeVisible();
  await expect(composer).toHaveAttribute("placeholder", "Queue a follow-up…");

  await composer.fill("usage-flow");
  await composer.press("Enter");
  await composer.fill("request-error-flow");
  await composer.press("Enter");
  const queue = page.getByRole("region", { name: "Queued messages" });
  await expect(queue).toContainText("2 queued");
  await expect(page.getByRole("article", { name: "Queued message 1" })).toContainText("usage-flow");

  await page.getByRole("button", { name: "Edit queued message 2" }).click();
  await expect(composer).toHaveValue("request-error-flow");
  await expect(queue).toContainText("1 queued");
  await composer.press("Enter");
  await expect(queue).toContainText("2 queued");

  await page.getByRole("button", { name: "Send queued message 1 now" }).click();
  await expect(permission).toBeHidden();
  await expect(page.getByText("ACP works.", { exact: true })).toBeVisible();
  await expect(page.getByText("max_tokens", { exact: true })).toBeVisible();
  await expect(page.locator(".message-user").filter({ hasText: "usage-flow" })).toBeVisible();
  await expect(page.locator(".message-user").filter({ hasText: "request-error-flow" })).toBeVisible();
  await expect(page.getByRole("heading", { name: "Sign in to continue" })).toBeVisible();
  await expect(queue).toBeHidden();
  expect(browserErrors).toEqual([]);
});

test("keeps queued ACP work paused after Stop and resumes it after a new message", async ({ page }) => {
  const browserErrors = collectBrowserErrors(page);
  await page.goto("/sessions/saved-session");

  const composer = page.locator('textarea[role="combobox"]');
  await expect(composer).toBeEnabled();
  await composer.fill("browser permission flow");
  await composer.press("Enter");
  await expect(page.getByRole("alertdialog", { name: "Agent permission request" })).toBeVisible();

  await composer.fill("usage-flow");
  await composer.press("Enter");
  const queue = page.getByRole("region", { name: "Queued messages" });
  await expect(queue).toContainText("1 queued");
  await composer.press("Escape");
  await expect(queue).toContainText("1 queued · paused");
  await expect(page.getByText("ACP works.", { exact: true })).toBeVisible();
  await expect(page.locator(".message-user").filter({ hasText: "usage-flow" })).toHaveCount(0);

  await composer.fill("activity-flow");
  await composer.press("Enter");
  await expect(page.getByText("Activity flow complete.", { exact: true })).toBeVisible();
  await expect(page.locator(".message-user").filter({ hasText: "usage-flow" })).toBeVisible();
  await expect(page.getByText("max_tokens", { exact: true })).toBeVisible();
  await expect(queue).toBeHidden();
  expect(browserErrors).toEqual([]);
});

test("pastes and drops negotiated ACP context into the Zed-style composer", async ({ page }) => {
  const browserErrors = collectBrowserErrors(page);
  await page.goto("/sessions/saved-session");
  const editor = page.locator('textarea[role="combobox"]');
  await expect(editor).toBeEnabled();

  await editor.evaluate((element) => {
    const encoded = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";
    const bytes = Uint8Array.from(atob(encoded), (character) => character.charCodeAt(0));
    const transfer = new DataTransfer();
    transfer.items.add(new File([bytes], "clipboard.png", { type: "image/png" }));
    element.dispatchEvent(new ClipboardEvent("paste", {
      bubbles: true,
      cancelable: true,
      clipboardData: transfer,
    }));
  });
  const attachments = page.locator(".attachment-list");
  await expect(attachments).toContainText("clipboard.png");
  await expect(attachments).toContainText("image");

  const composer = page.locator(".composer");
  await composer.evaluate((element) => {
    const transfer = new DataTransfer();
    transfer.items.add(new File(["# Project context\n"], "context.md", {
      type: "text/markdown",
    }));
    element.dispatchEvent(new DragEvent("dragenter", {
      bubbles: true,
      cancelable: true,
      dataTransfer: transfer,
    }));
  });
  await expect(page.getByRole("status").filter({ hasText: "Drop files to add context" }))
    .toBeVisible();
  await composer.evaluate((element) => {
    const transfer = new DataTransfer();
    transfer.items.add(new File(["# Project context\n"], "context.md", {
      type: "text/markdown",
    }));
    element.dispatchEvent(new DragEvent("drop", {
      bubbles: true,
      cancelable: true,
      dataTransfer: transfer,
    }));
  });
  await expect(attachments).toContainText("context.md");
  await expect(attachments).toContainText("context");

  await editor.fill("attachment-input-flow");
  await editor.press("Enter");
  await expect(page.getByText("Received prompt blocks: text,image,resource.", { exact: true }))
    .toBeVisible();
  const prompt = page.locator(".message-user").filter({ hasText: "attachment-input-flow" });
  await expect(prompt.getByAltText("clipboard.png")).toBeVisible();
  await expect(prompt).toContainText("# Project context");

  await editor.press("ArrowUp");
  await expect(editor).toHaveValue("attachment-input-flow");
  await expect(attachments).toContainText("clipboard.png");
  await expect(attachments).toContainText("context.md");
  await expect(page.locator(".composer-bar")).toContainText(/previous prompts/);
  await editor.press("ArrowDown");
  await expect(editor).toHaveValue("");
  await expect(attachments).toBeHidden();
  expect(await horizontalOverflow(page)).toBeLessThanOrEqual(1);
  expect(browserErrors).toEqual([]);
});

test("adds a workspace file through a Zed-style @ mention", async ({ page }) => {
  const browserErrors = collectBrowserErrors(page);
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto("/sessions/saved-session");
  const editor = page.locator('textarea[role="combobox"]');
  await expect(editor).toBeEnabled();

  await editor.fill("Review @workspace-context-note");
  const contextMenu = page.getByRole("listbox", { name: "Workspace context" });
  await expect(contextMenu).toBeVisible();
  const contextFile = contextMenu.getByRole("option", { name: /workspace-context-note\.md/ });
  await expect(contextFile).toContainText("tests/fixtures/workspace-context-note.md");
  await contextFile.click();

  const attachments = page.locator(".attachment-list");
  await expect(attachments).toContainText("tests/fixtures/workspace-context-note.md");
  await expect(editor).toHaveValue("Review ");
  await editor.fill("attachment-input-flow");
  await editor.press("Enter");

  await expect(page.getByText("Received prompt blocks: text,resource.", { exact: true }))
    .toBeVisible();
  const prompt = page.locator(".message-user").filter({ hasText: "attachment-input-flow" });
  await expect(prompt).toContainText("# Workspace context");
  expect(await horizontalOverflow(page)).toBeLessThanOrEqual(1);
  expect(browserErrors).toEqual([]);
});

test("renders structured ACP errors and retries the exact failed prompt", async ({ page }) => {
  const browserErrors = collectBrowserErrors(page);
  await page.goto("/sessions/saved-session");
  const composer = page.locator('textarea[role="combobox"]');
  await expect(composer).toBeEnabled();
  await expect(page.getByRole("button", { name: "NES" })).toHaveCount(0);

  await composer.fill("structured-error-flow");
  await composer.press("Enter");

  const failure = page.getByRole("alert").filter({ hasText: "Agent turn failed" });
  await expect(failure).toContainText("-32603");
  await expect(failure).toContainText("Synthetic structured failure");
  await failure.getByText("ACP error details", { exact: true }).click();
  await expect(failure).toContainText("Retry the same ACP ContentBlocks");
  await expect(failure).toContainText('"owner": "Agent"');

  await failure.getByRole("button", { name: "Retry" }).click();
  await expect(page.getByText("Recovered after structured ACP error.", { exact: true }))
    .toBeVisible();
  await expect(page.locator(".message-user").filter({ hasText: "structured-error-flow" }))
    .toHaveCount(2);
  expect(await horizontalOverflow(page)).toBeLessThanOrEqual(1);
  expect(browserErrors).toEqual([]);
});

test("keeps mobile session and Agent popovers bounded and restores focus after Escape", async ({ page }) => {
  const browserErrors = collectBrowserErrors(page);
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto("/sessions/saved-session");
  await expect(page.locator('textarea[role="combobox"]')).toBeEnabled();
  await expectFullWidthMain(page);

  const trigger = page.getByRole("button", { name: "Switch project session", exact: true });
  const picker = await openSessionPicker(page);
  await expect(picker).toHaveAttribute("open", "");
  await expectWithinViewport(picker.locator(".session-history"), 390);
  await page.keyboard.press("Escape");
  await expect(picker).not.toHaveAttribute("open", "");
  await expect(trigger).toBeFocused();

  const settings = page.getByRole("button", { name: "Agent settings", exact: true });
  await settings.click();
  await expect(page.locator(".agent-details")).toHaveAttribute("open", "");
  await expectWithinViewport(page.locator(".agent-details-body"), 390);
  await page.keyboard.press("Escape");
  await expect(page.locator(".agent-details")).not.toHaveAttribute("open", "");
  await expect(settings).toBeFocused();

  await settings.press("Enter");
  await expect(page.locator(".agent-details")).toHaveAttribute("open", "");
  const newThread = page.getByRole("button", { name: "New thread", exact: true });
  await newThread.focus();
  await newThread.press("Enter");
  const dialog = page.getByRole("dialog", { name: "New thread", exact: true });
  await expect(dialog).toBeVisible();
  await expect(page.locator(".agent-details")).not.toHaveAttribute("open", "");
  const headerCovered = await newThread.evaluate((button) => {
    const bounds = button.getBoundingClientRect();
    return document.elementFromPoint(bounds.x + bounds.width / 2, bounds.y + bounds.height / 2)
      ?.closest(".new-thread-overlay") != null;
  });
  expect(headerCovered).toBe(true);
  await dialog.getByRole("button", { name: "Cancel new thread" }).click();
  await expect(newThread).toBeFocused();

  for (const viewport of [
    { width: 390, height: 400 },
    { width: 667, height: 375 },
    { width: 850, height: 300 },
  ]) {
    const expectUsablePanel = async (panel: Locator) => {
      await expect(panel).toBeVisible();
      await expectWithinViewport(panel, viewport.width);
      await expect.poll(() => panel.evaluate((element) => {
        const bounds = element.getBoundingClientRect();
        return bounds.top >= 0 && bounds.bottom <= window.innerHeight + 1 && bounds.height > 0;
      })).toBe(true);
    };
    await page.setViewportSize({ width: viewport.width, height: 844 });
    await openSessionPicker(page);
    await page.setViewportSize(viewport);
    await expect(picker).toHaveAttribute("open", "");
    await expectUsablePanel(picker.locator(".session-switcher-panel"));
    const lastSession = picker.locator(".session-open").last();
    await lastSession.scrollIntoViewIfNeeded();
    await expect(lastSession).toBeInViewport({ ratio: 0.98 });

    await settings.click();
    await expect(picker).not.toHaveAttribute("open", "");
    const settingsPanel = page.locator(".agent-details-body");
    await expectUsablePanel(settingsPanel);
    const capabilities = settingsPanel.locator("summary").filter({ hasText: "Capabilities" });
    await capabilities.scrollIntoViewIfNeeded();
    await expect(capabilities).toBeInViewport({ ratio: 0.98 });
    await capabilities.click();
    await expectUsablePanel(settingsPanel);

    const actions = page.getByRole("button", { name: "Thread actions", exact: true });
    await actions.click();
    await expect(page.locator(".agent-details")).not.toHaveAttribute("open", "");
    const actionsPanel = page.locator(".thread-actions > div");
    await expectUsablePanel(actionsPanel);
    const lastAction = actionsPanel.getByRole("button", { name: "Delete thread", exact: true });
    await lastAction.scrollIntoViewIfNeeded();
    await expect(lastAction).toBeInViewport({ ratio: 0.98 });
    await page.keyboard.press("Escape");
    await expect(page.locator(".thread-actions")).not.toHaveAttribute("open", "");
    await expect(actions).toBeFocused();
  }
  expect(await horizontalOverflow(page)).toBeLessThanOrEqual(1);
  expect(browserErrors).toEqual([]);
});

test("filters only project sessions in the session switcher", async ({ page }) => {
  const browserErrors = collectBrowserErrors(page);
  await page.goto("/sessions/saved-session");
  await expect(page.locator('textarea[role="combobox"]')).toBeEnabled();

  const picker = await openSessionPicker(page);
  const filter = picker.getByRole("searchbox", { name: "Filter loaded Agent threads" });
  await expect(filter).toBeVisible();
  await expect(picker.locator(".session-filter-status")).toHaveText("2 loaded threads");
  const workspaceGroup = picker.locator(".session-group").filter({
    has: page.getByRole("heading", { name: process.cwd(), exact: true }),
  });
  await expect(workspaceGroup.locator(".session-group-label"))
    .toHaveAttribute("title", process.cwd());
  await expect(picker.locator(".session-group-label")).toHaveCount(1);
  await expect(workspaceGroup.locator('[aria-current="page"]')).toBeVisible();
  await expect(picker.getByText("Earlier Agent thread", { exact: true })).toBeVisible();
  const currentThread = picker.locator('[aria-current="page"]');

  await filter.fill("ear agent");
  await expect(picker.locator(".session-filter-status")).toHaveText("1 of 2 loaded threads");
  await expect(picker.getByText("Earlier Agent thread", { exact: true })).toBeVisible();
  await expect(currentThread).toBeHidden();
  await filter.press("Escape");
  await expect(filter).toHaveValue("");
  await expect(picker).toHaveAttribute("open", "");
  await expect(picker.locator('[aria-current="page"]')).toBeVisible();

  await page.setViewportSize({ width: 390, height: 844 });
  await expect(filter).toBeVisible();
  await expectWithinViewport(picker.locator(".session-history"), 390);
  await expectFullWidthMain(page);
  expect(await horizontalOverflow(page)).toBeLessThanOrEqual(1);
  expect(browserErrors).toEqual([]);
});

test("switches between already-open ACP threads without loading them twice", async ({ page }) => {
  const browserErrors = collectBrowserErrors(page);
  await page.goto("/sessions/saved-session");

  const picker = page.locator(".session-switcher");
  const thread = page.getByRole("region", { name: "Conversation thread" });
  await expect(page.getByRole("heading", { name: "Saved ACP session" })).toBeVisible();
  await page.addStyleTag({ content: ".conversation-wrap{min-height:1500px!important}" });

  await openSessionPicker(page);
  await picker.locator(".session-open")
    .filter({ hasText: "Earlier Agent thread" })
    .click();
  await expect(page.getByRole("heading", { name: "Earlier Agent thread" })).toBeVisible();
  await expect.poll(() => thread.evaluate((element) =>
    element.scrollHeight - element.clientHeight - element.scrollTop
  )).toBeLessThan(3);
  await expect(picker).not.toHaveAttribute("open", "");
  await openSessionPicker(page);
  await expect(picker.locator(".session-open").filter({ hasText: "Saved ACP session" }))
    .not.toContainText(/\bopen\b/iu);

  await thread.evaluate((element) => { element.scrollTop = 0; });
  await picker.locator(".session-open")
    .filter({ hasText: "Saved ACP session" })
    .click();
  await expect(page.getByRole("heading", { name: "Saved ACP session" })).toBeVisible();
  await expect.poll(() => thread.evaluate((element) =>
    element.scrollHeight - element.clientHeight - element.scrollTop
  )).toBeLessThan(3);
  await expect(page.getByText("Loaded history.", { exact: true })).toHaveCount(1);
  await expect(page.getByRole("alert")).toHaveCount(0);
  expect(browserErrors).toEqual([]);
});

test("runs different ACP sessions concurrently without treating running as a global lock", async ({ page }) => {
  const browserErrors = collectBrowserErrors(page);
  await page.goto("/sessions/saved-session");
  await expect(page.getByRole("heading", { name: "Saved ACP session" })).toBeVisible();

  const composer = page.locator('textarea[role="combobox"]');
  await composer.fill("disconnect-cancel-flow");
  await composer.press("Enter");
  await expect(page.getByRole("button", { name: "Stop current turn" })).toBeVisible();

  const picker = page.locator(".session-switcher");
  await openSessionPicker(page);
  const earlier = picker.locator(".session-open").filter({ hasText: "Earlier Agent thread" });
  await expect(earlier).toBeEnabled();
  await earlier.click();
  await expect(page.getByRole("heading", { name: "Earlier Agent thread" })).toBeVisible();
  await expect(composer).toBeEnabled();

  await composer.fill("usage-flow");
  await composer.press("Enter");
  await expect(page.getByText("max_tokens", { exact: true })).toBeVisible();

  await openSessionPicker(page);
  const saved = picker.locator(".session-open").filter({ hasText: "Saved ACP session" });
  await saved.click();
  await expect(page.getByRole("heading", { name: "Saved ACP session" })).toBeVisible();
  await expect(page.getByRole("button", { name: "Stop current turn" })).toBeVisible();
  await openSessionPicker(page);
  await expect(picker.getByRole("button", { name: "Delete Saved ACP session" })).toBeDisabled();
  await page.keyboard.press("Escape");
  await page.getByRole("button", { name: "Stop current turn" }).click();
  await expect(composer).toBeEnabled();
  expect(browserErrors).toEqual([]);
});

test("closes then deletes both switched-away and current ACP sessions", async ({ page }) => {
  const server = await startRustTestServer({
    cwd: process.cwd(),
    command: [
      process.execPath,
      "--import",
      "tsx",
      join(process.cwd(), "tests/fixtures/fake-agent.ts"),
      "--require-close-before-delete",
    ],
  });
  const browserErrors = collectBrowserErrors(page);

  try {
    await page.goto(`http://127.0.0.1:${server.port}/sessions/saved-session`);
    const picker = page.locator(".session-switcher");
    await expect(page.getByRole("heading", { name: "Saved ACP session" })).toBeVisible();

    await openSessionPicker(page);
    await picker.locator(".session-open")
      .filter({ hasText: "Earlier Agent thread" })
      .click();
    await expect(page.getByRole("heading", { name: "Earlier Agent thread" })).toBeVisible();
    await openSessionPicker(page);
    const oldDelete = picker.getByRole("button", { name: "Delete Saved ACP session" });
    await expect(oldDelete).toBeEnabled();
    page.once("dialog", (dialog) => dialog.accept());
    await oldDelete.click();
    await expect(picker.getByText("Saved ACP session", { exact: true })).toBeHidden();
    await expect(page).toHaveURL(/\/sessions\/earlier-session$/u);
    await expect(page.getByRole("alert")).toHaveCount(0);

    await page.keyboard.press("Escape");
    await page.getByRole("button", { name: "Thread actions" }).click();
    page.once("dialog", (dialog) => dialog.accept());
    await page.getByRole("button", { name: "Delete thread" }).click();
    await expect(page).toHaveURL(/\/$/u);
    await expect(page.getByRole("heading", { name: "Projects", exact: true })).toBeVisible();
    await expect(picker.getByText("Earlier Agent thread", { exact: true })).toBeHidden();
    await expect(page.getByRole("alert")).toHaveCount(0);
    expect(browserErrors).toEqual([]);
  } finally {
    await server.close();
  }
});

test("keeps expanded ACP turn payloads inside the message flow", async ({ page }) => {
  const browserErrors = collectBrowserErrors(page);
  await page.goto("/sessions/saved-session");

  const composer = page.locator('textarea[role="combobox"]');
  await expect(composer).toBeEnabled();
  await composer.fill("usage-flow");
  await composer.press("Enter");

  const payload = page.locator("details.raw-json").filter({ hasText: "Turn response" });
  await expect(payload).toBeVisible();
  await payload.locator("summary").click();
  await expect(payload.locator("pre")).toContainText('"stopReason": "max_tokens"');
  await expect(payload.locator("pre")).toContainText('"totalTokens": 21');
  expect(await horizontalOverflow(page)).toBeLessThanOrEqual(1);

  await page.setViewportSize({ width: 390, height: 844 });
  await expect(payload.locator("pre")).toBeVisible();
  expect(await horizontalOverflow(page)).toBeLessThanOrEqual(1);
  expect(browserErrors).toEqual([]);
});

test("renders live ACP context usage at the Zed-style composer edge", async ({ page }) => {
  const browserErrors = collectBrowserErrors(page);
  await page.goto("/sessions/saved-session");

  const composer = page.locator('textarea[role="combobox"]');
  await expect(composer).toBeEnabled();
  await expect(page.getByRole("button", { name: /Context usage:/ })).toHaveCount(0);

  await composer.fill("context-window-flow");
  await composer.press("Enter");
  await expect(page.getByText("Context usage updated.", { exact: true })).toBeVisible();
  const usage = page.getByRole("button", { name: /Context usage: 82%/ });
  await expect(usage).toHaveClass(/warning/);
  await usage.click();
  let details = page.getByRole("region", { name: "ACP context usage" });
  await expect(details).toContainText("82,000 tokens");
  await expect(details).toContainText("18,000 tokens");
  await expect(details).toContainText("1.2345 USD");
  await expect(details).toContainText("Reported by the Agent via ACP usage_update");
  await page.keyboard.press("Escape");
  await expect(details).toBeHidden();
  await expect(usage).toBeFocused();

  await usage.click();
  details = page.getByRole("region", { name: "ACP context usage" });

  await page.setViewportSize({ width: 390, height: 844 });
  await expect(details).toBeVisible();
  expect(await horizontalOverflow(page)).toBeLessThanOrEqual(1);
  expect(browserErrors).toEqual([]);
});

test("expands the same ACP composer with Zed's composer shortcut", async ({ page }) => {
  const browserErrors = collectBrowserErrors(page);
  await page.setViewportSize({ width: 900, height: 640 });
  await page.goto("/sessions/saved-session");

  const editor = page.locator('textarea[role="combobox"]');
  const composer = page.locator(".composer");
  await expect(editor).toBeEnabled();
  await editor.fill("Keep this ACP draft\nwith multiple lines.");
  await editor.press("Alt+Shift+Escape");
  await expect(composer).toHaveClass(/composer-expanded/);
  await expect(composer).toHaveAttribute("data-expanded", "true");
  await expect(editor).toHaveValue("Keep this ACP draft\nwith multiple lines.");
  await expect(editor).toBeFocused();
  await expect(page.getByRole("button", { name: "Collapse message composer" })).toBeVisible();

  const desktopBounds = await composer.boundingBox();
  expect(desktopBounds).not.toBeNull();
  expect(desktopBounds!.y).toBeGreaterThanOrEqual(67);
  expect(desktopBounds!.y + desktopBounds!.height).toBeLessThanOrEqual(640);
  await page.getByRole("button", { name: "Add ACP resource link" }).click();
  await expect(page.getByLabel("Resource URI")).toBeVisible();
  await expect(page.getByLabel("Resource name")).toBeVisible();

  await editor.focus();
  await editor.press("Alt+Shift+Escape");
  await expect(composer).not.toHaveClass(/composer-expanded/);
  await expect(editor).toHaveValue("Keep this ACP draft\nwith multiple lines.");
  await expect(editor).toBeFocused();

  await page.setViewportSize({ width: 390, height: 844 });
  await editor.press("Alt+Shift+Escape");
  await expect(composer).toHaveClass(/composer-expanded/);
  const mobileBounds = await composer.boundingBox();
  expect(mobileBounds).not.toBeNull();
  expect(mobileBounds!.x).toBeGreaterThanOrEqual(9);
  expect(mobileBounds!.x + mobileBounds!.width).toBeLessThanOrEqual(381);
  expect(mobileBounds!.y + mobileBounds!.height).toBeLessThanOrEqual(844);

  await editor.fill("browser permission flow");
  await editor.press("Enter");
  const permission = page.getByRole("alertdialog", { name: "Agent permission request" });
  await expect(permission).toBeVisible();
  await expect(composer).not.toHaveClass(/composer-expanded/);
  await expect(page.getByRole("button", { name: "Expand message composer" })).toBeDisabled();
  const allow = permission.getByRole("button", { name: "Allow once" });
  await expect(allow).toBeFocused();
  await allow.click();
  await expect(permission).toBeHidden();
  await expect(page.getByRole("button", { name: "Expand message composer" })).toBeEnabled();
  expect(await horizontalOverflow(page)).toBeLessThanOrEqual(1);
  expect(browserErrors).toEqual([]);
});

test("reviews Agent-reported ACP diffs without inventing editor actions", async ({ page }) => {
  const browserErrors = collectBrowserErrors(page);
  await page.goto("/sessions/saved-session");

  const composer = page.locator('textarea[role="combobox"]');
  await expect(composer).toBeEnabled();
  await composer.fill("review-flow");
  await composer.press("Enter");
  await expect(page.getByText("Reported two workspace changes.", { exact: true })).toBeVisible();

  const review = page.getByLabel("Agent-reported changes");
  const trigger = review.locator(".change-review-trigger");
  await expect(trigger).toContainText("2 files");
  await expect(trigger).toContainText("+4");
  await expect(trigger).toContainText("−2");
  await expect(trigger).toHaveAttribute("aria-expanded", "false");
  await trigger.click();

  await expect(review.getByText("Agent-reported ACP diffs", { exact: true })).toBeVisible();
  await expect(review).toContainText("Read-only");
  await expect(review).toContainText("src/config.ts");
  await expect(review).toContainText('export const theme = "light";');
  await expect(review).toContainText('export const theme = "zed";');
  await expect(review.getByRole("button", { name: /Accept|Reject|Restore/ })).toHaveCount(0);
  const editTool = page.locator(".tool-card").filter({ hasText: "Edit workspace files" });
  await expect(editTool).toHaveAttribute("data-open", "false");
  await expect(editTool).toHaveAttribute("data-tool-status", "completed");
  await expect(editTool.locator(".tool-status")).toHaveAttribute(
    "aria-label",
    "Tool status: Completed",
  );
  await editTool.locator(":scope > .tool-card-header .tool-disclosure").click();
  await expect(editTool).toHaveAttribute("data-open", "true");
  expect(await horizontalOverflow(page)).toBeLessThanOrEqual(1);

  await page.setViewportSize({ width: 390, height: 844 });
  await expect(review.locator(".change-review-panel")).toBeVisible();
  expect(await horizontalOverflow(page)).toBeLessThanOrEqual(1);
  expect(browserErrors).toEqual([]);
});

test("keeps file changes with their original turns across later prompts", async ({ page }) => {
  const browserErrors = collectBrowserErrors(page);
  await page.goto("/sessions/saved-session");
  const thread = page.getByRole("region", { name: "Conversation thread" });
  const composer = page.locator('textarea[role="combobox"]');
  const reviews = thread.getByLabel("Agent-reported changes");
  await expect(composer).toBeEnabled();
  await composer.fill("review-flow first edit");
  await composer.press("Enter");
  await expect(reviews).toHaveCount(1);
  const firstReview = reviews.first();
  await firstReview.locator(".change-review-trigger").click();
  await expect(firstReview.locator(".change-review-panel")).toBeVisible();

  await composer.fill("stream-follow-flow without edits");
  await composer.press("Enter");
  await expect(page.getByText("Stream follow complete.", { exact: true })).toBeVisible();
  await expect(reviews).toHaveCount(1);
  await expect(firstReview.locator(".change-review-trigger")).toHaveAttribute("aria-expanded", "true");

  await composer.fill("review-flow second edit");
  await composer.press("Enter");
  await expect(page.getByText("Reported two workspace changes.", { exact: true })).toHaveCount(2);
  await expect(reviews).toHaveCount(2);
  await expect(firstReview.locator(".change-review-trigger")).toHaveAttribute("aria-expanded", "true");
  await expect(reviews.last().locator(".change-review-trigger")).toHaveAttribute("aria-expanded", "false");
  await expect(thread.locator(".tool-card").filter({ hasText: "Edit workspace files" })).toHaveCount(2);
  await expect(page.locator(".composer-dock .change-review")).toHaveCount(0);
  for (const review of await reviews.all()) {
    await expect(review.locator(".change-review-trigger")).toContainText("2 files");
    await expect(review.locator(".change-review-trigger")).toContainText("+4");
  }
  const firstPanelId = await firstReview.locator(".change-review-trigger").getAttribute("aria-controls");
  const secondPanelId = await reviews.last().locator(".change-review-trigger").getAttribute("aria-controls");
  expect(firstPanelId).not.toBe(secondPanelId);
  expect(await thread.evaluate((element) => {
    const changes = [...element.querySelectorAll(".change-review")];
    const prompts = [...element.querySelectorAll('[data-thread-role="user"]')];
    const firstPrompt = prompts.find((prompt) => prompt.textContent?.includes("review-flow first edit"))!;
    const plainPrompt = prompts.find((prompt) => prompt.textContent?.includes("stream-follow-flow without edits"))!;
    const secondPrompt = prompts.find((prompt) => prompt.textContent?.includes("review-flow second edit"))!;
    const before = (left: Element, right: Element) => Boolean(
      left.compareDocumentPosition(right) & Node.DOCUMENT_POSITION_FOLLOWING
    );
    return before(firstPrompt, changes[0]) && before(changes[0], plainPrompt) &&
      before(plainPrompt, secondPrompt) && before(secondPrompt, changes[1]);
  })).toBe(true);
  expect(browserErrors).toEqual([]);
});

test("follows ACP thought and tool activity with responsive Zed-style disclosure", async ({ page }) => {
  const browserErrors = collectBrowserErrors(page);
  await page.goto("/sessions/saved-session");

  const composer = page.locator('textarea[role="combobox"]');
  await expect(composer).toBeEnabled();
  await composer.fill("activity-flow");
  await composer.press("Enter");

  const thinking = page.locator(".thinking-block").filter({ hasText: "Inspecting the requested task." });
  await expect(thinking).toHaveAttribute("data-live", "true");
  await expect(thinking).toHaveAttribute("data-open", "true");
  await expect(page.getByRole("status")).toContainText("Agent is thinking");

  await expect(page.getByRole("status")).toContainText("Running Inspect workspace");
  const tool = page.locator(".tool-card").filter({ hasText: "Inspect workspace" });
  await expect(tool).toHaveAttribute("data-live", "true");
  await expect(tool).toHaveAttribute("data-tool-status", "in_progress");
  await expect(tool.locator(".tool-status")).toHaveAttribute(
    "aria-label",
    "Tool status: Running",
  );
  await expect(tool).toHaveAttribute("data-open", "false");
  await expect(thinking).toHaveAttribute("data-live", "false");
  await expect(thinking).toHaveAttribute("data-open", "false");

  await expect(page.getByText("Activity flow complete.", { exact: true })).toBeVisible();
  await expect(tool).toHaveAttribute("data-live", "false");
  await expect(tool).toHaveAttribute("data-tool-status", "completed");
  await expect(tool.locator(".tool-status")).toHaveAttribute(
    "aria-label",
    "Tool status: Completed",
  );
  await expect(tool).toHaveAttribute("data-open", "false");
  await expect(tool.locator(":scope > .tool-card-header .tool-title strong")).toHaveText(
    "Inspect workspace dependencies and generated configuration files",
  );
  await expect(tool.locator(":scope > .tool-card-header .tool-title strong")).toHaveAttribute(
    "title",
    "Inspect workspace dependencies and generated configuration files",
  );
  await expect(
    tool.locator(":scope > .tool-card-header").getByRole("button", { name: "Tool info" }),
  ).toHaveCount(0);
  await expect(tool.locator(":scope > .tool-body")).toBeHidden();
  await tool.locator(":scope > .tool-card-header .tool-disclosure").click();
  await expect(tool).toHaveAttribute("data-open", "true");
  await expect(tool.locator(".tool-description")).toHaveCount(0);
  await expect(tool.locator(".tool-input")).toContainText("path");
  await expect(tool.locator(".tool-input")).toContainText("/workspace");
  await expect(tool.locator(".tool-output")).toContainText("dependencies");
  await expect(tool.locator(".tool-output")).toContainText("12");
  await expect(tool.locator(".tool-input > .structured-data")).toBeVisible();
  await expect(tool.locator(".tool-output > .structured-data")).toBeVisible();
  const [inputAppearance, outputAppearance] = await Promise.all([
    tool.locator(".tool-input > .structured-data").evaluate((element) => ({
      background: getComputedStyle(element).backgroundColor,
      border: getComputedStyle(element).borderColor,
    })),
    tool.locator(".tool-output > .structured-data").evaluate((element) => ({
      background: getComputedStyle(element).backgroundColor,
      border: getComputedStyle(element).borderColor,
    })),
  ]);
  expect(outputAppearance).toEqual(inputAppearance);
  expect(inputAppearance.background).not.toBe("rgba(0, 0, 0, 0)");
  const toolInfo = tool.locator(":scope > .tool-body .component-debug-meta")
    .getByRole("button", { name: "Tool info" });
  const toolDebug = tool.locator('[aria-label="Tool debug information"]');
  expect(await tool.evaluate((element) => {
    const business = element.querySelector<HTMLElement>(".tool-output")!.getBoundingClientRect();
    const meta = element.querySelector<HTMLElement>(".component-debug-meta")!.getBoundingClientRect();
    const button = element.querySelector<HTMLElement>('[aria-label="Tool info"]')!
      .getBoundingClientRect();
    return {
      belowBusinessContent: meta.top >= business.bottom,
      leftAligned: Math.abs(button.left - meta.left) <= 1,
    };
  })).toEqual({ belowBusinessContent: true, leftAligned: true });
  await expect(toolDebug).toBeHidden();
  await toolInfo.click();
  await expect(tool).toHaveAttribute("data-open", "true");
  await expect(toolDebug).toBeVisible();
  await expect(toolDebug).toContainText("activity-tool");
  await expect(toolDebug).toContainText("Message events");
  await expect(toolDebug.locator("pre")).toContainText('"sessionUpdate": "tool_call"');
  await expect(toolDebug.locator("pre")).toContainText('"status": "completed"');
  await expect(toolDebug.locator(".raw-json")).toHaveCount(0);
  await expect(
    thinking.locator(":scope > .thinking-header")
      .getByRole("button", { name: "Thinking info" }),
  ).toHaveCount(0);
  await thinking.locator(":scope > .thinking-header .thinking-disclosure").click();
  await expect(thinking).toHaveAttribute("data-open", "true");
  const thinkingInfo = thinking.locator(":scope > .thinking-body .component-debug-meta")
    .getByRole("button", { name: "Thinking info" });
  const thinkingDebug = thinking.locator('[aria-label="Thinking debug information"]');
  expect(await thinking.evaluate((element) => {
    const business = element.querySelector<HTMLElement>(".thinking-content")!.getBoundingClientRect();
    const meta = element.querySelector<HTMLElement>(".component-debug-meta")!.getBoundingClientRect();
    const button = element.querySelector<HTMLElement>('[aria-label="Thinking info"]')!
      .getBoundingClientRect();
    return {
      belowBusinessContent: meta.top >= business.bottom,
      leftAligned: Math.abs(button.left - meta.left) <= 1,
    };
  })).toEqual({ belowBusinessContent: true, leftAligned: true });
  await expect(thinkingDebug).toBeHidden();
  await thinkingInfo.click();
  await expect(thinking).toHaveAttribute("data-open", "true");
  await expect(thinkingDebug).toBeVisible();
  await expect(thinkingDebug).toContainText("activity-thought");
  await expect(thinkingDebug.locator("pre")).toContainText("agent_thought_chunk");
  await expect(thinkingDebug.locator(".raw-json")).toHaveCount(0);
  const userMessage = page.locator(".message-user").filter({ hasText: "activity-flow" });
  for (const width of [390, 320]) {
    await page.setViewportSize({ width, height: 844 });
    await tool.locator(":scope > .tool-card-header .tool-disclosure").click();
    await expect(tool).toHaveAttribute("data-open", "false");
    await expect(tool.locator(".tool-status")).toBeVisible();
    const layout = await toolSummaryLayout(tool);
    expect(layout.title.right).toBeLessThanOrEqual(layout.actions.left);
    expect(layout.actions.right).toBeLessThanOrEqual(layout.summary.right - 7);
    expect(layout.summary.height).toBeLessThanOrEqual(40);
    expect(layout.titleLineHeight).toBeLessThanOrEqual(16);
    expect(layout.titleTextOverflow).toBe("ellipsis");
    const radii = await Promise.all([
      userMessage.evaluate((element) => getComputedStyle(element).borderRadius),
      thinking.evaluate((element) => getComputedStyle(element).borderRadius),
      tool.evaluate((element) => getComputedStyle(element).borderRadius),
    ]);
    expect(radii).toEqual(["7px", "7px", "7px"]);
    const disclosureHeights = await Promise.all([
      thinking.locator(":scope > .thinking-header").evaluate((element) => element.getBoundingClientRect().height),
      tool.locator(":scope > .tool-card-header").evaluate((element) => element.getBoundingClientRect().height),
    ]);
    expect(Math.abs(disclosureHeights[0] - disclosureHeights[1])).toBeLessThanOrEqual(1);
    await tool.locator(":scope > .tool-card-header .tool-disclosure").click();
    await expect(tool).toHaveAttribute("data-open", "true");
    const expandedLayout = await toolSummaryLayout(tool);
    expect(expandedLayout.title.right).toBeLessThanOrEqual(expandedLayout.actions.left);
    expect(expandedLayout.titleLineHeight).toBeGreaterThan(16);
    expect(expandedLayout.titleOverflow).toBe(false);
    expect(expandedLayout.titleHeightOverflow).toBe(false);
    expect(await horizontalOverflow(page)).toBeLessThanOrEqual(1);
  }
  expect(browserErrors).toEqual([]);
});

test("uses one visual language for structured tool input and Markdown tool output", async ({ page }) => {
  const browserErrors = collectBrowserErrors(page);
  await page.goto("/sessions/saved-session");

  const composer = page.locator('textarea[role="combobox"]');
  await expect(composer).toBeEnabled();
  await composer.fill("tool-content-flow");
  await composer.press("Enter");
  await expect(page.getByText("Formatted tool content complete.", { exact: true })).toBeVisible();

  const tool = page.locator(".tool-card").filter({ hasText: "Inspect formatted tool output" });
  await expect(tool).toHaveAttribute("data-tool-status", "completed");
  await tool.locator(":scope > .tool-card-header .tool-disclosure").click();

  const input = tool.locator(".tool-input > .structured-data");
  const output = tool.locator(".tool-output .structured-markdown");
  await expect(input).toBeVisible();
  await expect(output).toBeVisible();
  await expect(output.locator("strong")).toHaveText("2 matches");
  await expect(output.locator("li")).toHaveText(["package.json", "Cargo.toml"]);
  const [inputAppearance, outputAppearance] = await Promise.all([input, output].map((locator) => locator.evaluate((element) => ({
    background: getComputedStyle(element).backgroundColor,
    border: getComputedStyle(element).borderColor,
    radius: getComputedStyle(element).borderRadius,
  }))));
  expect(outputAppearance).toEqual(inputAppearance);
  expect(inputAppearance.background).not.toBe("rgba(0, 0, 0, 0)");
  expect(inputAppearance.radius).toBe("6px");
  expect(browserErrors).toEqual([]);
});

test("keeps wide tool tables scrollable and long resource names fully readable", async ({ page }) => {
  const browserErrors = collectBrowserErrors(page);
  await page.goto("/sessions/saved-session");
  const composer = page.locator('textarea[role="combobox"]');
  await expect(composer).toBeEnabled();
  await composer.fill("tool-layout-flow");
  await composer.press("Enter");
  await expect(page.getByText("Wide tool results complete.", { exact: true })).toBeVisible();
  const tool = page.locator(".tool-card").filter({ hasText: "Inspect wide tool results" });
  await tool.locator(":scope > .tool-card-header .tool-disclosure").click();
  const markdown = tool.locator(".structured-markdown > .markdown");
  const resource = tool.locator(".resource-card");
  await expect(markdown.locator("thead th")).toHaveCount(20);
  await expect(resource.locator("strong")).toHaveText("x".repeat(300));

  for (const width of [1280, 320]) {
    await page.setViewportSize({ width, height: 844 });
    await expect(markdown).toHaveCSS("overflow-x", "auto");
    const tableLayout = await markdown.evaluate((element) => {
      element.scrollLeft = element.scrollWidth;
      const lastCell = element.querySelector("tbody td:last-child")!.getBoundingClientRect();
      const viewport = element.getBoundingClientRect();
      return {
        clientWidth: element.clientWidth,
        scrollWidth: element.scrollWidth,
        scrollLeft: element.scrollLeft,
        lastCellLeft: lastCell.left,
        lastCellRight: lastCell.right,
        viewportLeft: viewport.left,
        viewportRight: viewport.right,
      };
    });
    if (width === 320) {
      expect(tableLayout.scrollWidth).toBeGreaterThan(tableLayout.clientWidth);
      expect(tableLayout.scrollLeft).toBeGreaterThan(0);
    }
    expect(Math.abs(tableLayout.scrollLeft - (tableLayout.scrollWidth - tableLayout.clientWidth)))
      .toBeLessThanOrEqual(1);
    expect(tableLayout.lastCellLeft).toBeGreaterThanOrEqual(tableLayout.viewportLeft - 1);
    expect(tableLayout.lastCellRight).toBeLessThanOrEqual(tableLayout.viewportRight + 1);

    const resourceLayout = await resource.evaluate((element) => {
      const name = element.querySelector("strong")!;
      const text = document.createRange();
      text.selectNodeContents(name);
      return {
        width: name.clientWidth,
        contentWidth: name.scrollWidth,
        height: name.clientHeight,
        contentHeight: name.scrollHeight,
        nameBottom: name.getBoundingClientRect().bottom,
        cardBottom: element.getBoundingClientRect().bottom,
        renderedLines: text.getClientRects().length,
      };
    });
    expect(resourceLayout.contentWidth).toBeLessThanOrEqual(resourceLayout.width + 1);
    expect(resourceLayout.contentHeight).toBeLessThanOrEqual(resourceLayout.height + 1);
    expect(resourceLayout.nameBottom).toBeLessThanOrEqual(resourceLayout.cardBottom);
    expect(resourceLayout.renderedLines).toBeGreaterThan(1);
    expect(await horizontalOverflow(page)).toBeLessThanOrEqual(1);
  }
  expect(browserErrors).toEqual([]);
});

test("stops following streamed Agent output after the user scrolls upward", async ({ page }) => {
  const browserErrors = collectBrowserErrors(page);
  await page.setViewportSize({ width: 900, height: 480 });
  await page.goto("/sessions/saved-session");
  await page.addStyleTag({ content: ".conversation-wrap{min-height:1500px!important}" });

  const thread = page.getByRole("region", { name: "Conversation thread" });
  const composer = page.locator('textarea[role="combobox"]');
  await thread.evaluate((element) => {
    element.scrollTop = element.scrollHeight;
  });
  await composer.fill("activity-flow");
  await composer.press("Enter");
  await expect(page.getByText("Inspecting the requested task.", { exact: true })).toBeVisible();

  await thread.evaluate((element) => {
    element.dispatchEvent(new WheelEvent("wheel", {
      bubbles: true,
      deltaY: -120,
    }));
    element.scrollTop = Math.max(0, element.scrollHeight - element.clientHeight - 120);
  });
  await expect.poll(() => thread.evaluate((element) =>
    element.scrollHeight - element.clientHeight - element.scrollTop
  )).toBeGreaterThan(100);

  await expect(page.getByText("Activity flow complete.", { exact: true })).toBeVisible();
  await expect.poll(() => thread.evaluate((element) =>
    element.scrollHeight - element.clientHeight - element.scrollTop
  )).toBeGreaterThan(100);
  const toBottom = page.getByRole("button", { name: "Jump to bottom of thread" });
  await expect(toBottom).toBeEnabled();
  await toBottom.click();
  await expect.poll(() => thread.evaluate((element) =>
    element.scrollHeight - element.clientHeight - element.scrollTop
  )).toBeLessThan(3);

  await composer.fill("activity-flow after returning to bottom");
  await composer.press("Enter");
  await expect(page.getByText("Activity flow complete.", { exact: true })).toHaveCount(2);
  await expect.poll(() => thread.evaluate((element) =>
    element.scrollHeight - element.clientHeight - element.scrollTop
  )).toBeLessThan(3);
  expect(browserErrors).toEqual([]);
});

for (const gesture of ["wheel", "touch"] as const) {
  test(`follows a new turn after a downward ${gesture} gesture at the bottom`, async ({ page }) => {
    const browserErrors = collectBrowserErrors(page);
    await page.setViewportSize({ width: 900, height: 420 });
    await page.goto("/sessions/saved-session");

    const thread = page.getByRole("region", { name: "Conversation thread" });
    const composer = page.locator('textarea[role="combobox"]');
    await expect(composer).toBeEnabled();
    await composer.fill("stream-follow-flow");
    await composer.press("Enter");
    await expect(page.getByText("Stream follow complete.", { exact: true })).toBeVisible();
    await expect(page.getByRole("button", { name: "Send prompt", exact: true })).toBeVisible();
    await expect.poll(() => thread.evaluate((element) =>
      element.scrollHeight - element.clientHeight - element.scrollTop
    )).toBeLessThan(3);

    await thread.evaluate((element, input) => {
      if (input === "wheel") {
        element.dispatchEvent(new WheelEvent("wheel", { bubbles: true, deltaY: 120 }));
      } else {
        for (const [type, clientY] of [["touchstart", 200], ["touchmove", 80]] as const) {
          element.dispatchEvent(new TouchEvent(type, {
            bubbles: true,
            touches: [new Touch({ identifier: 1, target: element, clientY })],
          }));
        }
        element.dispatchEvent(new TouchEvent("touchend", { bubbles: true, touches: [] }));
      }
    }, gesture);

    await composer.fill("stream-follow-flow next turn");
    await composer.press("Enter");
    await expect(page.getByText("Stream follow complete.", { exact: true })).toHaveCount(2);
    await expect.poll(() => thread.evaluate((element) =>
      element.scrollHeight - element.clientHeight - element.scrollTop
    )).toBeLessThan(3);
    expect(browserErrors).toEqual([]);
  });
}

test("follows delayed content growth only while pinned to the bottom", async ({ page }) => {
  const browserErrors = collectBrowserErrors(page);
  await page.setViewportSize({ width: 900, height: 420 });
  await page.goto("/sessions/saved-session");
  const thread = page.getByRole("region", { name: "Conversation thread" });
  const composer = page.locator('textarea[role="combobox"]');
  await expect(composer).toBeEnabled();
  await composer.fill("stream-follow-flow");
  await composer.press("Enter");
  await expect(page.getByText("Stream follow complete.", { exact: true })).toBeVisible();
  await expect(page.getByRole("button", { name: "Send prompt", exact: true })).toBeVisible();

  // Simulate a late image/terminal layout update with no new ACP timeline event.
  await thread.evaluate((element) => {
    const content = element.querySelector<HTMLElement>(".conversation-wrap")!;
    content.style.paddingBottom = "300px";
  });
  await expect.poll(() => thread.evaluate((element) =>
    element.scrollHeight - element.clientHeight - element.scrollTop
  )).toBeLessThan(3);

  // Reading inside a nested output pane does not move the conversation itself.
  await thread.evaluate((element) => {
    const output = document.createElement("pre");
    output.dataset.nestedOutput = "true";
    output.style.cssText = "height:80px;overflow-y:auto";
    output.textContent = "Tool output\n".repeat(80);
    element.querySelector(".conversation-wrap")!.append(output);
    output.scrollTop = 100;
  });
  await expect.poll(() => thread.evaluate((element) =>
    element.scrollHeight - element.clientHeight - element.scrollTop
  )).toBeLessThan(3);
  await thread.evaluate((element) => {
    const output = element.querySelector<HTMLElement>("[data-nested-output]")!;
    output.dispatchEvent(new WheelEvent("wheel", { bubbles: true, deltaY: -50 }));
    output.scrollTop -= 50;
    element.querySelector<HTMLElement>(".conversation-wrap")!.style.paddingBottom = "600px";
  });
  await expect.poll(() => thread.evaluate((element) =>
    element.scrollHeight - element.clientHeight - element.scrollTop
  )).toBeLessThan(3);

  await thread.evaluate((element) => {
    element.dispatchEvent(new WheelEvent("wheel", { bubbles: true, deltaY: -150 }));
    element.scrollTop -= 150;
  });
  const readingPosition = await thread.evaluate((element) => element.scrollTop);
  await thread.evaluate((element) => {
    element.querySelector<HTMLElement>(".conversation-wrap")!.style.paddingBottom = "900px";
  });
  await expect.poll(() => thread.evaluate((element) =>
    element.scrollHeight - element.clientHeight - element.scrollTop
  )).toBeGreaterThan(400);
  expect(await thread.evaluate((element) => element.scrollTop)).toBe(readingPosition);
  expect(browserErrors).toEqual([]);
});

for (const distance of [24, 500]) {
  test(`preserves the reading position when sending ${distance}px above the bottom`, async ({ page }) => {
    const browserErrors = collectBrowserErrors(page);
    await page.setViewportSize({ width: 900, height: 420 });
    await page.goto("/sessions/saved-session");
    const thread = page.getByRole("region", { name: "Conversation thread" });
    const composer = page.locator('textarea[role="combobox"]');
    await expect(composer).toBeEnabled();
    await composer.fill("stream-follow-flow");
    await composer.press("Enter");
    await expect(page.getByText("Stream follow complete.", { exact: true })).toBeVisible();
    await expect(page.getByRole("button", { name: "Send prompt", exact: true })).toBeVisible();
    await thread.focus();
    await thread.evaluate((element, offset) => {
      element.dispatchEvent(new WheelEvent("wheel", { bubbles: true, deltaY: -offset }));
      element.scrollTop = element.scrollHeight - element.clientHeight - offset;
    }, distance);
    const readingPosition = await thread.evaluate((element) => element.scrollTop);
    const anchor = thread.getByText("Streamed paragraph 18:", { exact: false }).first();
    const anchorTop = await anchor.evaluate((element) => element.getBoundingClientRect().top);

    await composer.fill("stream-follow-flow while reading history");
    await composer.press("Enter");
    await expect(page.getByText("Stream follow complete.", { exact: true })).toHaveCount(2);
    await expect(page.getByRole("button", { name: "Send prompt", exact: true })).toBeVisible();
    expect(await thread.evaluate((element) => element.scrollTop)).toBe(readingPosition);
    expect(await anchor.evaluate((element) => element.getBoundingClientRect().top)).toBe(anchorTop);
    expect(browserErrors).toEqual([]);
  });
}

test("keeps the reading position after scrolling up during streamed output", async ({ page }) => {
  const browserErrors = collectBrowserErrors(page);
  await page.setViewportSize({ width: 900, height: 420 });
  await page.goto("/sessions/saved-session");
  const thread = page.getByRole("region", { name: "Conversation thread" });
  const composer = page.locator('textarea[role="combobox"]');
  await expect(composer).toBeEnabled();
  await composer.fill("stream-follow-flow");
  await composer.press("Enter");
  await expect(page.getByText("Stream follow complete.", { exact: true })).toBeVisible();
  await composer.fill("stream-follow-flow next turn");
  await composer.press("Enter");
  await expect(thread.getByText("Streamed paragraph 4:", { exact: false })).toHaveCount(2);
  const readingPosition = await thread.evaluate((element) => {
    element.dispatchEvent(new WheelEvent("wheel", { bubbles: true, deltaY: -500 }));
    element.scrollTop -= 500;
    return element.scrollTop;
  });
  const anchor = thread.getByText("Streamed paragraph 18:", { exact: false }).first();
  const anchorTop = await anchor.evaluate((element) => element.getBoundingClientRect().top);
  await expect(page.getByText("Stream follow complete.", { exact: true })).toHaveCount(2);
  await expect(page.getByRole("button", { name: "Send prompt", exact: true })).toBeVisible();
  expect(await thread.evaluate((element) => element.scrollTop)).toBe(readingPosition);
  expect(await anchor.evaluate((element) => element.getBoundingClientRect().top)).toBe(anchorTop);
  expect(browserErrors).toEqual([]);
});

test("keeps the historical search position when sending and receiving a later turn", async ({ page }) => {
  const browserErrors = collectBrowserErrors(page);
  await page.setViewportSize({ width: 900, height: 420 });
  await page.goto("/sessions/saved-session");
  const thread = page.getByRole("region", { name: "Conversation thread" });
  const composer = page.locator('textarea[role="combobox"]');
  await expect(composer).toBeEnabled();
  await composer.fill("stream-follow-flow");
  await composer.press("Enter");
  await expect(page.getByText("Stream follow complete.", { exact: true })).toBeVisible();
  await page.getByRole("button", { name: "Search Agent thread" }).click();
  const query = page.getByRole("searchbox", { name: "Search this thread" });
  await query.fill("Loaded history");
  const history = page.getByText("Loaded history.", { exact: true });
  await expect(history).toBeInViewport();
  let readingPosition = -1;
  await expect.poll(async () => {
    const position = await thread.evaluate((element) => element.scrollTop);
    const settled = position === readingPosition;
    readingPosition = position;
    return settled;
  }, { intervals: [100] }).toBe(true);
  await query.press("Escape");
  await expect(composer).toBeFocused();
  await expect(history).toBeInViewport();
  await expect.poll(async () => Math.abs(
    await thread.evaluate((element) => element.scrollTop) - readingPosition
  )).toBeLessThan(1);
  const anchorTop = await history.evaluate((element) => element.getBoundingClientRect().top);
  await composer.fill("stream-follow-flow after search");
  await composer.press("Enter");
  await expect(page.getByText("Stream follow complete.", { exact: true })).toHaveCount(2);
  await expect(page.getByRole("button", { name: "Send prompt", exact: true })).toBeVisible();
  expect(await thread.evaluate((element) => element.scrollTop)).toBe(readingPosition);
  expect(await history.evaluate((element) => element.getBoundingClientRect().top)).toBe(anchorTop);
  expect(browserErrors).toEqual([]);
});

test("keeps bottom following stable during rapid streamed output", async ({ page }) => {
  const browserErrors = collectBrowserErrors(page);
  await page.setViewportSize({ width: 900, height: 420 });
  await page.goto("/sessions/saved-session");

  const thread = page.getByRole("region", { name: "Conversation thread" });
  await expect(thread).toHaveCSS("scroll-behavior", "auto");
  await thread.evaluate((element) => {
    const samples: number[] = [];
    let frame = 0;
    const content = element.querySelector(".conversation-wrap");
    const observer = new MutationObserver(() => {
      cancelAnimationFrame(frame);
      frame = requestAnimationFrame(() => {
        if (element.scrollHeight <= element.clientHeight + 1) return;
        samples.push(element.scrollHeight - element.clientHeight - element.scrollTop);
      });
    });
    if (content) observer.observe(content, { childList: true, characterData: true, subtree: true });
    Object.assign(element, { __bottomFollowSamples: samples, __bottomFollowObserver: observer });
  });

  const composer = page.locator('textarea[role="combobox"]');
  await composer.fill("stream-follow-flow");
  await composer.press("Enter");
  await expect(page.getByText("Stream follow complete.", { exact: true })).toBeVisible();

  const samples = await thread.evaluate((element) => {
    const instrumented = element as HTMLDivElement & {
      __bottomFollowSamples: number[];
      __bottomFollowObserver: MutationObserver;
    };
    instrumented.__bottomFollowObserver.disconnect();
    return instrumented.__bottomFollowSamples;
  });
  expect(samples.length).toBeGreaterThan(3);
  expect(Math.max(...samples)).toBeLessThanOrEqual(2);
  expect(browserErrors).toEqual([]);
});

test("searches the visible ACP Agent thread with Zed-style match navigation", async ({ page }) => {
  const browserErrors = collectBrowserErrors(page);
  await page.goto("/sessions/saved-session");

  const composer = page.locator('textarea[role="combobox"]');
  await expect(composer).toBeEnabled();
  await composer.fill("activity-flow");
  await composer.press("Enter");
  await expect(page.getByText("Activity flow complete.", { exact: true })).toBeVisible();

  const thinking = page.locator(".thinking-block").filter({ hasText: "Thinking" });
  await expect(thinking).toHaveAttribute("data-open", "false");
  await composer.focus();
  await page.keyboard.press("Control+f");
  const search = page.getByRole("search", { name: "Search this Agent thread" });
  const query = search.getByRole("searchbox", { name: "Search this thread" });
  await expect(search).toBeVisible();
  await expect(query).toBeFocused();
  await expect(page.getByRole("button", { name: "Search Agent thread" })).toHaveAttribute("aria-pressed", "true");

  await query.fill("Inspecting");
  await expect(search.locator("output")).toHaveText("0/0");
  await thinking.locator(":scope > .thinking-header .thinking-disclosure").click();
  await expect(search.locator("output")).toHaveText("1/1");
  await expect(thinking).toHaveAttribute("data-thread-search-active", "true");
  await expect.poll(() => page.evaluate(() => {
    const registry = (CSS as typeof CSS & {
      highlights?: Map<string, { size: number }>;
    }).highlights;
    return registry?.get("attyd-thread-search-active")?.size ?? 0;
  })).toBe(1);

  await query.fill("activity");
  await expect(search.locator("output")).toHaveText("1/2");
  await query.press("Enter");
  await expect(search.locator("output")).toHaveText("2/2");
  await expect(page.locator('[data-thread-search-active="true"]'))
    .toContainText("Activity flow complete.");

  await search.getByRole("button", { name: "Use regular expression" }).click();
  await query.fill("(");
  await expect(search.locator("#thread-search-status")).toContainText(
    /regular expression|parenthes|unterminated/i,
  );
  await search.getByRole("button", { name: "Use regular expression" }).click();
  await query.fill("activity");
  await query.press("Escape");
  await expect(search).toBeHidden();
  await expect(composer).toBeFocused();

  await page.setViewportSize({ width: 390, height: 844 });
  await page.getByRole("button", { name: "Search Agent thread" }).click();
  await expect(page.getByRole("search", { name: "Search this Agent thread" })).toBeVisible();
  expect(await horizontalOverflow(page)).toBeLessThanOrEqual(1);
  expect(browserErrors).toEqual([]);
});

test("preserves terminal content beside additional output and searches only expanded content", async ({ page }) => {
  const browserErrors = collectBrowserErrors(page);
  await page.goto("/sessions/saved-session");

  const composer = page.locator('textarea[role="combobox"]');
  await expect(composer).toBeEnabled();
  await composer.fill("terminal-flow");
  await composer.press("Enter");

  const tool = page.locator(".tool-card").filter({ hasText: "Run terminal fixture" });
  await expect(tool).toHaveAttribute("data-tool-status", "completed");
  await expect(tool).toHaveAttribute("data-open", "false");

  await composer.focus();
  await page.keyboard.press("Control+f");
  const search = page.getByRole("search", { name: "Search this Agent thread" });
  const query = search.getByRole("searchbox", { name: "Search this thread" });
  await query.fill("TERMINAL_FLOW_OUTPUT");
  await expect(search.locator("output")).toHaveText("0/0");

  await tool.locator(":scope > .tool-card-header .tool-disclosure").click();
  await expect(tool).toHaveAttribute("data-open", "true");
  const terminal = tool.locator(".terminal-embed");
  await expect(terminal).toBeVisible();
  await expect(terminal.locator(".terminal-heading")).toHaveText("TerminalCompleted");
  await expect(terminal.locator("pre")).toHaveText("TERMINAL_FLOW_OUTPUT");
  const additional = tool.locator(".tool-output details").filter({ hasText: "Additional output" });
  await expect(additional).not.toHaveAttribute("open", "");
  await expect(additional.locator(".structured-data")).toBeHidden();
  await expect(search.locator("output")).toHaveText("1/1");
  await additional.locator("summary").click();
  await expect(additional.locator(".structured-data")).toBeVisible();
  await expect(additional).toContainText("TERMINAL_FLOW_OUTPUT");
  await expect(search.locator("output")).toHaveText("1/2");
  await additional.locator("summary").click();
  await expect(search.locator("output")).toHaveText("1/1");
  await expect(terminal).toBeVisible();

  await tool.getByRole("button", { name: "Tool info" }).click();
  const info = tool.getByRole("region", { name: "Tool debug information" });
  await expect(info).toBeVisible();
  await expect(info).toContainText("Locations");
  await expect(info).toContainText('"terminalId"');
  await expect(search.locator("output")).toHaveText("1/1");
  expect(browserErrors).toEqual([]);
});

test("keeps live terminal output across reconnect and retains released output for every observer", async ({ browser, page }) => {
  const temporaryDirectory = await mkdtemp(join(tmpdir(), "attyd-live-terminal-"));
  const gatePath = join(temporaryDirectory, "finish");
  const browserErrors = collectBrowserErrors(page);
  let observer: Page | undefined;
  let observerErrors: string[] = [];
  const cardFor = (tab: Page) => tab.locator(".tool-card").filter({ hasText: "Run live terminal fixture" });
  const openRunningCard = async (tab: Page) => {
    const card = cardFor(tab);
    await expect(card).toHaveAttribute("data-tool-status", "in_progress");
    await card.locator(":scope > .tool-card-header .tool-disclosure").click();
    await expect(card.locator(".terminal-heading")).toHaveText("TerminalRunning");
    await expect(card.locator(".terminal-embed pre")).toContainText("LIVE_START中😀");
    await expect(card.locator(".terminal-embed pre")).not.toContainText("LIVE_END");
  };

  try {
    await page.goto("/sessions/saved-session");
    const composer = page.locator('textarea[role="combobox"]');
    await expect(composer).toBeEnabled();
    await composer.fill(`terminal-live-flow ${gatePath}`);
    await composer.press("Enter");
    await openRunningCard(page);
    await page.reload();
    await openRunningCard(page);

    observer = await browser.newPage();
    observerErrors = collectBrowserErrors(observer);
    await observer.goto(page.url());
    await openRunningCard(observer);
    await writeFile(gatePath, "finish");
    for (const tab of [page, observer]) {
      const card = cardFor(tab);
      await expect(card).toHaveAttribute("data-tool-status", "completed");
      await expect(card.locator(".terminal-heading")).toHaveText("TerminalCompleted");
      await expect(card.locator(".terminal-embed pre")).toHaveText("LIVE_START中😀\nLIVE_END\n");
      await card.getByRole("button", { name: "Tool info" }).click();
      const terminals = card.locator(".debug-info-entry").filter({
        has: tab.locator(".debug-info-label > span", { hasText: "Terminals" }),
      });
      await expect(terminals).toContainText('"released": true');
    }

    await page.reload();
    const restored = cardFor(page);
    await expect(restored).toHaveAttribute("data-tool-status", "completed");
    await restored.locator(":scope > .tool-card-header .tool-disclosure").click();
    await expect(restored.locator(".terminal-heading")).toHaveText("TerminalCompleted");
    await expect(restored.locator(".terminal-embed pre")).toHaveText("LIVE_START中😀\nLIVE_END\n");
    const additional = restored.locator(".tool-additional-output");
    await expect(additional.locator(".structured-data")).toBeHidden();
    await additional.locator("summary").click();
    await expect(additional.locator("dt")).toHaveText(["result"]);
    await expect(additional.locator("dd")).toHaveText(["agent-output"]);
    expect(browserErrors).toEqual([]);
    expect(observerErrors).toEqual([]);
  } finally {
    await observer?.close();
    await rm(temporaryDirectory, { recursive: true, force: true });
  }
});

test("collapses completed ACP compaction summaries into a Zed-style thread disclosure", async ({ page }) => {
  const browserErrors = collectBrowserErrors(page);
  await page.goto("/sessions/saved-session");

  const composer = page.locator('textarea[role="combobox"]');
  await expect(composer).toBeEnabled();
  await composer.fill("compaction-flow");
  await composer.press("Enter");

  const compaction = page.locator(".compaction-card").filter({ hasText: "Context compacted" });
  await expect(compaction).toBeVisible();
  await expect(compaction).toHaveAttribute("data-compaction-status", "completed");
  await expect(compaction).toHaveAttribute("data-live", "false");
  await expect(compaction).not.toHaveAttribute("open", "");
  await expect(compaction.locator(".compaction-summary")).toBeHidden();
  await expect(compaction.locator(":scope > summary")).toContainText("1 summary block");

  await composer.focus();
  await page.keyboard.press("Control+f");
  const search = page.getByRole("search", { name: "Search this Agent thread" });
  const query = search.getByRole("searchbox", { name: "Search this thread" });
  await query.fill("Compact summary");
  await expect(search.locator("output")).toHaveText("0/0");

  await compaction.locator(":scope > summary").click();
  await expect(compaction).toHaveAttribute("open", "");
  await expect(compaction.locator(".compaction-summary")).toBeVisible();
  await expect(search.locator("output")).toHaveText("1/1");
  await expect(compaction).toHaveAttribute("data-thread-search-active", "true");

  await query.press("Escape");
  await expect(composer).toBeFocused();
  await page.setViewportSize({ width: 390, height: 844 });
  await expect(compaction).toBeVisible();
  expect(await horizontalOverflow(page)).toBeLessThanOrEqual(1);
  expect(browserErrors).toEqual([]);
});

test("offers Zed-style context actions on an ACP Agent response", async ({ page }) => {
  const browserErrors = collectBrowserErrors(page);
  await page.goto("/sessions/saved-session");

  const composer = page.locator('textarea[role="combobox"]');
  await expect(composer).toBeEnabled();
  await composer.fill("message-actions-flow");
  await composer.press("Enter");

  const prompt = page.locator(".message-user").filter({ hasText: "message-actions-flow" });
  const promptInfo = prompt.getByRole("button", { name: "Message info" });
  const edit = prompt.getByRole("button", { name: "Edit and resend user message" });
  const response = page.locator(".assistant-entry").filter({ hasText: "Context menu response." });
  await expect(response).toBeVisible();
  await page.setViewportSize({ width: 390, height: 844 });
  await expect(promptInfo).toHaveCount(1);
  await expect(promptInfo).toHaveText("");
  await expect(edit).toHaveText("");
  expect(await prompt.locator(".message-meta-actions").evaluate((actions) => ({
    justifyContent: getComputedStyle(actions).justifyContent,
    order: [...actions.children].map((element) => element.getAttribute("aria-label")),
  }))).toEqual({
    justifyContent: "flex-start",
    order: ["Message info", "Edit and resend user message"],
  });
  const thinking = response.locator(".thinking-block");
  const answer = response.locator(".assistant-chunk");
  const inlineCopy = response.getByRole("button", { name: "Copy agent response" });
  const answerInfo = answer.getByRole("button", { name: "Message info" });
  await expect(thinking).toBeVisible();
  await expect(inlineCopy).toHaveCount(1);
  await expect(answerInfo).toHaveCount(1);
  await expect(inlineCopy).toHaveText("");
  await expect(answerInfo).toHaveText("");
  expect(await answer.locator(".message-meta-actions").evaluate((actions) => ({
    justifyContent: getComputedStyle(actions).justifyContent,
    order: [...actions.children].map((element) => element.getAttribute("aria-label")),
  }))).toEqual({ justifyContent: "flex-start", order: ["Message info", "Copy agent response"] });
  await answerInfo.click();
  const infoPanel = answer.locator('[aria-label="Message debug information"]');
  await expect(infoPanel).toBeVisible();
  await expect(infoPanel).toContainText("message-actions-answer");
  await expect(infoPanel).toContainText("Message events");
  expect(await answer.locator(".message-meta").evaluate((footer) => {
    const actions = footer.querySelector(".message-meta-actions")!.getBoundingClientRect();
    const debug = footer.querySelector(".debug-info-panel")!.getBoundingClientRect();
    return {
      debugBelowActions: debug.top >= actions.bottom,
      debugWithinMessage: debug.right <= footer.getBoundingClientRect().right + 1,
      position: getComputedStyle(footer.querySelector(".debug-info-panel")!).position,
    };
  })).toEqual({ debugBelowActions: true, debugWithinMessage: true, position: "static" });
  await expect(infoPanel.locator("pre")).toContainText("agent_message_chunk");
  await expect(infoPanel.locator(".raw-json")).toHaveCount(0);
  await answerInfo.click();
  expect(await inlineCopy.evaluate((button) => Boolean(button.closest(".assistant-chunk"))))
    .toBe(true);
  await thinking.locator(":scope > .thinking-header").click({ button: "right" });
  await expect(page.getByRole("menu", { name: "Agent response actions" })).toBeHidden();
  await answer.click({ button: "right" });
  let menu = page.getByRole("menu", { name: "Agent response actions" });
  await expect(menu).toBeVisible();
  await expect(menu.getByRole("menuitem", { name: "Copy This Agent Response" })).toBeFocused();
  await expect(menu.getByRole("menuitem", { name: "Copy Selection" })).toHaveCount(0);
  await page.keyboard.press("Escape");
  await expect(response).toBeFocused();

  await answer.locator(".message-content").evaluate((element) => {
    const selection = window.getSelection();
    const range = document.createRange();
    range.selectNodeContents(element);
    selection?.removeAllRanges();
    selection?.addRange(range);
  });
  await answer.click({ button: "right" });
  menu = page.getByRole("menu", { name: "Agent response actions" });
  await expect(menu.getByRole("menuitem", { name: "Copy Selection" })).toBeVisible();
  await page.keyboard.press("Escape");
  await page.evaluate(() => window.getSelection()?.removeAllRanges());

  await response.focus();
  await page.keyboard.press("Shift+F10");
  menu = page.getByRole("menu", { name: "Agent response actions" });
  await expect(menu).toBeVisible();
  const popupPromise = page.waitForEvent("popup");
  await menu.getByRole("menuitem", { name: "Open Thread as Markdown" }).click();
  const markdown = await popupPromise;
  await markdown.waitForLoadState();
  await expect(markdown.locator("body")).toContainText("Context menu response.");
  await expect(markdown.locator("body")).toContainText("message-actions-answer");
  await markdown.close();

  expect(await horizontalOverflow(page)).toBeLessThanOrEqual(1);
  expect(browserErrors).toEqual([]);
});

test("navigates long threads and opens a faithful Markdown view", async ({ page }) => {
  const browserErrors = collectBrowserErrors(page);
  await page.setViewportSize({ width: 900, height: 480 });
  await page.goto("/sessions/saved-session");

  const composer = page.locator('textarea[role="combobox"]');
  await expect(composer).toBeEnabled();
  await composer.fill("usage-flow");
  await composer.press("Enter");
  await expect(page.getByText("max_tokens", { exact: true })).toBeVisible();

  await page.addStyleTag({ content: ".conversation-wrap{min-height:1500px!important}" });
  const thread = page.getByRole("region", { name: "Conversation thread" });
  const toTop = page.getByRole("button", { name: "Jump to top of thread" });
  const toBottom = page.getByRole("button", { name: "Jump to bottom of thread" });
  await expect(toBottom).toBeVisible();
  await thread.evaluate((element) => element.scrollTo(0, element.scrollHeight));
  await expect(toTop).toBeEnabled();
  await expect(toBottom).toBeDisabled();
  await toTop.click();
  await expect.poll(() => thread.evaluate((element) => element.scrollTop)).toBeLessThan(3);
  await expect(toTop).toBeDisabled();
  await expect(toBottom).toBeEnabled();
  await toBottom.click();
  await expect.poll(() => thread.evaluate((element) =>
    element.scrollHeight - element.clientHeight - element.scrollTop
  )).toBeLessThan(3);
  await expect(toBottom).toBeDisabled();

  await composer.focus();
  await page.setViewportSize({ width: 390, height: 320 });
  await expect.poll(() => thread.evaluate((element) =>
    element.scrollHeight - element.clientHeight - element.scrollTop
  )).toBeLessThan(3);
  await thread.evaluate((element) => {
    element.dispatchEvent(new WheelEvent("wheel", { bubbles: true, deltaY: -100 }));
    element.scrollTop = 0;
  });
  await page.setViewportSize({ width: 390, height: 480 });
  await expect.poll(() => thread.evaluate((element) => element.scrollTop)).toBeLessThan(3);

  await thread.focus();
  await page.keyboard.press("End");
  await expect.poll(() => thread.evaluate((element) =>
    element.scrollHeight - element.clientHeight - element.scrollTop
  )).toBeLessThan(3);
  await composer.focus();
  await expect(composer).toBeFocused();
  await page.keyboard.press("Control+Home");
  await expect.poll(() => thread.evaluate((element) => element.scrollTop)).toBeLessThan(3);

  await page.getByRole("button", { name: "Thread actions" }).click();
  const popupPromise = page.waitForEvent("popup");
  await page.getByRole("button", { name: "Open as Markdown" }).click();
  const markdown = await popupPromise;
  await markdown.waitForLoadState();
  await expect(markdown.locator("body")).toContainText("# Saved ACP session");
  await expect(markdown.locator("body")).toContainText("## You");
  await expect(markdown.locator("body")).toContainText("usage-flow");
  await expect(markdown.locator("body")).toContainText("Turn complete · max_tokens · 21 tokens");
  await markdown.close();

  expect(await horizontalOverflow(page)).toBeLessThanOrEqual(1);
  expect(browserErrors).toEqual([]);
});

for (const storedSessionId of ["stale-session", "earlier-session"]) {
  test(`keeps the homepage unselected despite cached ${storedSessionId}`, async ({ page }) => {
    const browserErrors = collectBrowserErrors(page);
    await page.addInitScript((sessionId) => {
      localStorage.setItem("attyd:last-session-id", sessionId);
    }, storedSessionId);
    const sessionRequests: string[] = [];
    page.on("request", (request) => {
      if (/^\/api\/v1\/sessions\/[^/]+(?:\/events)?$/u.test(new URL(request.url()).pathname)) {
        sessionRequests.push(request.url());
      }
    });

    await page.goto("/");
    await expect(page.locator(".project-card")).toHaveCount(1);
    await expect(page.locator(".session-open")).toHaveCount(0);
    await expect(page.getByRole("heading", { name: "Projects", exact: true })).toBeVisible();
    await expect(page.getByRole("button", { name: /^(?:New thread|New project)$/u })).toBeEnabled();
    await expect(page.locator('.session-history [aria-current="page"]')).toHaveCount(0);
    await expect(page).toHaveURL(/\/$/u);
    expect(sessionRequests).toEqual([]);
    expect(browserErrors).toEqual([]);
  });
}

test("returns a missing session URL to the homepage without an ACP error", async ({ page }) => {
  const browserErrors = collectBrowserErrors(page, [404]);
  await page.goto("/sessions/nonexistent-session");
  await expect(page).toHaveURL(/\/$/u);
  await expect(page.getByRole("heading", { name: "Projects", exact: true })).toBeVisible();
  await expect(page.locator(".project-card")).toHaveCount(1);
  await expect(page.locator(".session-open")).toHaveCount(0);
  await expect(page.locator('.session-history [aria-current="page"]')).toHaveCount(0);
  await expect(page.getByRole("button", { name: /^(?:New thread|New project)$/u })).toBeEnabled();
  await expect(page.getByRole("alert")).toHaveCount(0);
  await page.reload();
  await expect(page.getByRole("heading", { name: "Projects", exact: true })).toBeVisible();
  await expect(page.getByRole("button", { name: /^(?:New thread|New project)$/u })).toBeEnabled();
  expect(browserErrors).toEqual([]);
});

test("keeps the homepage selected when a previous create response arrives late", async ({ page }) => {
  const browserErrors = collectBrowserErrors(page);
  let releaseCreate!: () => void;
  const createGate = new Promise<void>((resolve) => { releaseCreate = resolve; });
  let notifyCreated!: () => void;
  const createdUpstream = new Promise<void>((resolve) => { notifyCreated = resolve; });
  await page.route("**/api/v1/sessions", async (route) => {
    if (route.request().method() !== "POST") {
      await route.continue();
      return;
    }
    const response = await route.fetch();
    notifyCreated();
    await createGate;
    await route.fulfill({ response });
  });

  try {
    await page.goto(projectPath(process.cwd()));
    await expect(page.locator(".project-browser-path")).toHaveText(process.cwd());
    await page.getByRole("link", { name: "Back to projects", exact: true }).click();
    await expect(page).toHaveURL(/\/$/u);
    await page.getByRole("button", { name: "New project", exact: true }).click();
    await page.getByRole("dialog", { name: "New project" })
      .getByLabel("Project working directory", { exact: true }).fill(process.cwd());
    await page.getByRole("dialog", { name: "New project" })
      .getByRole("button", { name: "Create project" }).click();
    await createdUpstream;
    await page.goBack();
    await expect.poll(() => new URL(page.url()).pathname).toBe(projectPath(process.cwd()));
    await page.getByRole("link", { name: "Back to projects", exact: true }).click();
    await expect(page).toHaveURL(/\/$/u);

    const delivered = page.waitForResponse((response) =>
      response.request().method() === "POST" &&
      new URL(response.url()).pathname === "/api/v1/sessions"
    );
    releaseCreate();
    expect((await delivered).status()).toBe(201);
    await page.evaluate(() => new Promise<void>((resolve) => {
      requestAnimationFrame(() => requestAnimationFrame(() => resolve()));
    }));
    await expect(page).toHaveURL(/\/$/u);
    await expect(page.getByRole("heading", { name: "Projects", exact: true })).toBeVisible();
    await expect(page.locator('.session-history [aria-current="page"]')).toHaveCount(0);
    await expect(page.getByRole("button", { name: /^(?:New thread|New project)$/u })).toBeEnabled();
    await expect(page.getByRole("alert")).toHaveCount(0);
    expect(browserErrors).toEqual([]);
  } finally {
    releaseCreate();
    await page.unrouteAll({ behavior: "wait" });
  }
});

test("serializes session list requests while startup and route refreshes overlap", async ({ page }) => {
  const browserErrors = collectBrowserErrors(page);
  let releaseList!: () => void;
  const listGate = new Promise<void>((resolve) => { releaseList = resolve; });
  let notifyListStarted!: () => void;
  const firstListStarted = new Promise<void>((resolve) => { notifyListStarted = resolve; });
  let listRequests = 0;
  let inFlight = 0;
  let maximumInFlight = 0;
  await page.route("**/api/v1/sessions", async (route) => {
    if (route.request().method() !== "GET") {
      await route.continue();
      return;
    }
    listRequests += 1;
    inFlight += 1;
    maximumInFlight = Math.max(maximumInFlight, inFlight);
    let settled = false;
    try {
      if (listRequests === 1) {
        notifyListStarted();
        await listGate;
      }
      const response = await route.fetch();
      inFlight -= 1;
      settled = true;
      await route.fulfill({ response });
    } finally {
      if (!settled) inFlight -= 1;
    }
  });

  try {
    await page.goto("/sessions/saved-session");
    await firstListStarted;
    await page.evaluate(() => {
      for (let index = 0; index < 3; index += 1) {
        window.dispatchEvent(new PopStateEvent("popstate"));
      }
    });
    // Keep the first request pending long enough for independent startup and
    // navigation callbacks to try to refresh the same Agent-owned list.
    await page.waitForTimeout(200);
    releaseList();
    await expect(page.getByRole("heading", { name: "Saved ACP session" })).toBeVisible();
    await expect(page.getByText("Loaded history.", { exact: true })).toBeVisible();
    await expect(page.locator('textarea[role="combobox"]')).toBeEnabled();
    await expect(page.getByRole("alert")).toHaveCount(0);
    expect(listRequests).toBeGreaterThanOrEqual(1);
    expect(maximumInFlight).toBe(1);
    expect(browserErrors).toEqual([]);
  } finally {
    releaseList();
    await page.unrouteAll({ behavior: "wait" });
  }
});

for (const width of [1280, 390]) {
  test(`creates a project with its first session and navigates up independently at ${width}px`, async ({ page }) => {
    const browserErrors = collectBrowserErrors(page);
    const cwd = `${process.cwd()}/tests`;
    await page.setViewportSize({ width, height: 844 });
    await page.goto("/");
    await expect(page.getByRole("heading", { name: "Projects", exact: true })).toBeVisible();
    await expect(page.locator(".page-back")).toHaveCount(0);
    await expectFullWidthMain(page);

    await page.getByRole("button", { name: "New project", exact: true }).click();
    const dialog = page.getByRole("dialog", { name: "New project", exact: true });
    await expect(dialog.getByRole("button", { name: "Cancel new project" })).toBeVisible();
    await expectWithinViewport(dialog, width);
    await dialog.getByLabel("Project working directory", { exact: true }).fill(cwd);
    await page.screenshot({ path: test.info().outputPath("new-project-dialog.png") });
    const created = page.waitForRequest((request) =>
      request.method() === "POST" && new URL(request.url()).pathname === "/api/v1/sessions"
    );
    await dialog.getByRole("button", { name: "Create project", exact: true }).click();
    expect((await created).postDataJSON()).toMatchObject({ cwd });
    await expect(dialog).toBeHidden();
    await expect.poll(() => new URL(page.url()).pathname).toBe(sessionPath("test-session", cwd));
    await expect(page.locator('textarea[role="combobox"]')).toBeEnabled();
    const backToProject = page.getByRole("link", { name: "Back to project", exact: true });
    await expect(backToProject).toHaveClass(/page-back/u);
    await expect(backToProject).toHaveAttribute("href", projectPath(cwd));
    await expectWithinViewport(backToProject, width);
    await expectFullWidthMain(page);
    await page.screenshot({ path: test.info().outputPath("session-back-to-project.png") });

    // The preceding browser entry is the homepage. This button must still go
    // to the new session's project rather than traversing browser history.
    await backToProject.click();
    await expect.poll(() => new URL(page.url()).pathname).toBe(projectPath(cwd));
    await expect(page.locator(".project-browser-path")).toHaveText(cwd);
    await expect(page.locator(".project-session-link")).toHaveCount(1);
    await expect(page.locator(".project-session-link"))
      .toHaveAttribute("href", sessionPath("test-session", cwd));
    const backToProjects = page.getByRole("link", { name: "Back to projects", exact: true });
    await expect(backToProjects).toHaveClass(/page-back/u);
    await expect(backToProjects).toHaveAttribute("href", "/");
    await expectWithinViewport(backToProjects, width);
    await backToProjects.click();
    await expect(page).toHaveURL(/\/$/u);
    await expect(page.getByRole("heading", { name: "Projects", exact: true })).toBeVisible();
    await expect(page.locator(".project-card").filter({ hasText: cwd })).toBeVisible();
    await expect(page.locator(".page-back")).toHaveCount(0);
    await expectFullWidthMain(page);
    expect(await horizontalOverflow(page)).toBeLessThanOrEqual(1);
    expect(browserErrors).toEqual([]);
  });

  test(`navigates up from a deep session link without showing idle Open badges at ${width}px`, async ({ page }) => {
    const browserErrors = collectBrowserErrors(page);
    const cwd = process.cwd();
    await page.setViewportSize({ width, height: 844 });
    await page.goto(projectPath(`${cwd}/tests`));
    await expect(page.locator(".project-browser-path")).toHaveText(`${cwd}/tests`);
    await page.goto(sessionPath("saved-session", "/wrong-project"));
    await expect(page.getByRole("heading", { name: "Saved ACP session" })).toBeVisible();
    await expect.poll(() => new URL(page.url()).pathname).toBe(sessionPath("saved-session", cwd));
    const backToProject = page.getByRole("link", { name: "Back to project", exact: true });
    await expect(backToProject).toHaveAttribute("href", projectPath(cwd));
    await expectWithinViewport(backToProject, width);
    await backToProject.click();
    await expect.poll(() => new URL(page.url()).pathname).toBe(projectPath(cwd));
    const saved = page.locator(".project-session-row").filter({ hasText: "Saved ACP session" });
    await expect(saved).toBeVisible();
    await expect(saved.locator(".project-session-status")).toHaveCount(0);
    await expect(saved.getByText("Open", { exact: true })).toHaveCount(0);
    await page.screenshot({ path: test.info().outputPath("idle-project-sessions.png") });
    await page.reload();
    await expect(saved).toBeVisible();
    await expect(saved.locator(".project-session-status")).toHaveCount(0);
    await expect(saved.getByText("Open", { exact: true })).toHaveCount(0);
    await saved.locator(".project-session-link").click();
    await expect(page.getByRole("heading", { name: "Saved ACP session" })).toBeVisible();
    const picker = await openSessionPicker(page);
    await expect(picker).not.toContainText(/\bopen\b/iu);
    await page.keyboard.press("Escape");
    await backToProject.click();
    await expect(saved).toBeVisible();
    await expect(saved.locator(".project-session-status")).toHaveCount(0);
    await expectWithinViewport(page.getByRole("link", { name: "Back to projects", exact: true }), width);
    await expectFullWidthMain(page);
    expect(await horizontalOverflow(page)).toBeLessThanOrEqual(1);
    expect(browserErrors).toEqual([]);
  });

  test(`navigates home, project, and session pages through browser back and forward at ${width}px`, async ({ page }) => {
    const browserErrors = collectBrowserErrors(page);
    await page.setViewportSize({ width, height: 844 });
    const cwd = process.cwd();
    await page.goto("/");
    await expect(page.getByRole("heading", { name: "Projects", exact: true })).toBeVisible();
    await expectFullWidthMain(page);
    const picker = page.locator(".session-switcher");
    await expect(picker.locator(".session-history")).toHaveCount(0);
    await page.locator(".project-card").filter({ hasText: cwd }).click();
    await expect.poll(() => new URL(page.url()).pathname).toBe(projectPath(cwd));
    await expect(page.locator(".project-browser-path")).toHaveText(cwd);
    await expectFullWidthMain(page);
    await expect(page.locator(".project-session-link")).toHaveCount(2);
    await expect(picker.locator(".session-history")).toHaveCount(0);
    await page.locator(".project-session-link").filter({ hasText: "Saved ACP session" }).click();
    await expect.poll(() => new URL(page.url()).pathname).toBe(sessionPath("saved-session", cwd));
    await expect(page.getByRole("heading", { name: "Saved ACP session" })).toBeVisible();
    await expectFullWidthMain(page);
    await expect(page.getByRole("link", { name: "Project sessions", exact: true })).toHaveAttribute("href", projectPath(cwd));
    await openSessionPicker(page);
    await picker.locator(".session-open").filter({ hasText: "Earlier Agent thread" }).click();
    await expect.poll(() => new URL(page.url()).pathname).toBe(sessionPath("earlier-session", cwd));
    await expect(page.getByRole("heading", { name: "Earlier Agent thread" })).toBeVisible();

    await page.goBack();
    await expect.poll(() => new URL(page.url()).pathname).toBe(sessionPath("saved-session", cwd));
    await expect(page.getByRole("heading", { name: "Saved ACP session" })).toBeVisible();
    await page.goBack();
    await expect.poll(() => new URL(page.url()).pathname).toBe(projectPath(cwd));
    await expect(page.locator(".project-session-link")).toHaveCount(2);
    await expect(page.locator('textarea[role="combobox"]')).toHaveCount(0);
    await page.goBack();
    await expect(page).toHaveURL(/\/$/u);
    await expect(page.getByRole("heading", { name: "Projects", exact: true })).toBeVisible();
    await expect(page.locator('.session-history [aria-current="page"]')).toHaveCount(0);
    await page.goForward();
    await expect.poll(() => new URL(page.url()).pathname).toBe(projectPath(cwd));
    await expect(page.locator(".project-session-link")).toHaveCount(2);
    await page.goForward();
    await expect.poll(() => new URL(page.url()).pathname).toBe(sessionPath("saved-session", cwd));
    await expect(page.getByRole("heading", { name: "Saved ACP session" })).toBeVisible();
    await expect(page.getByText("Loaded history.", { exact: true })).toHaveCount(1);

    await page.getByRole("link", { name: "Project sessions", exact: true }).click();
    await expect.poll(() => new URL(page.url()).pathname).toBe(projectPath(cwd));
    await page.getByRole("link", { name: "All projects", exact: true }).click();
    await expect(page).toHaveURL(/\/$/u);
    await expect(page.getByRole("heading", { name: "Projects", exact: true })).toBeVisible();
    expect(browserErrors).toEqual([]);
  });
}

test("puts created and forked sessions in the URL and returns home on close", async ({ page }) => {
  const browserErrors = collectBrowserErrors(page);
  await page.goto("/");
  await page.getByRole("button", { name: "New project", exact: true }).click();
  await page.getByRole("dialog", { name: "New project" })
    .getByLabel("Project working directory", { exact: true }).fill(process.cwd());
  await page.getByRole("dialog", { name: "New project" })
    .getByRole("button", { name: "Create project" }).click();
  await expect(page).toHaveURL(/\/sessions\/test-session$/u);
  await expect(page.locator('textarea[role="combobox"]')).toBeEnabled();

  await page.getByRole("button", { name: "Thread actions" }).click();
  await page.getByRole("button", { name: "Fork thread" }).click();
  await expect(page).toHaveURL(/\/sessions\/forked-session$/u);
  await expect(page.locator('textarea[role="combobox"]')).toBeEnabled();
  await page.goBack();
  await expect(page).toHaveURL(/\/sessions\/test-session$/u);
  await expect(page.locator('textarea[role="combobox"]')).toBeEnabled();
  await page.goForward();
  await expect(page).toHaveURL(/\/sessions\/forked-session$/u);
  await expect(page.locator('textarea[role="combobox"]')).toBeEnabled();

  if (!(await page.getByRole("button", { name: "Close thread" }).isVisible())) {
    await page.getByRole("button", { name: "Thread actions" }).click();
  }
  await page.getByRole("button", { name: "Close thread" }).click();
  await page.getByRole("alertdialog", { name: "Close session?" }).getByRole("button", { name: "Close session", exact: true }).click();
  await expect(page).toHaveURL(/\/$/u);
  await expect(page.getByRole("heading", { name: "Projects", exact: true })).toBeVisible();
  await expect(page.getByRole("button", { name: /^(?:New thread|New project)$/u })).toBeEnabled();
  await expect(page.getByRole("alert")).toHaveCount(0);
  expect(browserErrors).toEqual([]);
});

test("confirms closing a running session and permits live configuration changes", async ({ page }) => {
  const server = await startRustTestServer({
    command: [process.execPath, "--import", "tsx", join(process.cwd(), "tests/fixtures/session-capabilities-agent.ts"), "--no-load", "--close"],
  });
  const origin = `http://127.0.0.1:${server.port}`;
  const errors = collectBrowserErrors(page);
  let closeRequests = 0;
  page.on("request", (request) => {
    if (request.method() === "POST" && new URL(request.url()).pathname.endsWith("/close")) closeRequests += 1;
  });
  try {
    await page.goto(origin);
    await page.getByRole("button", { name: "New project", exact: true }).click();
    await page.getByRole("dialog").getByRole("button", { name: "Create project" }).click();
    const composer = page.locator('textarea[role="combobox"]');
    await expect(composer).toBeEnabled();
    await composer.fill("wait-close");
    await composer.press("Enter");
    await expect(page.getByText("Waiting for close.", { exact: true })).toBeVisible();
    const verbose = page.getByRole("switch", { name: "Verbose" });
    await expect(verbose).toBeEnabled();
    await verbose.click();
    await expect(verbose).toHaveAttribute("aria-checked", "true");
    await page.getByRole("button", { name: "Thread actions" }).click();
    await page.getByRole("button", { name: "Close thread" }).click();
    const dialog = page.getByRole("alertdialog", { name: "Close session?" });
    await expect(dialog).toBeVisible();
    await expect(dialog.getByRole("button", { name: "Keep session" })).toBeFocused();
    await expect(dialog).toContainText("child processes");
    expect(closeRequests).toBe(0);
    await page.keyboard.press("Escape");
    await expect(dialog).toBeHidden();
    expect(closeRequests).toBe(0);
    await expect(page.getByText("Waiting for close.", { exact: true })).toBeVisible();
    await page.getByRole("button", { name: "Close thread" }).click();
    await dialog.getByRole("button", { name: "Close session", exact: true }).click();
    await expect(page.getByRole("heading", { name: "Projects", exact: true })).toBeVisible();
    expect(closeRequests).toBe(1);
    expect(errors).toEqual([]);
  } finally {
    await server.close();
  }
});

test("creates a session in the selected project instead of the startup directory", async ({ page }) => {
  const browserErrors = collectBrowserErrors(page);
  const cwd = `${process.cwd()}/tests`;
  await page.goto(projectPath(cwd));
  await expect(page.locator(".project-browser-path")).toHaveText(cwd);
  await expect(page.locator(".project-session-link")).toHaveCount(0);
  await page.getByRole("button", { name: "New session in project", exact: true }).click();
  const dialog = page.getByRole("dialog", { name: "New thread" });
  await expect(dialog.getByLabel("Agent workspace", { exact: true })).toHaveValue(cwd);
  const created = page.waitForRequest((request) =>
    request.method() === "POST" && new URL(request.url()).pathname === "/api/v1/sessions"
  );
  await dialog.getByRole("button", { name: "Create thread" }).click();
  expect((await created).postDataJSON()).toMatchObject({ cwd });
  await expect.poll(() => new URL(page.url()).pathname).toBe(sessionPath("test-session", cwd));
  await expect(page.locator('textarea[role="combobox"]')).toBeEnabled();
  await expect(page.locator(".session-open")).toHaveCount(1);
  await expect(page.locator(".session-open").filter({ hasText: "Saved ACP session" })).toHaveCount(0);
  await page.getByRole("link", { name: "Project sessions", exact: true }).click();
  await expect.poll(() => new URL(page.url()).pathname).toBe(projectPath(cwd));
  await expect(page.locator(".project-session-link")).toHaveCount(1);
  await expect(page.locator(".project-session-link")).toHaveAttribute("href", sessionPath("test-session", cwd));
  await expect(page.getByRole("alert")).toHaveCount(0);
  expect(browserErrors).toEqual([]);
});

test("canonicalizes a session link to its reported project and sends missing nested sessions home", async ({ page }) => {
  const browserErrors = collectBrowserErrors(page, [404]);
  const cwd = process.cwd();
  await page.goto(sessionPath("saved-session", "/wrong-project"));
  await expect(page.getByRole("heading", { name: "Saved ACP session" })).toBeVisible();
  await expect.poll(() => new URL(page.url()).pathname).toBe(sessionPath("saved-session", cwd));
  await expect(page.getByRole("link", { name: "Project sessions", exact: true })).toHaveAttribute("title", cwd);
  await page.goto(sessionPath("nonexistent-session", cwd));
  await expect(page).toHaveURL(/\/$/u);
  await expect(page.getByRole("heading", { name: "Projects", exact: true })).toBeVisible();
  await expect(page.locator(".session-history")).toHaveCount(0);
  await expect(page.getByRole("alert")).toHaveCount(0);
  expect(browserErrors).toEqual([]);
});

test("reloads a created session URL even when the Agent list is empty", async ({ page }) => {
  const server = await startRustTestServer({
    cwd: process.cwd(),
    command: [
      process.execPath,
      "--import",
      "tsx",
      join(process.cwd(), "tests/fixtures/fake-agent.ts"),
      "--empty-session-list",
    ],
  });
  const browserErrors = collectBrowserErrors(page);
  try {
    await page.goto(`http://127.0.0.1:${server.port}/`);
    await expect(page.getByRole("heading", { name: "Projects", exact: true })).toBeVisible();
    await page.getByRole("button", { name: "New project", exact: true }).click();
    const refreshedList = page.waitForResponse((response) =>
      response.request().method() === "GET" &&
      new URL(response.url()).pathname === "/api/v1/sessions" &&
      response.status() === 200
    );
    await page.getByRole("dialog", { name: "New project" })
      .getByLabel("Project working directory", { exact: true }).fill(process.cwd());
    await page.getByRole("dialog", { name: "New project" })
      .getByRole("button", { name: "Create project" }).click();
    await expect(page).toHaveURL(/\/sessions\/test-session$/u);
    await expect(page.locator('textarea[role="combobox"]')).toBeEnabled();
    const listed = await refreshedList;
    expect((await listed.json()).sessions).toEqual([]);
    const picker = await openSessionPicker(page);
    const workspaceGroup = picker.locator(".session-group").filter({
      has: page.getByRole("heading", { name: process.cwd(), exact: true }),
    });
    await expect(workspaceGroup.locator('[aria-current="page"]')).toBeVisible();
    await expect(picker.locator(".session-group-label")).toHaveCount(1);

    await page.reload();
    await expect(page).toHaveURL(/\/sessions\/test-session$/u);
    await expect(page.getByRole("heading", { name: "New agent session" })).toBeVisible();
    await expect(page.locator('textarea[role="combobox"]')).toBeEnabled();
    await openSessionPicker(page);
    await expect(workspaceGroup.locator('[aria-current="page"]')).toBeVisible();
    await expect(picker.locator(".session-group-label")).toHaveCount(1);
    await expect(page.getByRole("alert")).toHaveCount(0);
    expect(browserErrors).toEqual([]);
  } finally {
    await server.close();
  }
});

test("browses cross-workspace projects and restores a shared session with its own cwd", async ({ browser, page }) => {
  const cwd = process.cwd();
  const otherCwd = "/other-workspace";
  const server = await startRustTestServer({
    cwd,
    command: [
      process.execPath,
      "--import",
      "tsx",
      join(cwd, "tests/fixtures/fake-agent.ts"),
      "--cross-workspace-sessions",
    ],
  });
  const origin = `http://127.0.0.1:${server.port}`;
  const browserErrors = collectBrowserErrors(page);
  try {
    await page.goto(`${origin}/`);
    const picker = page.locator(".session-switcher");
    await expect(page.locator(".project-card")).toHaveCount(1);
    await expect(picker.locator(".session-open")).toHaveCount(0);
    await page.getByRole("button", { name: "Load more sessions", exact: true }).click();
    await expect(page.locator(".project-card")).toHaveCount(2);
    await expect(page.locator(".project-card-path")).toHaveText([cwd, otherCwd]);
    const search = page.getByRole("searchbox", { name: "Search projects", exact: true });
    await search.fill(otherCwd);
    await expect(page.locator(".project-card")).toHaveCount(1);
    await expect(page.locator(".project-card-path")).toHaveText(otherCwd);
    await search.press("Escape");
    await expect(page.locator(".project-card")).toHaveCount(2);
    await page.locator(".project-card").filter({ hasText: otherCwd }).click();
    await expect.poll(() => new URL(page.url()).pathname).toBe(projectPath(otherCwd));
    await expect(page.locator(".project-browser-path")).toHaveText(otherCwd);
    await expect(page.locator(".project-session-link")).toHaveCount(1);
    await expect(page.locator(".project-session-link")).toContainText("Earlier Agent thread");
    await expect(picker.locator(".session-open")).toHaveCount(0);

    await page.reload();
    await expect.poll(() => new URL(page.url()).pathname).toBe(projectPath(otherCwd));
    await expect(page.locator(".project-session-link")).toHaveCount(1);
    await expect(page.locator(".project-session-link")).toContainText("Earlier Agent thread");
    await page.locator(".project-session-link").click();
    await expect.poll(() => new URL(page.url()).pathname).toBe(sessionPath("earlier-session", otherCwd));
    await expect(page.getByRole("heading", { name: "Earlier Agent thread" })).toBeVisible();
    await expect(page.locator('textarea[role="combobox"]')).toBeEnabled();
    await openSessionPicker(page);
    await expect(picker.locator(".session-open")).toHaveCount(1);
    await expect(picker.locator('.session-history [aria-current="page"]')).toContainText("Earlier Agent thread");
    await expect(picker.locator(".session-open").filter({ hasText: "Saved ACP session" })).toHaveCount(0);
    await expect(page.getByRole("link", { name: "Project sessions", exact: true }))
      .toHaveAttribute("title", otherCwd);

    await page.keyboard.press("Escape");
    await page.getByRole("button", { name: "New thread", exact: true }).click();
    const newThread = page.getByRole("dialog", { name: "New thread" });
    await expect(newThread.getByLabel("Agent workspace", { exact: true })).toHaveValue(otherCwd);
    await newThread.getByRole("button", { name: "Cancel new thread" }).click();

    // Release the active view so a shared URL must discover and load its cwd.
    await page.getByRole("button", { name: "Thread actions" }).click();
    const refreshedList = page.waitForResponse((response) =>
      response.request().method() === "GET" &&
      new URL(response.url()).pathname === "/api/v1/sessions" &&
      response.status() === 200
    );
    await page.getByRole("button", { name: "Close thread" }).click();
    await page.getByRole("alertdialog", { name: "Close session?" }).getByRole("button", { name: "Close session", exact: true }).click();
    await expect(page).toHaveURL(/\/$/u);
    await refreshedList;
    const shared = await browser.newPage();
    const sharedErrors = collectBrowserErrors(shared);
    const listedCursors: Array<string | null> = [];
    shared.on("request", (request) => {
      const url = new URL(request.url());
      if (request.method() === "GET" && url.pathname === "/api/v1/sessions") {
        listedCursors.push(url.searchParams.get("cursor"));
      }
    });
    try {
      await shared.goto(`${origin}${sessionPath("earlier-session", otherCwd)}`);
      await expect(shared.getByRole("heading", { name: "Earlier Agent thread" })).toBeVisible();
      await expect(shared.getByRole("link", { name: "Project sessions", exact: true }))
        .toHaveAttribute("title", otherCwd);
      await expect(shared.locator('textarea[role="combobox"]')).toBeEnabled();
      await expect.poll(() => new URL(shared.url()).pathname).toBe(sessionPath("earlier-session", otherCwd));
      await expect(shared.getByRole("alert")).toHaveCount(0);
      expect(listedCursors).toContain(null);
      expect(listedCursors.some((cursor) => cursor != null)).toBe(true);
      expect(sharedErrors).toEqual([]);
    } finally {
      await shared.close();
    }
    expect(await horizontalOverflow(page)).toBeLessThanOrEqual(1);
    expect(browserErrors).toEqual([]);
  } finally {
    await server.close();
  }
});

test("restores the session in a directly opened URL on reload and in a fresh browser", async ({ browser, page }) => {
  const browserErrors = collectBrowserErrors(page);
  const response = await page.goto("/sessions/saved-session");
  expect(response?.status()).toBe(200);
  expect(response?.headers()["content-type"]).toContain("text/html");
  await expect(page.getByRole("heading", { name: "Saved ACP session" })).toBeVisible();
  await expect(page.getByText("Loaded history.", { exact: true })).toBeVisible();
  await openSessionPicker(page);
  await expect(page.locator(".session-open")).toHaveCount(2);
  await expect(page.locator('.session-history [aria-current="page"]')).toHaveCount(1);
  await expect(page.getByText("Earlier Agent thread", { exact: true })).toBeVisible();

  await page.reload();
  await expect(page.getByRole("heading", { name: "Saved ACP session" })).toBeVisible();
  await expect(page.getByText("Loaded history.", { exact: true })).toBeVisible();
  await openSessionPicker(page);
  await expect(page.locator(".session-open")).toHaveCount(2);
  await expect(page.locator('.session-history [aria-current="page"]')).toHaveCount(1);
  await expect(page).toHaveURL(/\/sessions\/saved-session$/u);
  const shared = await browser.newPage();
  try {
    await shared.goto(page.url());
    await expect(shared.getByRole("heading", { name: "Saved ACP session" })).toBeVisible();
    await expect(shared.getByText("Loaded history.", { exact: true })).toHaveCount(1);
    await expect(shared.locator('textarea[role="combobox"]')).toBeEnabled();
  } finally {
    await shared.close();
  }
  expect(browserErrors).toEqual([]);
});

test("keeps completed turn markers at their original boundaries across reload", async ({ page }) => {
  const browserErrors = collectBrowserErrors(page);
  await page.goto("/sessions/saved-session");

  const composer = page.locator('textarea[role="combobox"]');
  await expect(composer).toBeEnabled();
  await composer.fill("context-window-flow");
  await composer.press("Enter");
  await expect(page.getByText("Context usage updated.", { exact: true })).toBeVisible();
  await expect(page.locator('.turn-stop[data-stop-reason="end_turn"]')).toHaveCount(1);

  await composer.fill("usage-flow");
  await composer.press("Enter");
  await expect(page.locator('.turn-stop[data-stop-reason="max_tokens"]')).toHaveCount(1);
  await expectTurnMarkerOrder(page);

  await page.reload();
  await expect(page.getByText("Context usage updated.", { exact: true })).toBeVisible();
  await expect(page.locator(".turn-stop")).toHaveCount(2);
  await expectTurnMarkerOrder(page);
  expect(browserErrors).toEqual([]);
});

test("offers an explicit reconnect over the composer after the ACP connection stops", async ({ page }) => {
  const browserErrors = collectBrowserErrors(page);
  await page.goto("/sessions/saved-session");

  const composer = page.locator('textarea[role="combobox"]');
  await expect(composer).toBeEnabled();
  await composer.fill("disconnect-flow");
  await composer.press("Enter");

  const recovery = page.locator(".composer-reconnect");
  await expect(recovery).toBeVisible();
  await expect(recovery).toContainText("Agent connection unavailable");
  await expect(composer).toBeDisabled();
  await recovery.getByRole("button", { name: "Reconnect" }).click();
  await expect(page.getByRole("heading", { name: "Saved ACP session" })).toBeVisible();
  await expect(page.locator('textarea[role="combobox"]')).toBeEnabled();
  expect(browserErrors).toEqual([]);
});

test("keeps an active stdio prompt alive across browser reconnect", async ({ browser, page }) => {
  const temporaryDirectory = await mkdtemp(join(tmpdir(), "attyd-disconnect-cancel-"));
  const marker = join(temporaryDirectory, "lifecycle.txt");
  const processMarker = join(temporaryDirectory, "agent.pid");
  const server = await startRustTestServer({
    cwd: process.cwd(),
    env: {
      ...process.env,
      ATTYD_FAKE_DISCONNECT_CANCEL_FILE: marker,
      ATTYD_FAKE_PROCESS_MARKER_FILE: processMarker,
    },
    command: [
      process.execPath,
      "--import",
      "tsx",
      join(process.cwd(), "tests/fixtures/fake-agent.ts"),
    ],
  });

  try {
    await expect.poll(() => readMarker(processMarker)).not.toBe("");
    const agentPid = await readMarker(processMarker);
    await page.goto(`http://127.0.0.1:${server.port}/sessions/saved-session`);
    const composer = page.locator('textarea[role="combobox"]');
    await expect(composer).toBeEnabled();
    await composer.fill("disconnect-cancel-flow");
    await composer.press("Enter");
    await expect.poll(() => readMarker(marker)).toBe("prompt");

    await page.close();
    await new Promise((resolve) => setTimeout(resolve, 300));
    expect(await readMarker(marker)).toBe("prompt");

    const reconnected = await browser.newPage();
    try {
      await reconnected.goto(`http://127.0.0.1:${server.port}/sessions/saved-session`);
      await expect(reconnected.getByText("disconnect-cancel-flow", { exact: true })).toBeVisible();
      const stop = reconnected.getByRole("button", { name: "Stop current turn" });
      await expect(stop).toBeVisible();
      await expect.poll(() => readMarker(processMarker)).toBe(agentPid);
      await stop.click();
      await expect.poll(() => readMarker(marker)).toBe("cancel");
      await expect(reconnected.locator('textarea[role="combobox"]')).toBeEnabled();
    } finally {
      await reconnected.close();
    }
  } finally {
    await server.close();
    await rm(temporaryDirectory, { recursive: true, force: true });
  }
});

test("reconnects a stale mobile-style socket after a focus liveness probe", async ({ page }) => {
  const browserErrors = collectBrowserErrors(page);
  await page.goto("/sessions/saved-session");

  const composer = page.locator('textarea[role="combobox"]');
  await expect(composer).toBeEnabled();
  await composer.fill("disconnect-flow");
  await composer.press("Enter");
  await expect(page.locator(".composer-reconnect")).toBeVisible();

  await page.evaluate(() => window.dispatchEvent(new Event("focus")));
  await expect(page.getByRole("heading", { name: "Saved ACP session" })).toBeVisible();
  await expect(page.locator('textarea[role="combobox"]')).toBeEnabled();
  expect(browserErrors).toEqual([]);
});

test("recovers startup through Agent-owned ACP authentication", async ({ page }) => {
  const authServer = await startRustTestServer({
    cwd: process.cwd(),
    command: [
      process.execPath,
      "--import",
      "tsx",
      join(process.cwd(), "tests/fixtures/fake-agent.ts"),
      "--auth-required",
    ],
  });
  const browserErrors = collectBrowserErrors(page, [409]);

  try {
    await page.goto(`http://127.0.0.1:${authServer.port}/sessions/saved-session`);
    const signIn = page.getByRole("button", { name: "Authenticate with Continue with Fake Agent" });
    await expect(page.getByRole("heading", { name: "Sign in to continue" })).toBeVisible();
    await expect(signIn).toBeFocused();
    await expect(page.locator('textarea[role="combobox"]')).toHaveCount(0);
    await expect(page.getByRole("button", { name: /^(?:New project|New session in project)$/u })).toBeDisabled();
    await signIn.click();

    await expect(page.getByRole("heading", { name: "Saved ACP session" })).toBeVisible();
    await expect(page.getByText("Loaded history.", { exact: true })).toBeVisible();
    await expect(page.getByRole("heading", { name: "Sign in to continue" })).toBeHidden();
    await expect(page.locator('textarea[role="combobox"]')).toBeEnabled();

    await page.getByRole("button", { name: "Agent settings", exact: true }).click();
    const authDetails = page.locator(".agent-auth-details");
    await authDetails.locator(":scope > summary").click();
    await expect(authDetails).toContainText("Signed in");
    await expect(authDetails.getByText("Authenticate response", { exact: true })).toBeVisible();

    page.once("dialog", (dialog) => dialog.accept());
    await authDetails.getByRole("button", { name: "Sign out of Agent" }).click();
    await expect(page.getByRole("heading", { name: "Signed out of attyd-test-agent" }))
      .toBeVisible();
    await expect(page.getByText("Loaded history.", { exact: true })).toBeVisible();
    await page.keyboard.press("Escape");
    await openSessionPicker(page);
    await expect(page.locator('.session-history [aria-current="page"]')).toBeVisible();
    await page.keyboard.press("Escape");
    await expect(page.getByRole("button", { name: "New thread" })).toBeDisabled();

    await page.getByRole("button", { name: "Authenticate with Continue with Fake Agent" }).first().click();
    await expect(page.getByRole("heading", { name: "Signed out of attyd-test-agent" }))
      .toBeHidden();
    await expect(page.getByRole("button", { name: /^(?:New thread|New project)$/u })).toBeEnabled();
    expect(await horizontalOverflow(page)).toBeLessThanOrEqual(1);
    expect(browserErrors).toEqual([]);
  } finally {
    await authServer.close();
  }
});

test("runs negotiated terminal authentication and reconnects the Agent", async ({ page }) => {
  const root = await mkdtemp(join(tmpdir(), "attyd-browser-terminal-auth-"));
  const authFile = join(root, "authenticated");
  const authServer = await startRustTestServer({
    cwd: process.cwd(),
    env: { ...process.env, ATTYD_FAKE_AUTH_FILE: authFile },
    command: [
      process.execPath,
      "--import",
      "tsx",
      join(process.cwd(), "tests/fixtures/fake-agent.ts"),
      "--terminal-auth-required",
    ],
  });
  const browserErrors = collectBrowserErrors(page, [409]);

  try {
    await page.goto(`http://127.0.0.1:${authServer.port}/sessions/saved-session`);
    const signIn = page.getByRole("button", { name: "Authenticate with Sign in in terminal" });
    await expect(page.getByRole("heading", { name: "Sign in to continue" })).toBeVisible();
    await expect(signIn).toBeEnabled();
    await signIn.click();

    const terminal = page.locator(".auth-terminal-card");
    await expect(terminal).toBeVisible();
    await expect(terminal).toContainText("Interactive");
    const terminalInput = page.getByLabel("Agent terminal authentication input");
    await expect(terminalInput).toBeFocused();
    await page.setViewportSize({ width: 390, height: 844 });
    await expect(terminal.getByRole("button", { name: "Cancel" })).toBeVisible();
    expect(await horizontalOverflow(page)).toBeLessThanOrEqual(1);
    await terminalInput.focus();
    await terminalInput.pressSequentially("open-sesame");
    await terminalInput.press("Enter");

    await expect(page.getByRole("heading", { name: "Saved ACP session" })).toBeVisible();
    await expect(page.getByText("Loaded history.", { exact: true })).toBeVisible();
    await expect(page.locator(".auth-terminal-card")).toBeHidden();
    await expect(page.locator('textarea[role="combobox"]')).toBeEnabled();
    expect(await horizontalOverflow(page)).toBeLessThanOrEqual(1);
    expect(browserErrors).toEqual([]);
  } finally {
    await authServer.close();
    await rm(root, { recursive: true, force: true });
  }
});

async function openSessionPicker(page: Page): Promise<Locator> {
  const picker = page.locator("details.session-switcher");
  if (await picker.getAttribute("open") == null) {
    await page.getByRole("button", { name: "Switch project session", exact: true }).click();
  }
  await expect(picker.locator(".session-history")).toBeVisible();
  return picker;
}

async function expectWithinViewport(locator: Locator, width: number): Promise<void> {
  const bounds = await locator.boundingBox();
  expect(bounds).not.toBeNull();
  expect(bounds!.x).toBeGreaterThanOrEqual(-1);
  expect(bounds!.x + bounds!.width).toBeLessThanOrEqual(width + 1);
}

async function expectFullWidthMain(page: Page): Promise<void> {
  await expect(page.locator("aside, .mobile-menu, .sidebar-overlay")).toHaveCount(0);
  await expect(page.getByRole("button", { name: "Open sidebar", exact: true })).toHaveCount(0);
  const bounds = await page.getByRole("main").boundingBox();
  const width = page.viewportSize()?.width;
  expect(bounds).not.toBeNull();
  expect(width).toBeDefined();
  expect(Math.abs(bounds!.x)).toBeLessThanOrEqual(1);
  expect(bounds!.width).toBeGreaterThanOrEqual(width! - 1);
}

async function expectTurnMarkerOrder(page: Page): Promise<void> {
  const entries = await page.locator("[data-thread-entry]").evaluateAll((elements) =>
    elements.map((element) => {
      if (element.classList.contains("turn-stop")) {
        return `stop:${element.getAttribute("data-stop-reason")}`;
      }
      const role = element.getAttribute("data-thread-role");
      const content = element.querySelector<HTMLElement>("[data-thread-searchable]")
        ?.textContent?.trim();
      return role == null ? "other" : `${role}:${content ?? ""}`;
    })
  );
  const firstAnswer = entries.indexOf("agent:Context usage updated.");
  const firstStop = entries.indexOf("stop:end_turn");
  const secondPrompt = entries.indexOf("user:usage-flow");
  const secondStop = entries.indexOf("stop:max_tokens");
  expect(firstAnswer).toBeGreaterThanOrEqual(0);
  expect(firstStop).toBeGreaterThan(firstAnswer);
  expect(secondPrompt).toBeGreaterThan(firstStop);
  expect(secondStop).toBeGreaterThan(secondPrompt);
}

function collectBrowserErrors(page: Page, expectedHttpStatuses: number[] = []): string[] {
  const errors: string[] = [];
  page.on("console", (message) => {
    if (message.type() !== "error") return;
    const text = message.text();
    if (
      text.startsWith("Failed to load resource: the server responded with a status of") &&
      expectedHttpStatuses.some((status) => text.includes(`status of ${status} (`))
    ) return;
    errors.push(text);
  });
  page.on("pageerror", (error) => errors.push(error.message));
  return errors;
}

async function readMarker(path: string): Promise<string> {
  try {
    return (await readFile(path, "utf8")).trim();
  } catch {
    return "";
  }
}

function horizontalOverflow(page: Page): Promise<number> {
  return page.evaluate(() =>
    Math.max(document.documentElement.scrollWidth, document.body.scrollWidth) - window.innerWidth
  );
}

function toolSummaryLayout(tool: Locator) {
  return tool.locator(":scope > .tool-card-header").evaluate((summary) => {
    const title = summary.querySelector<HTMLElement>(".tool-title")!.getBoundingClientRect();
    const actions = summary.querySelector<HTMLElement>(".tool-actions")!.getBoundingClientRect();
    const strongElement = summary.querySelector<HTMLElement>(".tool-title strong")!;
    const strong = strongElement.getBoundingClientRect();
    return {
      summary: summary.getBoundingClientRect().toJSON(),
      title: title.toJSON(),
      actions: actions.toJSON(),
      titleLineHeight: strong.height,
      titleTextOverflow: getComputedStyle(strongElement).textOverflow,
      titleOverflow: strongElement.scrollWidth > strongElement.clientWidth,
      titleHeightOverflow: strongElement.scrollHeight > strongElement.clientHeight,
    };
  });
}
