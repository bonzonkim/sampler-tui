import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const repositoryRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const read = (relativePath) =>
  fs.readFileSync(path.join(repositoryRoot, relativePath), "utf8");

const requiredFiles = [
  "dist-workspace.toml",
  ".github/workflows/release.yml",
  "docs/index.html",
  "docs/ko/index.html",
  "docs/app.js",
];

for (const relativePath of requiredFiles) {
  assert.ok(fs.existsSync(path.join(repositoryRoot, relativePath)), `missing ${relativePath}`);
}

const cargo = read("Cargo.toml");
const appCargo = read("crates/sampler-tui/Cargo.toml");
const dist = read("dist-workspace.toml");
const workflow = read(".github/workflows/release.yml");
const english = read("docs/index.html");
const korean = read("docs/ko/index.html");
const javascript = read("docs/app.js");

assert.match(cargo, /repository = "https:\/\/github\.com\/bonzonkim\/sampler-tui"/);
assert.match(cargo, /homepage = "https:\/\/bonzonkim\.github\.io\/sampler-tui\/"/);
assert.match(cargo, /description = "[^"]+"/);
assert.match(appCargo, /repository\.workspace = true/);
assert.match(appCargo, /homepage\.workspace = true/);
assert.match(appCargo, /description\.workspace = true/);

assert.match(dist, /cargo-dist-version = "0\.32\.0"/);
assert.match(dist, /ci = \["github"\]/);
assert.match(dist, /installers = \["shell", "homebrew"\]/);
assert.match(dist, /tap = "bonzonkim\/homebrew-tap"/);
assert.match(dist, /checksum = "sha256"/);
assert.match(dist, /"aarch64-apple-darwin"/);
assert.match(dist, /"x86_64-apple-darwin"/);
assert.match(dist, /"x86_64-unknown-linux-gnu"/);
assert.match(dist, /\[dist\.dependencies\.apt\]/);
assert.match(dist, /libasound2-dev = "\*"/);

assert.match(workflow, /tags:/);
assert.match(workflow, /v\[0-9\]\+\.\[0-9\]\+\.\[0-9\]\+/);
assert.match(workflow, /HOMEBREW_TAP_TOKEN/);

for (const [locale, html] of [
  ["en", english],
  ["ko", korean],
]) {
  assert.match(html, /id="install-downloads"/, `${locale}: missing download section`);
  assert.match(html, /data-release-target="aarch64-apple-darwin"/);
  assert.match(html, /data-release-target="x86_64-apple-darwin"/);
  assert.match(html, /data-release-target="x86_64-unknown-linux-gnu"/);
  assert.match(html, /brew install bonzonkim\/tap\/sampler-tui/);
  assert.match(html, /sampler-tui-installer\.sh/);
}

assert.match(javascript, /api\.github\.com\/repos\/bonzonkim\/sampler-tui\/releases\/latest/);
assert.match(javascript, /\[data-release-target\]/);
assert.match(javascript, /release\.assets/);

console.log("release distribution contract: OK");
