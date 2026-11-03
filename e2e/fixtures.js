// Test fixture images: named after their solid colour so tests can map
// tile → expected pixel colour directly.
const fs = require("fs");
const path = require("path");
const { png } = require("./png");

const FIXTURES = [
  ["red", 200, 150, [220, 30, 30]],
  ["green", 220, 140, [30, 180, 30]],
  ["blue", 180, 160, [30, 60, 220]],
  ["purple", 240, 120, [160, 30, 200]],
  ["yellow", 160, 160, [230, 200, 30]],
  ["cyan", 190, 130, [30, 200, 200]],
  ["orange", 210, 150, [240, 140, 20]],
  ["gray", 170, 170, [130, 130, 130]],
];

const EXPECTED = Object.fromEntries(
  FIXTURES.map(([name]) => [name, FIXTURES.find(f => f[0] === name)[3]])
);

function writeFixtures(dir) {
  fs.rmSync(dir, { recursive: true, force: true });
  fs.mkdirSync(dir, { recursive: true });
  for (const [name, w, h, rgb] of FIXTURES) {
    fs.writeFileSync(path.join(dir, `${name}.png`), png(w, h, rgb));
  }
  return FIXTURES.map(([name]) => name);
}

module.exports = { writeFixtures, FIXTURES, EXPECTED };

// `node fixtures.js [dir]` — webServer runs this before the binary starts
// (Playwright launches webServer ahead of globalSetup).
if (require.main === module) {
  const dir = process.argv[2] || path.join(__dirname, ".fixtures");
  const names = writeFixtures(dir);
  // Reset the -m target dir only for the default fixtures run — the
  // rubber-band server's invocation would otherwise wipe the dir the main
  // server already created.
  if (!process.argv[2]) {
    fs.rmSync(path.join(__dirname, ".move-target"), { recursive: true, force: true });
  }
  console.log(`fixtures: wrote ${names.join(", ")} to ${dir}`);
}
