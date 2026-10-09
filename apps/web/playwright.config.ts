import { defineConfig, devices } from '@playwright/test';

export default defineConfig({
  testDir: './tests',
  fullyParallel: true,
  forbidOnly: !!process.env.CI,
  retries: 0,
  reporter: 'list',
  use: { baseURL: 'http://127.0.0.1:4174', trace: 'retain-on-failure' },
  projects: [{ name: 'chromium', use: { ...devices['Desktop Chrome'] } }],
  webServer: [
    {
      command: 'npm run dev -- --port 4174',
      url: 'http://127.0.0.1:4174',
      reuseExistingServer: false,
      timeout: 30_000,
    },
    {
      command: 'node tests/fixture-server.mjs',
      url: 'http://127.0.0.1:4318/health',
      reuseExistingServer: false,
      timeout: 10_000,
    },
  ],
});
