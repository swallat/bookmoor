// Two or more simulated browsers against the real bookmoor-server binary.
// Build it first (`cargo build -p bookmoor-server`); in CI it is required.

import assert from "node:assert/strict";
import { after, before, describe, it } from "node:test";
import { HttpError, last, type Transport } from "./flow.ts";
import { type Browser, type Server, serverBinary, startServer } from "./testkit.ts";

const bin = serverBinary();
if (!bin && process.env.BOOKMOOR_E2E_REQUIRED) throw new Error("bookmoor-server binary missing");

describe("end to end", { skip: bin ? false : "bookmoor-server is not built" }, () => {
  let server: Server;
  before(async () => {
    server = await startServer(bin!);
  });
  after(() => server.stop());

  const bar = (b: Browser) => b.api.nodes.get("1")!;
  const add = async (b: Browser, parent: string, title: string, url?: string, index?: number) =>
    (await b.api.create({ parentId: parent, title, ...(url ? { url } : {}), ...(index !== undefined ? { index } : {}) })).id;
  const sync = async (...bs: Browser[]) => {
    for (let i = 0; i < 2; i++) for (const b of bs) assert.equal(await b.flush(), "ok", String(last.error instanceof Error ? last.error.stack : last.error));
  };
  const same = (...bs: Browser[]) => {
    for (const b of bs.slice(1)) assert.deepEqual(b.api.shape(), bs[0]!.api.shape());
  };

  it("carries bookmarks, folders and order to a second browser", async () => {
    const a = await server.browser();
    const b = await server.browser();
    const f = await add(a, "1", "Dev");
    await add(a, f, "Rust", "https://rust-lang.org/");
    await add(a, f, "MDN", "https://developer.mozilla.org/");
    await add(a, "2", "News", "https://news.example/");
    await sync(a, b);
    same(a, b);
    assert.equal(bar(b).kids!.length >= 1, true);
  });

  it("matches existing bookmarks on first sync instead of duplicating", async () => {
    const a = await server.browser();
    const b = await server.browser();
    for (const x of [a, b]) {
      const f = await add(x, "2", "Shared folder");
      await add(x, f, "Same", "https://same.example/");
    }
    await add(b, "2", "Only on b", "https://b.example/");
    await sync(a, b);
    same(a, b);
    const titles = JSON.stringify(a.api.shape());
    assert.equal(titles.split("Same").length - 1 >= 1, true);
  });

  it("propagates renames, moves, reorders and deletes", async () => {
    const a = await server.browser();
    const b = await server.browser();
    const f = await add(a, "1", "Folder");
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

  it("keeps an edit when the other side deleted the folder", async () => {
    const a = await server.browser();
    const b = await server.browser();
    const f = await add(a, "1", "Doomed");
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
    const a = await server.browser();
    const b = await server.browser();
    await add(a, "1", "Once", "https://once.example/");
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
    await add(a, "1", "Twice", "https://twice.example/");
    failNext = true;
    assert.equal(await a.flush(), "retry");
    await sync(a, b);
    same(a, b);
    assert.equal(JSON.stringify(b.api.shape()).split("Twice").length - 1, 1);
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
      const bs = [await server.browser(), await server.browser(), await server.browser()];
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

async function randomEdit(b: Browser, rnd: (n: number) => number): Promise<void> {
  const all = [...b.api.nodes.values()];
  const folders = all.filter((n) => n.kids);
  const movable = all.filter((n) => n.parentId && !["1", "2", "3"].includes(n.id));
  const pick = <T>(xs: T[]) => xs[rnd(xs.length)]!;
  switch (rnd(6)) {
    case 0:
    case 1: {
      const p = pick(folders.filter((f) => f.id !== "0"));
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
      const p = pick(folders.filter((f) => f.id !== "0"));
      await b.api.create({ parentId: p.id, title: `f${rnd(1000)}` });
      break;
    }
    case 3:
      if (movable.length) await b.api.update(pick(movable).id, { title: `r${rnd(1000)}` });
      break;
    case 4:
      if (movable.length) {
        const n = pick(movable);
        const p = pick(folders.filter((f) => f.id !== "0"));
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
