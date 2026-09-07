const { spawnSync } = require("node:child_process");
const { readFileSync, writeFileSync, copyFileSync } = require("node:fs");
const { createHash } = require("node:crypto");
const { resolve } = require("node:path");

const root = resolve(__dirname, "..");
const result = spawnSync("cargo", [
  "build", "--manifest-path", "transport/Cargo.toml", "--target", "wasm32-unknown-unknown",
  "--target-dir", "transport/target", "--release", "--offline", "--locked",
], { cwd: root, stdio: "inherit" });
if (result.error) throw result.error;
if (result.status !== 0) process.exit(result.status || 1);
copyFileSync(resolve(root, "transport/target/wasm32-unknown-unknown/release/me_transport.wasm"), resolve(root, "src/webui/transport.wasm"));
const files = ["transport/Cargo.toml", "transport/Cargo.lock", "transport/src/lib.rs", "transport/src/wasm.rs", "src/webui/transport.wasm"];
const checksums = files.map((path) => `${createHash("sha256").update(readFileSync(resolve(root, path))).digest("hex")}  ${path}`).join("\n") + "\n";
writeFileSync(resolve(root, "transport/wasm.sha256"), checksums);
console.log("built and bound the browser transport to its Rust sources");
