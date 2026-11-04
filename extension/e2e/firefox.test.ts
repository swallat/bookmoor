// End-to-end: the real Firefox package (dist-e2e-firefox/) in two real Firefox
// profiles via geckodriver, against the real server binary. Needs Firefox
// (selenium-manager fetches geckodriver) and BOOKMOOR_E2E_FIREFOX=1, which CI
// sets; the browsers are not installed on every developer machine.

import assert from "node:assert/strict";
import { resolve } from "node:path";
import { after, before, describe, it } from "node:test";
import { Builder, type WebDriver } from "selenium-webdriver";
import firefox from "selenium-webdriver/firefox.js";
import { serverBinary, startServer, type Server } from "../src/testkit.ts";

const EXT = resolve(import.meta.dirname, "../dist-e2e-firefox");
const ADDON_ID = "bookmoor@wallat.de";
// A fixed internal UUID makes the options page URL predictable.
const UUID = "6a1c1c25-3a0a-4d3c-9d7e-2d8f5e0b7a11";
const OPTIONS = `moz-extension://${UUID}/options.html`;
const BAR = "toolbar_____";

const enabled = process.env.BOOKMOOR_E2E_FIREFOX === "1";
const BIN = serverBinary();

async function launch(): Promise<WebDriver> {
  const options = new firefox.Options();
  options.addArguments("-headless");
  options.setPreference("extensions.webextensions.uuids", JSON.stringify({ [ADDON_ID]: UUID }));
  const driver = await new Builder().forBrowser("firefox").setFirefoxOptions(options).build();
  await (driver as WebDriver & { installAddon(p: string, temporary: boolean): Promise<string> }).installAddon(EXT, true);
  return driver;
}

/** Call a WebExtension API (e.g. "bookmarks.getChildren") on the options page, through e2e/bridge.js. */
async function api<T>(d: WebDriver, path: string, ...args: unknown[]): Promise<T> {
  await d.get(OPTIONS);
  await d.executeScript(
    "document.body.dataset.cmd = arguments[0]; document.dispatchEvent(new Event('e2e-cmd'));",
    JSON.stringify({ path, args }),
  );
  const raw = await d.wait(async () => (await d.findElement({ css: "body" }).getAttribute("data-result")) || false, 15_000);
  const out = JSON.parse(raw as string) as { value?: T; error?: string };
  if (out.error) throw new Error(`${path}: ${out.error}`);
  return out.value as T;
}

async function configure(d: WebDriver, server: Server): Promise<void> {
  const token = await server.syncToken();
  await d.get(OPTIONS);
  await d.findElement({ id: "url" }).sendKeys(server.url);
  await d.findElement({ id: "token" }).sendKeys(token);
  await d.findElement({ id: "save" }).click();
  await d.wait(async () => (await d.findElement({ id: "status" }).getText()) === "Saved.", 10_000);
}

async function sync(d: WebDriver): Promise<void> {
  await api(d, "runtime.sendMessage", { type: "bookmoor-sync-now" });
}

async function bar(d: WebDriver): Promise<{ title: string; url?: string }[]> {
  const kids = await api<{ title: string; url?: string }[]>(d, "bookmarks.getChildren", BAR);
  return kids.map((k) => ({ title: k.title, ...(k.url ? { url: k.url } : {}) }));
}

function create(d: WebDriver, title: string, url: string): Promise<unknown> {
  return api(d, "bookmarks.create", { parentId: BAR, title, url });
}

describe("two real Firefox profiles", { skip: !enabled }, () => {
  let server: Server;
  let a: WebDriver;
  let b: WebDriver;

  before(async () => {
    assert.ok(BIN, "bookmoor-server is not built");
    server = await startServer(BIN);
    a = await launch();
    b = await launch();
    await configure(a, server);
    await configure(b, server);
  });

  after(async () => {
    await a?.quit();
    await b?.quit();
    server?.stop();
  });

  it("syncs a new bookmark from A to B", async () => {
    await create(a, "Example", "https://example.com/");
    await sync(a);
    await sync(b);
    assert.deepEqual(await bar(b), [{ title: "Example", url: "https://example.com/" }]);
  });

  it("syncs a rename from B to A", async () => {
    const [m] = await api<{ id: string }[]>(b, "bookmarks.getChildren", BAR);
    await api(b, "bookmarks.update", m!.id, { title: "Renamed" });
    await sync(b);
    await sync(a);
    assert.deepEqual(await bar(a), [{ title: "Renamed", url: "https://example.com/" }]);
  });

  it("syncs a deletion", async () => {
    const [m] = await api<{ id: string }[]>(a, "bookmarks.getChildren", BAR);
    await api(a, "bookmarks.remove", m!.id);
    await sync(a);
    await sync(b);
    assert.deepEqual(await bar(b), []);
  });
});
