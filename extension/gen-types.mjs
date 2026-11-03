// Generates src/wire.ts from ../schema/wire.schema.json.
// `node gen-types.mjs --check` fails if the checked-in file is stale.
import { compileFromFile } from "json-schema-to-typescript";
import { readFileSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

const out = new URL("./src/wire.ts", import.meta.url);
const banner =
  "// GENERATED from schema/wire.schema.json by `npm run gen:types`. Do not edit.\n" +
  "/* eslint-disable */";
const ts = await compileFromFile(fileURLToPath(new URL("../schema/wire.schema.json", import.meta.url)), {
  bannerComment: banner,
  additionalProperties: false,
  style: { singleQuote: false },
});
if (process.argv.includes("--check")) {
  // Windows checkouts may use CRLF line endings.
  if (readFileSync(out, "utf8").replace(/\r\n/g, "\n") !== ts) {
    console.error("src/wire.ts is stale; run `npm run gen:types`");
    process.exit(1);
  }
} else {
  writeFileSync(out, ts);
}
