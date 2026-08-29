#!/usr/bin/env node
"use strict";

const fs = require("node:fs");
const path = require("node:path");

const root = path.resolve(__dirname, "..");
const read = (relative) => fs.readFileSync(path.join(root, relative), "utf8");

function cargoVersion(relative) {
  const source = read(relative);
  const packageSection = source.match(/^\[package\]\s*$([\s\S]*?)(?=^\[|\z)/m);
  const match = packageSection?.[1].match(/^version\s*=\s*"([^"]+)"\s*$/m);
  if (!match) throw new Error(`${relative} has no package version`);
  return match[1];
}

function lockPackageVersion(relative, name) {
  const source = read(relative);
  const packages = source.split(/^\[\[package\]\]\s*$/m).slice(1);
  const section = packages.find((candidate) => new RegExp(`^name\\s*=\\s*"${name}"\\s*$`, "m").test(candidate));
  const match = section?.match(/^version\s*=\s*"([^"]+)"\s*$/m);
  if (!match) throw new Error(`${relative} has no ${name} package version`);
  return match[1];
}

const versions = new Map([
  ["Cargo.toml", cargoVersion("Cargo.toml")],
  ["me-client/package.json", JSON.parse(read("me-client/package.json")).version],
  ["me-client/src-tauri/Cargo.toml", cargoVersion("me-client/src-tauri/Cargo.toml")],
  ["me-client/src-tauri/tauri.conf.json", JSON.parse(read("me-client/src-tauri/tauri.conf.json")).version],
  ["me-client/src-tauri/Cargo.lock", lockPackageVersion("me-client/src-tauri/Cargo.lock", "me-client")],
]);

const productVersion = versions.get("Cargo.toml");
if (!/^[0-9]+\.[0-9]+\.[0-9]+(?:[-+][0-9A-Za-z.-]+)?$/.test(productVersion)) {
  throw new Error(`invalid ME product version: ${productVersion}`);
}
for (const [file, version] of versions) {
  if (version !== productVersion) {
    throw new Error(`${file} version ${version} does not match ME ${productVersion}`);
  }
}

if (process.argv.length > 2 && process.argv[2] !== "--print") {
  throw new Error(`unsupported argument: ${process.argv[2]}`);
}
process.stdout.write(`${productVersion}\n`);
