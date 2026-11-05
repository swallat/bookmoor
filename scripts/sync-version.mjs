#!/usr/bin/env node
// One release number for everything. The single source of truth is
// [workspace.package] version in Cargo.toml (bookmoor-core and bookmoor-server
// inherit it); extension/package.json, package-lock.json and manifest.json
// are copies of it.
//
//   node scripts/sync-version.mjs            write the version into the extension files
//   node scripts/sync-version.mjs --check    fail if they differ (CI)
//   node scripts/sync-version.mjs --tag v1.2.3   fail unless the Git tag matches (release)
//   node scripts/sync-version.mjs --print    print the version
//   node scripts/sync-version.mjs --prerelease   print true for 0.x (beta), else false
//   node scripts/sync-version.mjs --set 1.2.3    change Cargo.toml, then sync
import { readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const cargoPath = join(root, "Cargo.toml");
const SEMVER = /^\d+\.\d+\.\d+$/;
// Before releases were coordinated, CI signed Firefox builds as 0.<run>.<attempt>
// (highest: 0.9.1, run 9). Versions only ever go up for stores and Mozilla, so
// every release must be higher than this.
const FLOOR = [0, 9, 1];
const cmp = (a, b) => a.reduce((d, x, i) => d || x - b[i], 0);

// [workspace.package] up to the next table header (a line starting with "["),
// so values like authors = ["x"] above `version` are fine.
const VERSION_RE = /(^\[workspace\.package\]\n(?:(?!\[).*\n)*?version\s*=\s*")([^"]+)(")/m;

function readCargoVersion() {
  const m = readFileSync(cargoPath, "utf8").match(VERSION_RE);
  if (!m) throw new Error("no [workspace.package] version in Cargo.toml");
  return m[2];
}

const args = process.argv.slice(2);
if (args.includes("--set") && (args.includes("--check") || args.includes("--tag"))) {
  throw new Error("--set cannot be combined with --check or --tag");
}
let version = readCargoVersion();

const setIdx = args.indexOf("--set");
if (setIdx >= 0) {
  const v = args[setIdx + 1];
  if (!SEMVER.test(v ?? "")) throw new Error(`--set needs X.Y.Z, got ${v}`);
  const old = readFileSync(cargoPath, "utf8");
  writeFileSync(cargoPath, old.replace(VERSION_RE, `$1${v}$3`));
  version = v;
  console.log(`Cargo.toml -> ${v} (run "cargo update --workspace" to refresh Cargo.lock)`);
}
if (!SEMVER.test(version)) {
  // Browsers only accept dotted integers, so no -rc.1 suffixes.
  throw new Error(`release version must be X.Y.Z, got "${version}"`);
}
if (cmp(version.split(".").map(Number), FLOOR) <= 0) {
  throw new Error(`version ${version} must be higher than ${FLOOR.join(".")}, which was already published as a dev build`);
}
if (args.includes("--prerelease")) {
  // 0.x is beta: GitHub pre-release, no Docker "latest".
  console.log(version.startsWith("0.") ? "true" : "false");
  process.exit(0);
}
if (args.includes("--print")) {
  console.log(version);
  process.exit(0);
}
const tagIdx = args.indexOf("--tag");
if (tagIdx >= 0) {
  const tag = args[tagIdx + 1] ?? "";
  if (tag.replace(/^refs\/tags\//, "") !== `v${version}`) {
    console.error(`Git tag "${tag}" does not match the version ${version} in Cargo.toml (expected v${version}).`);
    process.exit(1);
  }
}

const check = args.includes("--check");
let bad = false;
function sync(rel, edit) {
  const path = join(root, rel);
  const json = JSON.parse(readFileSync(path, "utf8"));
  const before = JSON.stringify(json);
  edit(json);
  if (JSON.stringify(json) === before) return;
  if (check) {
    console.error(`${rel} is out of step with Cargo.toml (${version}). Run: node scripts/sync-version.mjs`);
    bad = true;
  } else {
    writeFileSync(path, JSON.stringify(json, null, 2) + "\n");
    console.log(`${rel} -> ${version}`);
  }
}
sync("extension/package.json", (j) => (j.version = version));
sync("extension/package-lock.json", (j) => {
  j.version = version;
  if (j.packages?.[""]) j.packages[""].version = version;
});
sync("extension/manifest.json", (j) => (j.version = version));
if (bad) process.exit(1);
