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

export type Kind = "chrome" | "firefox";

/**
 * chrome.bookmarks semantics. Chrome's `move` takes a pre-removal index when
 * a node moves down inside its folder; Firefox's takes the final one, and
 * Firefox has a fourth root folder (the menu) that is not synced.
 */
export class FakeBookmarks implements BookmarkApi {
  nodes = new Map<string, FNode>();
  private n = 100;
  kind: Kind;
  root: string;
  bar: string;
  other: string;
  mobile: string;
  /** Roots that are never synced. */
  local = new Set<string>();

  constructor(kind: Kind = "chrome") {
    this.kind = kind;
    const ids =
      kind === "chrome"
        ? ["0", "1", "2", "3"]
        : ["root________", "toolbar_____", "unfiled_____", "mobile______"];
    [this.root, this.bar, this.other, this.mobile] = ids as [string, string, string, string];
    const kids = [this.bar, this.other, this.mobile];
    if (kind === "firefox") {
      kids.unshift("menu________");
      this.local.add("menu________");
    }
    this.nodes.set(this.root, { id: this.root, title: "", kids });
    for (const k of kids) this.nodes.set(k, { id: k, parentId: this.root, title: k, kids: [] });
  }

  isRoot(id: string): boolean {
    return id === this.root || this.nodes.get(id)?.parentId === this.root;
  }

  /** Whether `id` sits in a folder that is not synced. */
  isLocal(id: string): boolean {
    for (let n = this.nodes.get(id); n; n = n.parentId ? this.nodes.get(n.parentId) : undefined) {
      if (this.local.has(n.id)) return true;
    }
    return false;
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
    return [this.view(this.root)];
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
    if (old === dest.kids && this.kind === "chrome") {
      dest.kids.splice(index, 0, id);
      dest.kids.splice(index <= oldIdx ? oldIdx + 1 : oldIdx, 1);
    } else if (old === dest.kids) {
      dest.kids.splice(oldIdx, 1);
      dest.kids.splice(index, 0, id);
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
  shape(id = this.root): unknown {
    const f = this.nodes.get(id)!;
    if (f.kids === undefined) return [f.title, f.url];
    return f.kids.filter((k) => !this.local.has(k)).map((k) => this.shape(k));
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
  api: FakeBookmarks;
  store = new MemoryStore();
  transport: Transport;
  constructor(transport: Transport, kind: Kind = "chrome") {
    this.transport = transport;
    this.api = new FakeBookmarks(kind);
  }
  identity = "";
  flush(): Promise<Outcome> {
    return flush(this.api, this.store, this.transport, this.identity);
  }
}

export interface Server {
  url: string;
  admin: string;
  stop(): void;
  browser(kind?: Kind): Promise<Browser>;
  settings(token: string): Settings;
  /** A fresh sync-scope token. */
  syncToken(): Promise<string>;
}

export function serverBinary(): string | undefined {
  const bin =
    process.env.BOOKMOOR_SERVER_BIN ??
    fileURLToPath(
      new URL(
        `../../target/debug/bookmoor-server${process.platform === "win32" ? ".exe" : ""}`,
        import.meta.url,
      ),
    );
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
  // Tokens belong to a user; the single test user owns the one tree.
  let user: Promise<unknown> | undefined;
  const syncToken = async (): Promise<string> => {
    user ??= fetch(url + "/v1/users", {
      method: "POST",
      headers: { Authorization: `Bearer ${admin}`, "Content-Type": "application/json" },
      body: JSON.stringify({ name: "test" }),
    });
    await user;
    const res = await fetch(url + "/v1/tokens", {
      method: "POST",
      headers: { Authorization: `Bearer ${admin}`, "Content-Type": "application/json" },
      body: JSON.stringify({ name: "test", scope: "sync" }),
    });
    return ((await res.json()) as { token: string }).token;
  };
  const settings = (token: string): Settings => ({ serverUrl: url, token, headers: [], query: [], sso: false });
  return {
    url,
    admin,
    stop: () => proc.kill(),
    settings,
    syncToken,
    async browser(kind: Kind = "chrome") {
      return new Browser(httpTransport(settings(await syncToken())), kind);
    },
  };
}
