import { defineConfig, devices } from "@playwright/test";

// Everything the browser tests need is thrown away between runs: the database
// lives in .e2e-data, which `make e2e` deletes first. That matters beyond
// hygiene - the server only creates the bootstrap admin when no user exists, so
// a leftover database means no account to sign in with.
//
// .e2e-data is deliberately separate from .data, which is the developer's own
// instance and which `make clean` does delete.
const E2E_SECRET = "e2e-only-not-a-secret";

// Deliberately not 8080 and 5173. A developer running `make dev` and
// `make web-dev` holds those, and playwright's reuseExistingServer would then
// silently point the suite at their instance - writing test users and
// repositories into .data, and failing confusingly because the bootstrap admin
// there has a different password. On its own ports, `make e2e` works whether
// or not anything else is running.
const API_PORT = 8090;
const UI_PORT = 5273;
const API_URL = `http://127.0.0.1:${API_PORT}`;

export default defineConfig({
  testDir: "./test/e2e",
  // Signs the accounts in once and leaves their cookies on disk.
  globalSetup: "./test/e2e/setup/global-setup.ts",
  fullyParallel: true,
  forbidOnly: Boolean(process.env.CI),
  retries: 0,
  workers: process.env.CI ? 1 : undefined,
  reporter: process.env.CI ? "github" : "list",
  use: {
    baseURL: `http://127.0.0.1:${UI_PORT}`,
    trace: "on-first-retry",
    screenshot: "only-on-failure",
    video: "retain-on-failure",
  },
  projects: [
    // Chromium only. This is an internal console; run time is worth more here
    // than cross-browser coverage.
    { name: "chromium", use: { ...devices["Desktop Chrome"] } },
  ],
  webServer: [
    {
      // The API. Run the Rust binary from the crate root.
      command: "cd .. && cargo run --locked --bin forklift",
      url: `${API_URL}/readyz`,
      env: {
        FORKLIFT_DATA_DIR: "./.e2e-data",
        FORKLIFT_HTTP_ADDR: `127.0.0.1:${API_PORT}`,
        FORKLIFT_METRICS_ADDR: "127.0.0.1:0",
        FORKLIFT_PPROF_ADDR: "127.0.0.1:0",
        FORKLIFT_BOOTSTRAP_ADMIN_USER: "e2e-admin",
        // Set rather than read back from the startup log: BootstrapAdmin only
        // generates a password when none is given, and parsing a log line
        // breaks the moment its format changes.
        FORKLIFT_BOOTSTRAP_ADMIN_PASSWORD: E2E_SECRET,
        // Without this the server invents an ephemeral secret, and every saved
        // storageState becomes invalid the moment it restarts.
        FORKLIFT_SESSION_SECRET: E2E_SECRET,
        FORKLIFT_LOG_LEVEL: "warn",
        FORKLIFT_RBAC_POLICY_FILE: "./web/test/e2e/setup/rbac-policy.csv",
      },
      reuseExistingServer: false,
      timeout: 600_000,
    },
    {
      command: `pnpm dev --host 127.0.0.1 --port ${UI_PORT} --strictPort`,
      url: `http://127.0.0.1:${UI_PORT}/login`,
      env: { FORKLIFT_API_TARGET: API_URL },
      reuseExistingServer: false,
      timeout: 120_000,
    },
  ],
});
