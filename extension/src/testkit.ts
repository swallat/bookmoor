// Test helpers: an in-memory chrome.bookmarks and a real bookmoor-server
// process, so tests exercise the extension logic against the actual engine.

import { spawn, type ChildProcess } from "node:child_process";
import { existsSync } from "node:fs";
import { createServer } from "node:net";
import { fileURLToPath } from "node:url";
import { flush, type Outcome, type StateStore, type Transport } from "./flow.ts";
import type { BookmarkApi, LocalNode, SyncState } from "./local.ts";
import type { Settings } from "./settings.ts";
import { httpTransport } from "./transport.ts";

interface FNode {
  id: string;
  parentId?: string;
  title: string;
  url?: string;
  kids?: string[];
}

/** chrome.bookmarks semantics, including `move` taking a pre-removal index. */
export class FakeBookmarks implements BookmarkApi {
  nodes = new Map<string, FNode>();
  private n = 100;

  constructor() {
    this.nodes.set("0", { id: "0", title: "", kids: ["1", "2", "3"] });
    this.nodes.set("1", { id: "1", parentId: "0", title: "Bookmarks bar", kids: [] });
    this.nodes.set("2", { id: "2", parentId: "0", title: "Other bookmarks", kids: [] });
    this.nodes.set("3", { id: "3", parentId: "0", title: "Mobile bookmarks", kids: [] });
  }

  private view(id: string): LocalNode {
    const f = this.nodes.get(id);
    if (!f) throw new Error(`no node ${id}`);
    const parent = f.parentId ? this.nodes.get(f.parentId) : undefined;
    const out: LocalNode = { id, title: f.title };
    if (f.parentId) out.parentId = f.parentId;
    if (parent) out.index = parent.kids!.indexOf(id);
    if (f.url !== undefined) out.url = f.url;
    if (f.kids) out.children = f.kids.map((k) => this.view(k));
    return out;
  }

  async getTree() {
    return [this.view("0")];
  }
  async getSubTree(id: string) {
    return [this.view(id)];
  }
  async create(a: { parentId: string; index?: number; title: string; url?: string }) {
    const p = this.nodes.get(a.parentId);
    if (!p?.kids) throw new Error("bad parent");
    const id = String(++this.n);
    const f: FNode = { id, parentId: a.parentId, title: a.title };
    if (a.url !== undefined) f.url = a.url;
    else f.kids = [];
    this.nodes.set(id, f);
    p.kids.splice(a.index ?? p.kids.length, 0, id);
    return this.view(id);
  }
  async update(id: string, c: { title?: string; url?: string }) {
    const f = this.nodes.get(id)!;
    if (c.title !== undefined) f.title = c.title;
    if (c.url !== undefined && f.url !== undefined) f.url = c.url;
  }
  async move(id: string, d: { parentId: string; index?: number }) {
    const f = this.nodes.get(id)!;
    const dest = this.nodes.get(d.parentId);
    if (!dest?.kids) throw new Error("bad parent");
    for (let p: FNode | undefined = dest; p; p = p.parentId ? this.nodes.get(p.parentId) : undefined) {
      if (p.id === id) throw new Error("cycle");
    }
    const old = this.nodes.get(f.parentId!)!.kids!;
    const oldIdx = old.indexOf(id);
    const index = d.index ?? dest.kids.length;
    if (old === dest.kids) {
      dest.kids.splice(index, 0, id);
      dest.kids.splice(index <= oldIdx ? oldIdx + 1 : oldIdx, 1);
    } else {
      old.splice(oldIdx, 1);
      dest.kids.splice(index, 0, id);
    }
    f.parentId = d.parentId;
  }
  async remove(id: string) {
    const f = this.nodes.get(id)!;
    if (f.kids?.length) throw new Error("not empty");
    await this.removeTree(id);
  }
  async removeTree(id: string) {
    const f = this.nodes.get(id)!;
    for (const k of [...(f.kids ?? [])]) await this.removeTree(k);
    const p = this.nodes.get(f.parentId!)!;
    p.kids!.splice(p.kids!.indexOf(id), 1);
    this.nodes.delete(id);
  }

  /** Titles and URLs of everything below the three roots, nested. */
  shape(id = "0"): unknown {
    const f = this.nodes.get(id)!;
    if (f.kids === undefined) return [f.title, f.url];
    return f.kids.map((k) => this.shape(k));
  }
}

export class MemoryStore implements StateStore {
  state: SyncState | null = null;
  async load() {
    return this.state ? structuredClone(this.state) : null;
  }
  async save(s: SyncState) {
    this.state = structuredClone(s);
  }
}

export class Browser {
  api = new FakeBookmarks();
  store = new MemoryStore();
  transport: Transport;
  constructor(transport: Transport) {
    this.transport = transport;
  }
  flush(): Promise<Outcome> {
    return flush(this.api, this.store, this.transport);
  }
}

export interface Server {
  url: string;
  admin: string;
  stop(): void;
  browser(): Promise<Browser>;
  settings(token: string): Settings;
}

export function serverBinary(): string | undefined {
  const bin =
    process.env.BOOKMOOR_SERVER_BIN ??
    fileURLToPath(new URL("../../target/debug/bookmoor-server", import.meta.url));
  return existsSync(bin) ? bin : undefined;
}

async function freePort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const s = createServer();
    s.listen(0, "127.0.0.1", () => {
      const port = (s.address() as { port: number }).port;
      s.close(() => resolve(port));
    });
    s.on("error", reject);
  });
}

export async function startServer(bin: string): Promise<Server> {
  const port = await freePort();
  const admin = `bkm_adm_${"a".repeat(64)}`;
  const proc: ChildProcess = spawn(bin, ["--bind", `127.0.0.1:${port}`], {
    env: { ...process.env, BOOKMOOR_ADMIN_TOKEN: admin },
    stdio: "ignore",
  });
  const url = `http://127.0.0.1:${port}`;
  for (let i = 0; ; i++) {
    try {
      if ((await fetch(url + "/health")).ok) break;
    } catch {
      /* not up yet */
    }
    if (i > 100) throw new Error("server did not start");
    await new Promise((r) => setTimeout(r, 50));
  }
  const settings = (token: string): Settings => ({ serverUrl: url, token, headers: [] });
  return {
    url,
    admin,
    stop: () => proc.kill(),
    settings,
    async browser() {
      const res = await fetch(url + "/v1/tokens", {
        method: "POST",
        headers: { Authorization: `Bearer ${admin}`, "Content-Type": "application/json" },
        body: JSON.stringify({ name: "test", scope: "sync" }),
      });
      const { token } = (await res.json()) as { token: string };
      return new Browser(httpTransport(settings(token)));
    },
  };
}
