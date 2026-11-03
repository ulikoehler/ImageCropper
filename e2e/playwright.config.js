const path = require("path");
const { defineConfig } = require("@playwright/test");

const BINARY =
  process.env.IMAGECROPPER_BIN ||
  path.join(__dirname, "..", "target", "debug", "imagecropper");
const PORT = 8790;
const FIXTURE_DIR = path.join(__dirname, ".fixtures");
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
  webServer: {
    command: `node ${JSON.stringify(path.join(__dirname, "fixtures.js"))} && ${JSON.stringify(BINARY)} album --serve=127.0.0.1:${PORT} --auth=none -G -m ${JSON.stringify(MOVE_DIR)} ${JSON.stringify(FIXTURE_DIR)}`,
    url: `http://127.0.0.1:${PORT}/healthz`,
    reuseExistingServer: !process.env.CI,
    timeout: 120_000,
  },
});
