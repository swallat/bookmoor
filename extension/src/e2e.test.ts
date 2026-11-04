// Two or more simulated browsers against the real bookmoor-server binary.
// Build it first (`cargo build -p bookmoor-server`); in CI it is required.

import assert from "node:assert/strict";
import { after, before, describe, it } from "node:test";
import { HttpError, last, type Transport } from "./flow.ts";
import { type Browser, type Kind, type Server, serverBinary, startServer } from "./testkit.ts";

const bin = serverBinary();
if (!bin && process.env.BOOKMOOR_E2E_REQUIRED) throw new Error("bookmoor-server binary missing");

for (const kind of ["chrome", "firefox"] as Kind[]) {
describe(`end to end (${kind})`, { skip: bin ? false : "bookmoor-server is not built" }, () => {
  let server: Server;
  before(async () => {
    server = await startServer(bin!);
  });
  after(() => server.stop());

  const bar = (b: Browser) => b.api.nodes.get(b.api.bar)!;
  const add = async (b: Browser, parent: string, title: string, url?: string, index?: number) =>
    (await b.api.create({ parentId: parent, title, ...(url ? { url } : {}), ...(index !== undefined ? { index } : {}) })).id;
  const sync = async (...bs: Browser[]) => {
    for (let i = 0; i < 2; i++) for (const b of bs) assert.equal(await b.flush(), "ok", String(last.error instanceof Error ? last.error.stack : last.error));
  };
  const same = (...bs: Browser[]) => {
    for (const b of bs.slice(1)) assert.deepEqual(b.api.shape(), bs[0]!.api.shape());
  };

  it("carries bookmarks, folders and order to a second browser", async () => {
    const a = await server.browser(kind);
    const b = await server.browser(kind);
    const f = await add(a, a.api.bar, "Dev");
    await add(a, f, "Rust", "https://rust-lang.org/");
    await add(a, f, "MDN", "https://developer.mozilla.org/");
    await add(a, a.api.other, "News", "https://news.example/");
    await sync(a, b);
    same(a, b);
    assert.equal(bar(b).kids!.length >= 1, true);
  });

  it("matches existing bookmarks on first sync instead of duplicating", async () => {
    const a = await server.browser(kind);
    const b = await server.browser(kind);
    for (const x of [a, b]) {
      const f = await add(x, x.api.other, "Shared folder");
      await add(x, f, "Same", "https://same.example/");
    }
    await add(b, b.api.other, "Only on b", "https://b.example/");
    await sync(a, b);
    same(a, b);
    for (const x of [a, b]) {
      const titles = [...x.api.nodes.values()].map((n) => n.title);
      assert.equal(titles.filter((t) => t === "Same").length, 1);
      assert.equal(titles.filter((t) => t === "Shared folder").length, 1);
      assert.equal(titles.filter((t) => t === "Only on b").length, 1);
    }
  });

  it("propagates renames, moves, reorders and deletes", async () => {
    const a = await server.browser(kind);
    const b = await server.browser(kind);
    const f = await add(a, a.api.bar, "Folder");
    const x = await add(a, f, "X", "https://x.example/");
    const y = await add(a, f, "Y", "https://y.example/");
    const z = await add(a, f, "Z", "https://z.example/");
    await sync(a, b);
    await a.api.update(x, { title: "X renamed" });
    await a.api.move(z, { parentId: f, index: 0 });
    await a.api.removeTree(y);
    await sync(a, b);
    same(a, b);
    const bf = [...b.api.nodes.values()].find((n) => n.title === "Folder")!;
    assert.deepEqual(
      bf.kids!.map((k) => b.api.nodes.get(k)!.title),
      ["Z", "X renamed"],
    );
  });

  it("moves a node down within its folder whatever the browser's index rules", async () => {
    const a = await server.browser(kind);
    const b = await server.browser(kind);
    const f = await add(a, a.api.bar, "Down");
    const ids: string[] = [];
    for (const t of ["p", "q", "r", "s"]) ids.push(await add(a, f, t, `https://${t}.example/`));
    await sync(a, b);
    await a.api.move(ids[0]!, { parentId: f, index: kind === "chrome" ? 4 : 3 });
    await a.api.move(ids[1]!, { parentId: f, index: kind === "chrome" ? 3 : 2 });
    await sync(a, b);
    same(a, b);
    const bf = [...b.api.nodes.values()].find((n) => n.title === "Down")!;
    assert.deepEqual(
      bf.kids!.map((k) => b.api.nodes.get(k)!.title),
      ["r", "s", "q", "p"],
    );
  });

  it("keeps an edit when the other side deleted the folder", async () => {
    const a = await server.browser(kind);
    const b = await server.browser(kind);
    const f = await add(a, a.api.bar, "Doomed");
    await add(a, f, "Old", "https://old.example/");
    await sync(a, b);
    const bf = [...b.api.nodes.values()].find((n) => n.title === "Doomed")!;
    await b.api.create({ parentId: bf.id, title: "New on b", url: "https://new.example/" });
    await a.api.removeTree(f);
    await sync(a, b);
    await sync(a, b);
    same(a, b);
    assert.match(JSON.stringify(a.api.shape()), /New on b/);
  });

  it("resends unacknowledged ops without duplicating", async () => {
    const a = await server.browser(kind);
    const b = await server.browser(kind);
    await add(a, a.api.bar, "Once", "https://once.example/");
    const real = a.transport;
    let failNext = false;
    const flaky: Transport = {
      endpoint: () => real.endpoint(),
      tree: () => real.tree(),
      sync: async (req) => {
        const resp = await real.sync(req);
        if (failNext && req.ops.length > 0) {
          failNext = false;
          throw new HttpError(502); // the server applied it, the client never heard
        }
        return resp;
      },
    };
    a.transport = flaky;
    await sync(a);
    await add(a, a.api.bar, "Twice", "https://twice.example/");
    failNext = true;
    assert.equal(await a.flush(), "retry");
    await sync(a, b);
    same(a, b);
    assert.equal(JSON.stringify(b.api.shape()).split("Twice").length - 1, 1);
  });

  it("recovers from conflicting moves that would form a cycle", async () => {
    const a = await server.browser(kind);
    const b = await server.browser(kind);
    const x = await add(a, a.api.bar, "CycX");
    const y = await add(a, a.api.bar, "CycY");
    await sync(a, b);
    const bx = [...b.api.nodes.values()].find((n) => n.title === "CycX")!.id;
    const by = [...b.api.nodes.values()].find((n) => n.title === "CycY")!.id;
    await a.api.move(x, { parentId: y });
    await b.api.move(by, { parentId: bx });
    await sync(a, b);
    await sync(a, b);
    same(a, b);
  });

  it("starts afresh when the token changes", async () => {
    const a = await server.browser(kind);
    await add(a, a.api.bar, "Before", "https://before.example/");
    a.identity = "one";
    await sync(a);
    const other = await server.browser(kind);
    a.transport = other.transport;
    a.identity = "two";
    await sync(a);
    const titles = [...a.api.nodes.values()].map((n) => n.title);
    assert.equal(titles.filter((t) => t === "Before").length, 1);
  });

  it("reports a bad token as auth", async () => {
    const bad = new (await import("./testkit.ts")).Browser(
      (await import("./transport.ts")).httpTransport(server.settings("bkm_x_" + "b".repeat(64))),
    );
    assert.equal(await bad.flush(), "auth");
  });

  it("three browsers converge under random edits", async () => {
    for (let seed = 1; seed <= Number(process.env.BOOKMOOR_SEEDS ?? 6); seed++) {
      let s = seed * 7919;
      const rnd = (n: number) => {
        s = (s * 1103515245 + 12345) & 0x7fffffff;
        return s % n;
      };
      const bs = [await server.browser(kind), await server.browser(kind), await server.browser(kind)];
      await sync(...bs);
      for (let round = 0; round < 12; round++) {
        for (const b of bs) {
          if (rnd(3) === 0) continue;
          for (let k = 0; k < 1 + rnd(3); k++) await randomEdit(b, rnd);
        }
        const order = [...bs].sort(() => rnd(3) - 1);
        for (const b of order) {
          if (rnd(4) !== 0) assert.equal(await b.flush(), "ok", String(last.error instanceof Error ? last.error.stack : last.error));
        }
      }
      await sync(...bs);
      await sync(...bs);
      same(...bs);
    }
  });
});
}

async function randomEdit(b: Browser, rnd: (n: number) => number): Promise<void> {
  const all = [...b.api.nodes.values()];
  const synced = all.filter((n) => !b.api.isLocal(n.id));
  const folders = synced.filter((n) => n.kids);
  const movable = synced.filter((n) => n.parentId && !b.api.isRoot(n.id));
  const pick = <T>(xs: T[]) => xs[rnd(xs.length)]!;
  switch (rnd(6)) {
    case 0:
    case 1: {
      const p = pick(folders.filter((f) => f.id !== b.api.root));
      const idx = rnd(p.kids!.length + 1);
      await b.api.create({
        parentId: p.id,
        index: idx,
        title: `b${rnd(1000)}`,
        url: `https://e${rnd(1000)}.example/`,
      });
      break;
    }
    case 2: {
      const p = pick(folders.filter((f) => f.id !== b.api.root));
      await b.api.create({ parentId: p.id, title: `f${rnd(1000)}` });
      break;
    }
    case 3:
      if (movable.length) await b.api.update(pick(movable).id, { title: `r${rnd(1000)}` });
      break;
    case 4:
      if (movable.length) {
        const n = pick(movable);
        const p = pick(folders.filter((f) => f.id !== b.api.root));
        try {
          await b.api.move(n.id, { parentId: p.id, index: rnd(p.kids!.length + 1) });
        } catch {
          /* cycle: the browser refuses */
        }
      }
      break;
    default:
      if (movable.length) await b.api.removeTree(pick(movable).id);
  }
}

describe("end to end (chrome and firefox together)", { skip: bin ? false : "bookmoor-server is not built" }, () => {
  let server: Server;
  before(async () => {
    server = await startServer(bin!);
  });
  after(() => server.stop());

  const titles = (b: Browser, folder: string) =>
    b.api.nodes.get(folder)!.kids!.map((k) => b.api.nodes.get(k)!.title);
  const sync = async (...bs: Browser[]) => {
    for (let i = 0; i < 2; i++) for (const b of bs) assert.equal(await b.flush(), "ok", String(last.error instanceof Error ? last.error.stack : last.error));
  };

  it("pairs the Firefox toolbar with Chrome's bookmarks bar, in both directions", async () => {
    const ff = await server.browser("firefox");
    const ch = await server.browser("chrome");
    await ff.api.create({ parentId: ff.api.bar, title: "From Firefox", url: "https://ff.example/" });
    await ff.api.create({ parentId: ff.api.other, title: "FF other", url: "https://ffo.example/" });
    await ff.api.create({ parentId: "menu________", title: "FF menu", url: "https://ffm.example/" });
    await ch.api.create({ parentId: ch.api.bar, title: "From Chrome", url: "https://ch.example/" });
    await ch.api.create({ parentId: ch.api.other, title: "CH other", url: "https://cho.example/" });
    await sync(ff, ch);
    assert.deepEqual(titles(ch, ch.api.bar).sort(), ["From Chrome", "From Firefox"]);
    assert.deepEqual(titles(ff, ff.api.bar).sort(), ["From Chrome", "From Firefox"]);
    assert.deepEqual(titles(ch, ch.api.other).sort(), ["CH other", "FF other"]);
    assert.deepEqual(titles(ff, ff.api.other).sort(), ["CH other", "FF other"]);
    // The Firefox menu has no counterpart: it stays out of Chrome entirely.
    const all = [...ch.api.nodes.values()].map((n) => n.title);
    assert.equal(all.includes("FF menu"), false);
  });
});
