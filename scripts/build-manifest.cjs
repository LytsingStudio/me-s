#!/usr/bin/env node

const { createHash } = require("node:crypto");
const { readFileSync, writeFileSync, statSync, readdirSync } = require("node:fs");
const { join } = require("node:path");

const PACKAGE_ASSETS = [
  "ME-macos-universal.pkg",
  "ME-windows-x86_64-setup.exe",
  "ME-linux-x86_64.run",
  "ME-linux-arm64.run",
];
const CHECKSUMS = "SHA256SUMS";
const MANIFEST = "BUILD-MANIFEST.json";

function fail(message) {
  console.error(`error: ${message}`);
  process.exit(1);
}

function sha256(path) {
  return createHash("sha256").update(readFileSync(path)).digest("hex");
}

function fileRecord(directory, name) {
  const path = join(directory, name);
  const stat = statSync(path);
  if (!stat.isFile() || stat.size === 0) fail(`missing or empty build output: ${name}`);
  return { name, size: stat.size, sha256: sha256(path) };
}

function expectedFiles(includeManifest) {
  return [...PACKAGE_ASSETS, CHECKSUMS, ...(includeManifest ? [MANIFEST] : [])].sort();
}

function assertFileSet(directory, includeManifest) {
  const actual = readdirSync(directory, { withFileTypes: true })
    .filter((entry) => entry.isFile())
    .map((entry) => entry.name)
    .sort();
  const expected = expectedFiles(includeManifest);
  if (JSON.stringify(actual) !== JSON.stringify(expected)) {
    fail(`unexpected build output set\nexpected: ${expected.join(", ")}\nactual: ${actual.join(", ")}`);
  }
}

function parseDirty(value) {
  if (value === "true") return true;
  if (value === "false") return false;
  fail(`source dirty flag must be true or false; received ${value}`);
}

function createManifest(directory, version, commit, dirty) {
  assertFileSet(directory, false);
  const manifest = {
    schema: 1,
    version,
    commit,
    source_dirty: parseDirty(dirty),
    assets: PACKAGE_ASSETS.map((name) => fileRecord(directory, name)),
    checksums: fileRecord(directory, CHECKSUMS),
  };
  writeFileSync(join(directory, MANIFEST), `${JSON.stringify(manifest, null, 2)}\n`);
  assertFileSet(directory, true);
}

function verifyManifest(directory, version, commit, dirtyExpectation) {
  assertFileSet(directory, true);
  const manifest = JSON.parse(readFileSync(join(directory, MANIFEST), "utf8"));
  if (manifest.schema !== 1) fail("unsupported build manifest schema");
  if (manifest.version !== version) fail(`build manifest version is ${manifest.version}; expected ${version}`);
  if (manifest.commit !== commit) fail(`build manifest commit is ${manifest.commit}; expected ${commit}`);
  if (dirtyExpectation !== "any" && manifest.source_dirty !== parseDirty(dirtyExpectation)) {
    fail(`build manifest source_dirty is ${manifest.source_dirty}; expected ${dirtyExpectation}`);
  }
  const actualAssets = PACKAGE_ASSETS.map((name) => fileRecord(directory, name));
  if (JSON.stringify(manifest.assets) !== JSON.stringify(actualAssets)) fail("build asset metadata no longer matches dist");
  const actualChecksums = fileRecord(directory, CHECKSUMS);
  if (JSON.stringify(manifest.checksums) !== JSON.stringify(actualChecksums)) fail("SHA256SUMS metadata no longer matches dist");
}

const [command, directory, version, commit, dirty = "any"] = process.argv.slice(2);
if (!command || !directory || !version || !commit || !["create", "verify"].includes(command)) {
  fail("usage: build-manifest.cjs <create|verify> <dist> <version> <commit> [true|false|any]");
}
if (command === "create") createManifest(directory, version, commit, dirty);
else verifyManifest(directory, version, commit, dirty);
