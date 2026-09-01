import { expect, test, type Locator, type Page } from "@playwright/test";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { startRustTestServer } from "../../scripts/rust-test-server";

test("drives permission and form ACP interactions with real focus restoration", async ({ page }) => {
  const browserErrors = collectBrowserErrors(page);
  await page.goto("/");

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
  await page.goto("/");

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
  await page.goto("/");

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
  await page.goto("/");
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
  await expect(prompt.getByAltText("ACP image content")).toBeVisible();
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
  await page.goto("/");
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
  await page.goto("/");
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

test("keeps the mobile sidebar bounded and restores focus after Escape", async ({ page }) => {
  const browserErrors = collectBrowserErrors(page);
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto("/");
  await expect(page.locator('textarea[role="combobox"]')).toBeEnabled();

  const open = page.getByRole("button", { name: "Open sidebar" });
  await open.click();
  const sidebar = page.getByRole("complementary", { name: "Application sidebar" });
  const close = page.getByRole("button", { name: "Close sidebar" });
  await expect(sidebar).toHaveClass(/sidebar-open/);
  await expect(close).toBeFocused();
  await page.keyboard.press("Escape");
  await expect(sidebar).not.toHaveClass(/sidebar-open/);
  await expect(open).toBeFocused();
  expect(await horizontalOverflow(page)).toBeLessThanOrEqual(1);
  expect(browserErrors).toEqual([]);
});

test("filters only Agent-reported threads in the Zed-style sidebar", async ({ page }) => {
  const browserErrors = collectBrowserErrors(page);
  await page.goto("/");
  await expect(page.locator('textarea[role="combobox"]')).toBeEnabled();

  const sidebar = page.getByRole("complementary", { name: "Application sidebar" });
  const filter = sidebar.getByRole("searchbox", { name: "Filter loaded Agent threads" });
  await expect(filter).toBeVisible();
  await expect(sidebar.locator(".session-filter-status")).toHaveText("2 loaded threads");
  await expect(sidebar.getByRole("heading", { name: "Current" })).toBeVisible();
  await expect(sidebar.getByText("Earlier Agent thread", { exact: true })).toBeVisible();
  const currentThread = sidebar.locator('[aria-current="page"]');

  await filter.fill("ear agent");
  await expect(sidebar.locator(".session-filter-status")).toHaveText("1 of 2 loaded threads");
  await expect(sidebar.getByText("Earlier Agent thread", { exact: true })).toBeVisible();
  await expect(currentThread).toBeHidden();
  await filter.press("Escape");
  await expect(filter).toHaveValue("");
  await expect(sidebar.locator('[aria-current="page"]')).toBeVisible();

  await page.setViewportSize({ width: 390, height: 844 });
  await page.getByRole("button", { name: "Open sidebar" }).click();
  await expect(sidebar).toHaveClass(/sidebar-open/);
  await expect(filter).toBeVisible();
  await expect.poll(async () => (await sidebar.boundingBox())?.x ?? -1).toBeGreaterThanOrEqual(-1);
  const bounds = await sidebar.boundingBox();
  expect(bounds).not.toBeNull();
  expect(bounds!.x).toBeGreaterThanOrEqual(0);
  expect(bounds!.x + bounds!.width).toBeLessThanOrEqual(390);
  expect(await horizontalOverflow(page)).toBeLessThanOrEqual(1);
  expect(browserErrors).toEqual([]);
});

test("switches between already-open ACP threads without loading them twice", async ({ page }) => {
  const browserErrors = collectBrowserErrors(page);
  await page.goto("/");

  const sidebar = page.getByRole("complementary", { name: "Application sidebar" });
  const thread = page.getByRole("region", { name: "Conversation thread" });
  await expect(page.getByRole("heading", { name: "Saved ACP session" })).toBeVisible();
  await page.addStyleTag({ content: ".conversation-wrap{min-height:1500px!important}" });

  await sidebar.locator(".session-open")
    .filter({ hasText: "Earlier Agent thread" })
    .click();
  await expect(page.getByRole("heading", { name: "Earlier Agent thread" })).toBeVisible();
  await expect.poll(() => thread.evaluate((element) =>
    element.scrollHeight - element.clientHeight - element.scrollTop
  )).toBeLessThan(3);
  await expect(sidebar.locator(".session-open").filter({ hasText: "Saved ACP session" }))
    .toContainText("open");

  await thread.evaluate((element) => { element.scrollTop = 0; });
  await sidebar.locator(".session-open")
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
    await page.goto(`http://127.0.0.1:${server.port}`);
    const sidebar = page.getByRole("complementary", { name: "Application sidebar" });
    await expect(page.getByRole("heading", { name: "Saved ACP session" })).toBeVisible();

    await sidebar.locator(".session-open")
      .filter({ hasText: "Earlier Agent thread" })
      .click();
    await expect(page.getByRole("heading", { name: "Earlier Agent thread" })).toBeVisible();
    const oldDelete = sidebar.getByRole("button", { name: "Delete Saved ACP session" });
    await expect(oldDelete).toBeEnabled();
    page.once("dialog", (dialog) => dialog.accept());
    await oldDelete.click();
    await expect(sidebar.getByText("Saved ACP session", { exact: true })).toBeHidden();
    await expect(page.getByRole("alert")).toHaveCount(0);

    await page.getByRole("button", { name: "Thread actions" }).click();
    page.once("dialog", (dialog) => dialog.accept());
    await page.getByRole("button", { name: "Delete thread" }).click();
    await expect(page.getByRole("heading", { name: "No active session" })).toBeVisible();
    await expect(sidebar.getByText("Earlier Agent thread", { exact: true })).toBeHidden();
    await expect(page.getByRole("alert")).toHaveCount(0);
    expect(browserErrors).toEqual([]);
  } finally {
    await server.close();
  }
});

test("keeps expanded ACP turn payloads inside the message flow", async ({ page }) => {
  const browserErrors = collectBrowserErrors(page);
  await page.goto("/");

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
  await page.goto("/");

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
  await page.goto("/");

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
  await page.goto("/");

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

test("follows ACP thought and tool activity with responsive Zed-style disclosure", async ({ page }) => {
  const browserErrors = collectBrowserErrors(page);
  await page.goto("/");

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
  await expect(tool.locator(":scope > .tool-card-header .tool-title strong")).toHaveText("Read");
  await expect(
    tool.locator(":scope > .tool-card-header").getByRole("button", { name: "Tool info" }),
  ).toHaveCount(0);
  await expect(tool.locator(":scope > .tool-body")).toBeHidden();
  await tool.locator(":scope > .tool-card-header .tool-disclosure").click();
  await expect(tool).toHaveAttribute("data-open", "true");
  await expect(tool.locator(".tool-description")).toContainText(
    "Inspect workspace dependencies and generated configuration files",
  );
  await expect(tool.locator(".tool-input")).toContainText("path");
  await expect(tool.locator(".tool-input")).toContainText("/workspace");
  await expect(tool.locator(".tool-output")).toContainText("dependencies");
  await expect(tool.locator(".tool-output")).toContainText("12");
  await expect(tool.locator(".tool-input > .structured-data")).toBeVisible();
  await expect(tool.locator(".tool-output > .structured-data")).toBeVisible();
  expect(await Promise.all([
    tool.locator(".tool-input > .structured-data").evaluate((element) => ({
      background: getComputedStyle(element).backgroundColor,
      border: getComputedStyle(element).borderColor,
    })),
    tool.locator(".tool-output > .structured-data").evaluate((element) => ({
      background: getComputedStyle(element).backgroundColor,
      border: getComputedStyle(element).borderColor,
    })),
  ])).toEqual([
    { background: "rgb(248, 248, 246)", border: "rgb(228, 228, 223)" },
    { background: "rgb(248, 248, 246)", border: "rgb(228, 228, 223)" },
  ]);
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
  await expect(toolDebug.locator("pre")).toContainText("tool_call_update");
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
    await expect(tool.locator(".tool-status")).toBeVisible();
    const layout = await toolSummaryLayout(tool);
    expect(layout.title.right).toBeLessThanOrEqual(layout.actions.left);
    expect(layout.actions.right).toBeLessThanOrEqual(layout.summary.right - 7);
    expect(layout.summary.height).toBeLessThanOrEqual(40);
    expect(layout.titleLineHeight).toBeLessThanOrEqual(16);
    expect(layout.titleOverflow).toBe(false);
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
    expect(await horizontalOverflow(page)).toBeLessThanOrEqual(1);
  }
  expect(browserErrors).toEqual([]);
});

test("stops following streamed Agent output after the user scrolls upward", async ({ page }) => {
  const browserErrors = collectBrowserErrors(page);
  await page.setViewportSize({ width: 900, height: 480 });
  await page.goto("/");
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

test("searches the visible ACP Agent thread with Zed-style match navigation", async ({ page }) => {
  const browserErrors = collectBrowserErrors(page);
  await page.goto("/");

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

test("searches tool output only after the default-collapsed tool is expanded", async ({ page }) => {
  const browserErrors = collectBrowserErrors(page);
  await page.goto("/");

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
  await expect(search.locator("output")).toHaveText("1/1");
  expect(browserErrors).toEqual([]);
});

test("collapses completed ACP compaction summaries into a Zed-style thread disclosure", async ({ page }) => {
  const browserErrors = collectBrowserErrors(page);
  await page.goto("/");

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
  await page.goto("/");

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
  await page.goto("/");

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

test("restores the latest Agent session instead of creating an empty thread on reload", async ({ page }) => {
  const browserErrors = collectBrowserErrors(page);
  await page.goto("/");
  await expect(page.getByRole("heading", { name: "Saved ACP session" })).toBeVisible();
  await expect(page.getByText("Loaded history.", { exact: true })).toBeVisible();
  await expect(page.locator(".session-open")).toHaveCount(2);
  await expect(page.locator('[aria-current="page"]')).toHaveCount(1);
  await expect(page.getByText("Earlier Agent thread", { exact: true })).toBeVisible();

  await page.reload();
  await expect(page.getByRole("heading", { name: "Saved ACP session" })).toBeVisible();
  await expect(page.getByText("Loaded history.", { exact: true })).toBeVisible();
  await expect(page.locator(".session-open")).toHaveCount(2);
  await expect(page.locator('[aria-current="page"]')).toHaveCount(1);
  expect(browserErrors).toEqual([]);
});

test("offers an explicit reconnect over the composer after the ACP connection stops", async ({ page }) => {
  const browserErrors = collectBrowserErrors(page);
  await page.goto("/");

  const composer = page.locator('textarea[role="combobox"]');
  await expect(composer).toBeEnabled();
  await composer.fill("disconnect-flow");
  await composer.press("Enter");

  const recovery = page.locator(".composer-reconnect");
  await expect(recovery).toBeVisible();
  await expect(recovery).toContainText("Agent connection unavailable");
  await expect(composer).toBeDisabled();
  await Promise.all([
    page.waitForEvent("domcontentloaded"),
    recovery.getByRole("button", { name: "Reconnect" }).click(),
  ]);
  await expect(page.getByRole("heading", { name: "Saved ACP session" })).toBeVisible();
  await expect(page.locator('textarea[role="combobox"]')).toBeEnabled();
  expect(browserErrors).toEqual([]);
});

test("reconnects a stale mobile-style socket after a focus liveness probe", async ({ page }) => {
  const browserErrors = collectBrowserErrors(page);
  await page.goto("/");

  const composer = page.locator('textarea[role="combobox"]');
  await expect(composer).toBeEnabled();
  await composer.fill("disconnect-flow");
  await composer.press("Enter");
  await expect(page.locator(".composer-reconnect")).toBeVisible();

  const reloaded = page.waitForEvent("domcontentloaded");
  await page.evaluate(() => window.dispatchEvent(new Event("focus")));
  await reloaded;
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
  const browserErrors = collectBrowserErrors(page);

  try {
    await page.goto(`http://127.0.0.1:${authServer.port}`);
    const signIn = page.getByRole("button", { name: "Authenticate with Continue with Fake Agent" });
    await expect(page.getByRole("heading", { name: "Sign in to continue" })).toBeVisible();
    await expect(signIn).toBeFocused();
    await expect(page.locator('textarea[role="combobox"]')).toBeDisabled();
    await expect(page.getByRole("button", { name: "New thread" })).toBeDisabled();
    await signIn.click();

    await expect(page.getByRole("heading", { name: "Saved ACP session" })).toBeVisible();
    await expect(page.getByText("Loaded history.", { exact: true })).toBeVisible();
    await expect(page.getByRole("heading", { name: "Sign in to continue" })).toBeHidden();
    await expect(page.locator('textarea[role="combobox"]')).toBeEnabled();

    await page.locator(".agent-details > summary").click();
    const authDetails = page.locator(".agent-auth-details");
    await authDetails.locator(":scope > summary").click();
    await expect(authDetails).toContainText("Signed in");
    await expect(authDetails.getByText("Authenticate response", { exact: true })).toBeVisible();

    page.once("dialog", (dialog) => dialog.accept());
    await authDetails.getByRole("button", { name: "Sign out of Agent" }).click();
    await expect(page.getByRole("heading", { name: "Signed out of attyd-test-agent" }))
      .toBeVisible();
    await expect(page.getByText("Loaded history.", { exact: true })).toBeVisible();
    await expect(page.locator('[aria-current="page"]')).toBeVisible();
    await expect(page.getByRole("button", { name: "New thread" })).toBeDisabled();

    await page.getByRole("button", { name: "Authenticate with Continue with Fake Agent" }).first().click();
    await expect(page.getByRole("heading", { name: "Signed out of attyd-test-agent" }))
      .toBeHidden();
    await expect(page.getByRole("button", { name: "New thread" })).toBeEnabled();
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
  const browserErrors = collectBrowserErrors(page);

  try {
    await page.goto(`http://127.0.0.1:${authServer.port}`);
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

function collectBrowserErrors(page: Page): string[] {
  const errors: string[] = [];
  page.on("console", (message) => {
    if (message.type() === "error") errors.push(message.text());
  });
  page.on("pageerror", (error) => errors.push(error.message));
  return errors;
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
      titleOverflow: strongElement.scrollWidth > strongElement.clientWidth,
    };
  });
}
