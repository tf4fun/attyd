import { defineConfig, devices } from "@playwright/test";

const localChannel = process.env.CI ? undefined : "chrome";

export default defineConfig({
  testDir: "./tests/browser",
  testMatch: "**/*.pw.ts",
  fullyParallel: false,
  forbidOnly: Boolean(process.env.CI),
  retries: process.env.CI ? 1 : 0,
  workers: 1,
  reporter: process.env.CI ? [["github"], ["html", { open: "never" }]] : "list",
  timeout: 30_000,
  expect: { timeout: 8_000 },
  use: {
    baseURL: "http://127.0.0.1:7332",
    channel: localChannel,
    trace: "retain-on-failure",
    screenshot: "only-on-failure",
  },
  webServer: {
    command: "npm run test:ui:serve -- 7332",
    url: "http://127.0.0.1:7332/api/health",
    reuseExistingServer: !process.env.CI,
    timeout: 30_000,
  },
  projects: [{
    name: "chromium",
    use: { ...devices["Desktop Chrome"] },
  }],
});
