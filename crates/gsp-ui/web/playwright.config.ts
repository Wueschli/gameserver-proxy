import { defineConfig } from "@playwright/test";

// Browser-level smoke tests against the real built UI served by `vite preview`.
// The gsp-ui backend is stubbed with page.route() in each test (see e2e/), so
// no Rust process is needed. In CI the build already ran as its own step, so
// the server only previews it. Set PW_CHROMIUM to use a pre-installed Chromium
// instead of the one `npx playwright install chromium` fetches.
export default defineConfig({
  testDir: "./e2e",
  fullyParallel: true,
  reporter: process.env.CI ? "github" : "list",
  use: {
    baseURL: "http://127.0.0.1:4173",
    launchOptions: process.env.PW_CHROMIUM ? { executablePath: process.env.PW_CHROMIUM } : {},
  },
  webServer: {
    command: `${process.env.CI ? "" : "npm run build && "}npx vite preview --host 127.0.0.1 --port 4173 --strictPort`,
    url: "http://127.0.0.1:4173",
    reuseExistingServer: !process.env.CI,
    timeout: 120_000,
  },
});
