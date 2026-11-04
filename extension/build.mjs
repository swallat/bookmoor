import { build } from "esbuild";
import { cpSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";

// One bundle, two packages: dist/ for Chrome (service worker) and
// dist-firefox/ for Firefox, which has no service workers and runs the
// background script as an event page instead.
await build({
  entryPoints: { background: "src/background.ts", options: "src/options.ts" },
  outdir: "dist",
  bundle: true,
  format: "esm",
  target: "es2022",
  sourcemap: true,
});
cpSync("manifest.json", "dist/manifest.json");
cpSync("src/options.html", "dist/options.html");
cpSync("icons", "dist/icons", { recursive: true });

mkdirSync("dist-firefox", { recursive: true });
cpSync("dist", "dist-firefox", { recursive: true });
const manifest = JSON.parse(readFileSync("manifest.json", "utf8"));
manifest.background = { scripts: ["background.js"], type: "module" };
manifest.browser_specific_settings = {
  gecko: {
    id: "bookmoor@wallat.de",
    strict_min_version: "128.0",
    data_collection_permissions: { required: ["bookmarksInfo"] },
  },
};
writeFileSync("dist-firefox/manifest.json", JSON.stringify(manifest, null, 2) + "\n");

// Test-only packages for the end-to-end tests (e2e/): the same code, with the
// local test server's origin granted up front, because a browser driven by a
// script cannot click through the permission prompt of the options page.
if (process.env.BOOKMOOR_E2E) {
  for (const dir of ["dist", "dist-firefox"]) {
    const out = `${dir.replace("dist", "dist-e2e")}`;
    cpSync(dir, out, { recursive: true });
    const m = JSON.parse(readFileSync(`${dir}/manifest.json`, "utf8"));
    m.host_permissions = ["http://127.0.0.1/*"];
    // geckodriver has no access to WebExtension APIs; the bridge lets a test
    // call them through the DOM (strings only).
    cpSync("e2e/bridge.js", `${out}/e2e-bridge.js`);
    const html = readFileSync(`${out}/options.html`, "utf8");
    writeFileSync(`${out}/options.html`, html.replace("</body>", '  <script src="e2e-bridge.js"></script>\n</body>'));
    writeFileSync(`${out}/manifest.json`, JSON.stringify(m, null, 2) + "\n");
  }
}
console.log(
  "built -> dist/ (Chrome: chrome://extensions > Load unpacked) and dist-firefox/ (Firefox: about:debugging > Load Temporary Add-on > manifest.json)",
);
