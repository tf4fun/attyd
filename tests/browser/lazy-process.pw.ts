import { expect, test as baseTest } from "@playwright/test";
import { join } from "node:path";
import { startRustTestServer } from "../../scripts/rust-test-server";
import { opsOf, trackApi } from "./api-requests";

const SESSION_ID = "lazy-process-session";
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

// Protocol v2 keeps only final messages in the log (S2, §10): a loaded turn's
// tools and thoughts are not part of any document, and no process page exists.
// The former paging tests ("pages of ten", "release after five minutes", the
// pagination footer through loading, timeout and retry) had nothing left to
// page; their replacement is that the history arrives without process and the
// page never asks for it. Running process layout stays covered below and in
// turn-collapse.pw.ts, before the turn ends.

test("loads history as final messages only and never requests execution details", async ({ page }) => {
  const api = trackApi(page);
  await page.goto("/sessions/lazy-process-session");
  const turn = page.locator(".conversation-turn").filter({ hasText: FINAL_ANSWER });
  await expect(turn.getByText(FINAL_ANSWER, { exact: true })).toBeVisible();
  await expect(turn.locator(".message-user")).toContainText("Inspect the recorded execution process.");
  expect(api.streams.map((url) => url.searchParams.getAll("scope"))).toContainEqual(
    expect.arrayContaining([`session/${SESSION_ID}`]),
  );
  const document = await (await page.request.get(`/api/v2/scopes/${encodeURIComponent(`session/${SESSION_ID}`)}`)).text();
  expect(document).toContain(FINAL_ANSWER);
  expect(document).not.toContain("HIDDEN_PROCESS_PAYLOAD_");
  expect(document).not.toContain("Inspect process item");
  await expect(turn.locator(".tool-card")).toHaveCount(0);
  await expect(turn.locator(".turn-process-count")).toHaveCount(0);
  await expect(turn.getByRole("button", { name: "Expand execution process", exact: true })).toHaveCount(0);
  await expect(page.getByText("Inspect process item 01", { exact: true })).toHaveCount(0);
  await page.reload();
  await expect(turn.getByText(FINAL_ANSWER, { exact: true })).toBeVisible();
  await expect(turn.locator(".tool-card")).toHaveCount(0);
  expect(api.process).toEqual([]);
  expect(api.v1).toEqual([]);
});

test("keeps task progress on the left and recovers composer space when the mobile plan is collapsed", async ({ page }, testInfo) => {
  await page.setViewportSize({ width: 390, height: 844 });
  const api = trackApi(page);
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
  await expect.poll(() => opsOf(api, "cancel", `session/${SESSION_ID}`)).toHaveLength(1);
  // The cancelled turn commits; its plan was process and leaves with it.
  await expect(page.getByRole("button", { name: "Stop current turn", exact: true })).toBeHidden();
  await expect(plan).toHaveCount(0);
  expect(api.v1).toEqual([]);
});
