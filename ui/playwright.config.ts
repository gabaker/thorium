import { defineConfig, devices } from '@playwright/test';

// An alternate Chrome binary (for hosts where Playwright's own browser download is unavailable);
// it is launched without the sandbox when the host restricts unprivileged user namespaces.
const CHROME = process.env.THORIUM_CHROME;
// A UI that is already being served; Playwright then starts no dev server of its own.
const UI_URL = process.env.THORIUM_UI_URL;

export default defineConfig({
  testDir: './e2e',
  timeout: 60_000,
  retries: 0,
  projects: [
    {
      name: 'chromium',
      use: {
        ...devices['Desktop Chrome'],
        ...(CHROME
          ? {
              launchOptions: {
                executablePath: CHROME,
                args: ['--no-sandbox', '--disable-gpu', '--disable-dev-shm-usage'],
              },
            }
          : {}),
      },
    },
  ],
  use: {
    baseURL: UI_URL || 'http://localhost:8000',
    headless: true,
    screenshot: 'off',
    trace: 'retain-on-failure',
  },
  webServer: UI_URL
    ? undefined
    : {
        command: 'npm run dev',
        port: 8000,
        reuseExistingServer: true,
        env: {
          THORIUM_API_URL: process.env.THORIUM_API_URL || 'http://localhost:8080',
        },
      },
});
