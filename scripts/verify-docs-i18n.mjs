import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const repositoryRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const docsRoot = path.join(repositoryRoot, "docs");
const english = read("index.html");
const korean = read("ko/index.html");
const css = read("styles.css");
const javascript = read("app.js");

function read(relativePath) {
  return fs.readFileSync(path.join(docsRoot, relativePath), "utf8");
}

function openingTag(html, tagName) {
  const match = html.match(new RegExp(`<${tagName}\\b[^>]*>`, "i"));
  assert.ok(match, `missing <${tagName}> opening tag`);
  return match[0];
}

function attribute(tag, name) {
  const match = tag.match(new RegExp(`\\b${name}="([^"]*)"`, "i"));
  assert.ok(match, `missing ${name} on ${tag}`);
  return match[1];
}

function tagAttribute(html, tagName, name) {
  return attribute(openingTag(html, tagName), name);
}

function sectionIds(html) {
  return [...html.matchAll(/<section\b[^>]*\bid="([^"]+)"/gi)].map((match) => match[1]);
}

function unresolvedAnchors(html) {
  const ids = new Set([...html.matchAll(/\sid="([^"]+)"/g)].map((match) => match[1]));
  return [...html.matchAll(/href="#([^"]+)"/g)]
    .map((match) => match[1])
    .filter((target) => !ids.has(target));
}

function linkTags(html) {
  return [...html.matchAll(/<link\b[^>]*>/gi)].map((match) => match[0]);
}

function assertCanonical(html, expectedHref) {
  const canonical = linkTags(html).find((tag) => /\brel="canonical"/i.test(tag));
  assert.ok(canonical, "missing canonical link");
  assert.equal(attribute(canonical, "href"), expectedHref);
}

function assertAlternateLanguages(html, expectedLanguages) {
  const languages = linkTags(html)
    .filter((tag) => /\brel="alternate"/i.test(tag))
    .map((tag) => attribute(tag, "hreflang"));
  assert.deepEqual(languages.sort(), [...expectedLanguages].sort());
}

function assertRuntimeStrings(html, language) {
  const body = openingTag(html, "body");
  const names = [
    "data-theme-light-label",
    "data-theme-dark-label",
    "data-search-empty-template",
    "data-copy-success",
  ];
  for (const name of names) {
    assert.ok(attribute(body, name).trim(), `${language} ${name} must not be empty`);
  }
  const values = names.map((name) => attribute(body, name)).join(" ");
  if (language === "en") assert.match(values, /[A-Za-z]/);
  if (language === "ko") assert.match(values, /[가-힣]/);
}

function assertNoRootRelativeAssets(html) {
  const rootRelative = [...html.matchAll(/(?:href|src)="(\/[^"]*)"/g)].map(
    (match) => match[1],
  );
  assert.deepEqual(rootRelative, []);
}

assert.equal(tagAttribute(english, "html", "lang"), "en");
assert.equal(tagAttribute(korean, "html", "lang"), "ko");
assert.deepEqual(sectionIds(english), sectionIds(korean));
assert.equal(sectionIds(english).length, 14);
assert.equal(unresolvedAnchors(english).length, 0);
assert.equal(unresolvedAnchors(korean).length, 0);
assert.match(english, /href="\.\/ko\/"/);
assert.match(korean, /href="\.\.\/"/);
assert.match(english, /href="\.\/styles\.css"/);
assert.match(english, /src="\.\/app\.js"/);
assert.match(korean, /href="\.\.\/styles\.css"/);
assert.match(korean, /src="\.\.\/app\.js"/);
assertCanonical(english, "https://bonzonkim.github.io/sampler-tui/");
assertCanonical(korean, "https://bonzonkim.github.io/sampler-tui/ko/");
assertAlternateLanguages(english, ["en", "ko", "x-default"]);
assertAlternateLanguages(korean, ["en", "ko", "x-default"]);
assertRuntimeStrings(english, "en");
assertRuntimeStrings(korean, "ko");
assertNoRootRelativeAssets(english);
assertNoRootRelativeAssets(korean);
assert.doesNotMatch(css, /status-dot|terminal-dots|mode-dot|border-radius:\s*50%/);
assert.match(javascript, /uiStrings/);
assert.ok(fs.existsSync(path.join(docsRoot, ".nojekyll")));

console.log("docs i18n contract: OK");
