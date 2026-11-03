const path = require("path");
const { writeFixtures } = require("./fixtures");

// Regenerate fixture images before the web server (and tests) start.
module.exports = async function globalSetup() {
  const dir = path.join(__dirname, ".fixtures");
  const names = writeFixtures(dir);
  console.log(`e2e fixtures: ${names.length} images in ${dir}`);
};
