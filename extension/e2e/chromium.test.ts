// End-to-end: the real extension (dist-e2e/) in two real Chromium profiles
// talking to the real server binary. Needs `BOOKMOOR_E2E=1 npm run build`,
// `cargo build -p bookmoor-server` and a Chromium for playwright-core
// (`npx playwright-core install chromium`, or PLAYWRIGHT_BROWSERS_PATH).

import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { after, before, describe, it } from "node:test";
import { chromium, type BrowserContext, type Page, type Worker } from "playwright-core";
import { serverBinary, startServer, type Server } from "../src/testkit.ts";

const EXT = resolve(import.meta.dirname, "../dist-e2e");
const BIN = serverBinary();
const required = process.env.BOOKMOOR_E2E_REQUIRED === "1";

interface Profile {
  ctx: BrowserContext;
  worker: Worker;
  id: string;
  dir: string;
}

async function launch(): Promise<Profile> {
  const dir = mkdtempSync(join(tmpdir(), "bookmoor-chromium-"));
  const ctx = await chromium.launchPersistentContext(dir, {
    channel: "chromium", // the new headless mode, which supports extensions
    args: [`--disable-extensions-except=${EXT}`, `--load-extension=${EXT}`],
  });
  const worker = ctx.serviceWorkers()[0] ?? (await ctx.waitForEvent("serviceworker"));
  return { ctx, worker, id: new URL(worker.url()).host, dir };
}

/** Fill in the options page like a user would. */
async function configure(p: Profile, server: Server): Promise<void> {
  const token = await server.syncToken();
  const page = await p.ctx.newPage();
  await page.goto(`chrome-extension://${p.id}/options.html`);
  await page.fill("#url", server.url);
  await page.fill("#token", token);
  await page.click("#save");
  await page.waitForFunction(() => document.getElementById("status")?.textContent === "Saved.");
  await page.close();
}

/** Sync now, through the extension's own page (the service worker has no sender). */
async function sync(p: Profile): Promise<void> {
  const page = await p.ctx.newPage();
  await page.goto(`chrome-extension://${p.id}/options.html`);
  await page.evaluate(() => chrome.runtime.sendMessage({ type: "bookmoor-sync-now" }));
  await page.close();
}

interface Mark {
  title: string;
  url?: string;
}

/** Titles and URLs of everything below the bookmarks bar. */
async function bar(p: Profile): Promise<Mark[]> {
  return p.worker.evaluate(async () => {
    const kids = await chrome.bookmarks.getChildren("1");
    return kids.map((k) => ({ title: k.title, ...(k.url ? { url: k.url } : {}) }));
  });
}

describe("two real Chromium profiles", { skip: !BIN && !required }, () => {
  let server: Server;
  let a: Profile;
  let b: Profile;

  before(async () => {
    assert.ok(BIN, "bookmoor-server is not built");
    server = await startServer(BIN);
    [a, b] = await Promise.all([launch(), launch()]);
    await Promise.all([configure(a, server), configure(b, server)]);
  });

  after(async () => {
    for (const p of [a, b]) {
      await p?.ctx.close();
      if (p) rmSync(p.dir, { recursive: true, force: true });
    }
    server?.stop();
  });

  it("syncs a new bookmark from A to B", async () => {
    await a.worker.evaluate(() =>
      chrome.bookmarks.create({ parentId: "1", title: "Example", url: "https://example.com/" }),
    );
    await sync(a);
    await sync(b);
    assert.deepEqual(await bar(b), [{ title: "Example", url: "https://example.com/" }]);
  });

  it("syncs a rename from B to A", async () => {
    const [m] = await b.worker.evaluate(() => chrome.bookmarks.getChildren("1"));
    await b.worker.evaluate((id) => chrome.bookmarks.update(id, { title: "Renamed" }), m!.id);
    await sync(b);
    await sync(a);
    assert.deepEqual(await bar(a), [{ title: "Renamed", url: "https://example.com/" }]);
  });

  it("syncs a deletion", async () => {
    const [m] = await a.worker.evaluate(() => chrome.bookmarks.getChildren("1"));
    await a.worker.evaluate((id) => chrome.bookmarks.remove(id), m!.id);
    await sync(a);
    await sync(b);
    assert.deepEqual(await bar(b), []);
  });

  it("syncs from the toolbar popup and shows the result", async () => {
    await a.worker.evaluate(() => chrome.bookmarks.create({ parentId: "1", title: "Via popup", url: "https://popup.example/" }));
    const page = await a.ctx.newPage();
    await page.goto(`chrome-extension://${a.id}/popup.html`);
    await page.click("#syncnow");
    await page.waitForFunction(() => document.getElementById("sync")?.textContent?.startsWith("Last sync succeeded."));
    await page.close();
    await sync(b);
    assert.ok((await bar(b)).some((m) => m.title === "Via popup"));
    await b.worker.evaluate(async () => {
      const [m] = (await chrome.bookmarks.getChildren("1")).filter((k) => k.title === "Via popup");
      if (m) await chrome.bookmarks.remove(m.id);
    });
    await sync(b);
    await sync(a);
  });

  it("keeps edits made on both sides", async () => {
    await a.worker.evaluate(() => chrome.bookmarks.create({ parentId: "1", title: "From A", url: "https://a.example/" }));
    await b.worker.evaluate(() => chrome.bookmarks.create({ parentId: "1", title: "From B", url: "https://b.example/" }));
    await sync(a);
    await sync(b);
    await sync(a);
    const titles = async (p: Profile) => (await bar(p)).map((m) => m.title).sort();
    assert.deepEqual(await titles(a), ["From A", "From B"]);
    assert.deepEqual(await titles(b), ["From A", "From B"]);
  });
});

export type { Page };
