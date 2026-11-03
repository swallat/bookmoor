import { build } from "esbuild";
import { cpSync, mkdirSync } from "node:fs";

mkdirSync("dist", { recursive: true });
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
console.log("built -> dist/ (load it via chrome://extensions > Load unpacked)");
