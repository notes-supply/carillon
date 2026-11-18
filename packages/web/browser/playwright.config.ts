import { join } from "node:path";
import { defineConfig } from "@playwright/test";
import type { Options } from "./fixtures.ts";

export default defineConfig<object, Options>({
  testDir: ".",
  outputDir: join(import.meta.dirname, "..", "test-results"),
  fullyParallel: true,
  forbidOnly: true,
  retries: 0,
  timeout: 30_000,
  expect: { timeout: 10_000 },
  reporter: "list",
  use: {
    site: join(import.meta.dirname, "..", "dist"),
    headless: true,
    locale: "en-GB",
    timezoneId: "UTC",
    viewport: { width: 1280, height: 800 },
    trace: "retain-on-failure",
  },
  projects: [
    { name: "chromium", use: { browserName: "chromium" } },
    { name: "webkit", use: { browserName: "webkit" } },
  ],
});
