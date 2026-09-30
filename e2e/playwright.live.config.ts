import { defineConfig, devices } from '@playwright/test';

// The live post-deploy smoke (#179): e2e/live/ against a DEPLOYED host, signing
// in through the real IdP. Run by scripts/smoke-web-live.sh after the dev
// deploy (.woodpecker/deploy.yml → smoke-web-live-*).
//
// Deliberately separate from playwright.config.ts, not a project inside it:
// that config's global setup mints a session through the mock IdP's
// client_credentials shortcut and seeds every context with it, which is exactly
// what this suite must not have. Its testDir is ./tests, so nothing here ever
// runs in the pre-deploy mock-IdP suite, and nothing there runs here.
const baseURL = process.env.BASE_URL;
if (!baseURL) {
  throw new Error('BASE_URL environment variable is required');
}

export default defineConfig({
  testDir: './live',
  fullyParallel: false,
  workers: 1,
  // One retry absorbs a single transient (the IdP or Traefik hiccuping right
  // after a redeploy) without hiding a real break: a failure that repeats still
  // fails the step and blocks the release tag.
  retries: process.env.CI ? 1 : 0,
  timeout: 120_000,
  use: {
    baseURL,
    headless: true,
    // Only for pointing the spec at the local e2e stack's self-signed app
    // (docs/DEPLOY.md). Against a deployed host a certificate error is a real
    // failure and must stay one.
    ignoreHTTPSErrors: process.env.LIVE_SMOKE_IGNORE_HTTPS_ERRORS === '1',
    screenshot: 'only-on-failure',
    // Off: a trace records every `fill`, including the password.
    trace: 'off',
    video: 'off',
  },
  projects: [
    {
      name: 'chromium',
      use: { ...devices['Desktop Chrome'] },
    },
  ],
  reporter: [['list']],
  outputDir: './test-results-live',
});
