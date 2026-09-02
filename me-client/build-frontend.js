import { copyFile, cp, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const clientRoot = dirname(fileURLToPath(import.meta.url));
const repositoryRoot = resolve(clientRoot, "..");
const webuiRoot = resolve(repositoryRoot, "src/webui");
const vendorRoot = resolve(webuiRoot, "vendor");
const outputRoot = resolve(clientRoot, "frontend-dist");

await rm(outputRoot, { recursive: true, force: true });
await mkdir(outputRoot, { recursive: true });

const assets = [
  [resolve(webuiRoot, "app.js"), "app.js"],
  [resolve(webuiRoot, "style.css"), "style.css"],
  [resolve(webuiRoot, "theme.js"), "theme.js"],
  [resolve(webuiRoot, "theme.css"), "theme.css"],
  [resolve(webuiRoot, "markdown.js"), "markdown.js"],
  [resolve(webuiRoot, "transcript.js"), "transcript.js"],
  [resolve(webuiRoot, "tool-presenters.js"), "tool-presenters.js"],
  [resolve(webuiRoot, "edb-cache.js"), "edb-cache.js"],
  [resolve(webuiRoot, "file-manager.js"), "file-manager.js"],
  [resolve(webuiRoot, "session-terminal.js"), "session-terminal.js"],
  [resolve(webuiRoot, "remote-control.js"), "remote-control.js"],
  [resolve(vendorRoot, "markdown-it.min.js"), "markdown-it.js"],
  [resolve(vendorRoot, "katex.min.js"), "katex.js"],
  [resolve(vendorRoot, "katex.min.css"), "katex.css"],
  [resolve(vendorRoot, "xterm.js"), "xterm.js"],
  [resolve(vendorRoot, "xterm.css"), "xterm.css"],
  [resolve(vendorRoot, "xterm-addon-fit.js"), "xterm-addon-fit.js"],
  [resolve(vendorRoot, "xterm-addon-unicode11.js"), "xterm-addon-unicode11.js"],
  [resolve(clientRoot, "client-runtime.js"), "runtime.js"],
  [resolve(clientRoot, "client.css"), "client.css"],
  [resolve(clientRoot, "app-icon.svg"), "app-icon.svg"],
  [resolve(clientRoot, "window-shadow.html"), "window-shadow.html"],
];
for (const [source, destination] of assets) {
  await copyFile(source, resolve(outputRoot, destination));
}
await cp(resolve(vendorRoot, "katex-fonts"), resolve(outputRoot, "fonts"), { recursive: true });

const sourceHtml = await readFile(resolve(webuiRoot, "index.html"), "utf8");
const html = sourceHtml
  .replace("<title>ME</title>", "<title>ME Client</title>")
  .replace("<link rel=\"stylesheet\" href=\"/theme.css\">", "<link rel=\"stylesheet\" href=\"/theme.css\">\n  <link rel=\"stylesheet\" href=\"/client.css\">")
  .replace("      <p>请输入访问密码。</p>", "      <p>输入服务地址和访问密码。</p>");
if (html === sourceHtml || !html.includes("/runtime.js") || !html.includes("login-endpoint")) {
  throw new Error("Shared WebUI entry structure changed; unable to assemble me-client frontend");
}
await writeFile(resolve(outputRoot, "index.html"), html, "utf8");
console.log(`assembled shared frontend at ${outputRoot}`);
