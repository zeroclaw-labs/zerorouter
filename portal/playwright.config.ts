import { defineConfig } from '@playwright/test'

export default defineConfig({
  testDir: './e2e',
  globalSetup: './e2e/global-setup.mjs',
  globalTeardown: './e2e/global-teardown.mjs',
  // One worker: the specs share a router, a database, and one e2e user.
  workers: 1,
  use: {
    baseURL: 'http://127.0.0.1:9410',
    trace: 'retain-on-failure',
  },
  reporter: [['list']],
})
