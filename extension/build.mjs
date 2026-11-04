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
console.log(
  "built -> dist/ (Chrome: chrome://extensions > Load unpacked) and dist-firefox/ (Firefox: about:debugging > Load Temporary Add-on > manifest.json)",
);
