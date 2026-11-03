const path = require("path");
const { defineConfig } = require("@playwright/test");

const BINARY =
  process.env.IMAGECROPPER_BIN ||
  path.join(__dirname, "..", "target", "debug", "imagecropper");
const PORT = 8790;
const PORT_RB = 8791;
const FIXTURE_DIR = path.join(__dirname, ".fixtures");
const FIXTURE_DIR_RB = path.join(__dirname, ".fixtures-rb");
const MOVE_DIR = path.join(__dirname, ".move-target");

module.exports = defineConfig({
  testDir: "./tests",
  timeout: 60_000,
  fullyParallel: false,
  workers: 1, // the fixture dir + server are shared
  reporter: [["list"], ["html", { open: "never" }]],
  globalSetup: "./global-setup.js",
  use: {
    baseURL: `http://127.0.0.1:${PORT}`,
    viewport: { width: 1280, height: 800 },
  },
  projects: [{ name: "chromium", use: { browserName: "chromium" } }],
  webServer: [
    {
      command: `node ${JSON.stringify(path.join(__dirname, "fixtures.js"))} && ${JSON.stringify(BINARY)} album --serve=127.0.0.1:${PORT} --auth=none -G -m ${JSON.stringify(MOVE_DIR)} ${JSON.stringify(FIXTURE_DIR)}`,
      url: `http://127.0.0.1:${PORT}/healthz`,
      reuseExistingServer: !process.env.CI,
      timeout: 120_000,
    },
    {
      // Second server running rubber-band drag-select for parity coverage.
      command: `node ${JSON.stringify(path.join(__dirname, "fixtures.js"))} ${JSON.stringify(FIXTURE_DIR_RB)} && ${JSON.stringify(BINARY)} album --serve=127.0.0.1:${PORT_RB} --auth=none --mouse-select-mode=rubber-band -G ${JSON.stringify(FIXTURE_DIR_RB)}`,
      url: `http://127.0.0.1:${PORT_RB}/healthz`,
      reuseExistingServer: !process.env.CI,
      timeout: 120_000,
    },
  ],
});
