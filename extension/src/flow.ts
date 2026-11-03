// One sync round: diff the browser against the base, exchange ops with the
// server, apply the answer. All persistent state lives in `StateStore`, so a
// restarted service worker continues where the last one stopped.

import { applyRemote } from "./apply.ts";
import { computeOps, dropSubtree } from "./diff.ts";
import {
  type BookmarkApi,
  type LocalNode,
  type SyncState,
  ROLES,
  ROOT,
  emptyState,
  kindOf,
  mapRoots,
  nodeFromWire,
  reverseMap,
} from "./local.ts";
import type { Op, SyncRequest, SyncResponse, TreeResponse } from "./wire.ts";

export class HttpError extends Error {
  status: number;
  constructor(status: number) {
    super(`HTTP ${status}`);
    this.status = status;
  }
}

export interface Transport {
  /** Register this device and return its endpoint id. */
  endpoint(): Promise<string>;
  sync(req: SyncRequest): Promise<SyncResponse>;
  tree(): Promise<TreeResponse>;
}

export interface StateStore {
  load(): Promise<SyncState | null>;
  save(s: SyncState): Promise<void>;
}

/** `auth`: stop until the token is fixed; `retry`: try again later. */
export type Outcome = "ok" | "auth" | "retry";

const BATCH = 500;

/** The error behind the last `retry`/`auth` outcome, for logs and tests. */
export const last: { error: unknown } = { error: undefined };

function fingerprint(kind: string, title: string, url: string | null | undefined): string {
  return `${kind}\u0000${title.trim()}\u0000${(url ?? "").trim()}`;
}

/**
 * Pair the browser with the server's tree without deleting anything: nodes
 * with the same kind, title and URL under paired folders are the same node;
 * server nodes without a partner are created here, browser nodes without a
 * partner are offered to the server by the next diff (5.4).
 */
export async function matchTree(
  api: BookmarkApi,
  state: SyncState,
  tree: TreeResponse,
): Promise<void> {
  const server = new Map(tree.nodes.map((n) => [n.id, n]));
  state.rev = tree.rev;
  state.pending = [];
  state.map = {};
  state.base = {};
  const local = await api.getTree();
  mapRoots(state, local);
  for (const id of [ROOT, ...ROLES]) {
    const n = server.get(id);
    if (n) state.base[id] = nodeFromWire(n);
  }
  const kidsOf = new Map<string, string[]>();
  for (const n of tree.nodes) {
    if (n.parent) kidsOf.set(n.parent, [...(kidsOf.get(n.parent) ?? []), n.id]);
  }
  const bySibling = (a: string, b: string) => {
    const x = server.get(a)!;
    const y = server.get(b)!;
    return x.position < y.position ? -1 : x.position > y.position ? 1 : a < b ? -1 : 1;
  };
  const walk = (sid: string, chrome: LocalNode) => {
    const used = new Set<string>();
    const locals = (chrome.children ?? []).filter((c) => !c.unmodifiable && !(c.id in state.map));
    for (const kid of [...(kidsOf.get(sid) ?? [])].sort(bySibling)) {
      const s = server.get(kid)!;
      const fp = fingerprint(s.kind, s.title, s.url);
      const hit = locals.find((l) => !used.has(l.id) && fingerprint(kindOf(l), l.title, l.url) === fp);
      if (!hit) continue;
      used.add(hit.id);
      state.map[hit.id] = kid;
      state.base[kid] = nodeFromWire(s);
      if (s.kind === "folder") walk(kid, hit);
    }
  };
  const root = local[0];
  if (root) {
    for (const c of root.children ?? []) {
      const sid = state.map[c.id];
      if (sid && sid !== ROOT) walk(sid, c);
    }
  }
  const creates: Op[] = tree.nodes
    .filter((n) => n.parent && !(n.id in state.base))
    .map((n) => ({
      op_id: "match",
      type: "add" as const,
      id: n.id,
      parent: n.parent!,
      position: n.position,
      kind: n.kind,
      title: n.title,
      url: n.url,
    }));
  await applyRemote(api, state, creates);
}

/** After rejections, take the server's version of the affected nodes. */
async function adopt(
  api: BookmarkApi,
  state: SyncState,
  transport: Transport,
  ids: Set<string>,
): Promise<void> {
  if (ids.size === 0) return;
  const tree = await transport.tree();
  const known = new Map(tree.nodes.map((n) => [n.id, n]));
  const ops: Op[] = [];
  for (const n of tree.nodes) {
    if (ids.has(n.id) && n.parent) {
      ops.push({
        op_id: "adopt",
        type: "add",
        id: n.id,
        parent: n.parent,
        position: n.position,
        kind: n.kind,
        title: n.title,
        url: n.url,
      });
    }
  }
  for (const id of ids) if (!known.has(id)) ops.push({ op_id: "adopt", type: "remove", id });
  await applyRemote(api, state, ops);
}

export async function flush(
  api: BookmarkApi,
  store: StateStore,
  transport: Transport,
): Promise<Outcome> {
  try {
    let state = await store.load();
    if (!state) {
      state = emptyState(await transport.endpoint());
      await matchTree(api, state, await transport.tree());
      await store.save(state);
    }
    const tree = await api.getTree();
    mapRoots(state, tree);
    state.pending.push(...computeOps(state, tree));
    await store.save(state);

    let first = true;
    while (first || state.pending.length > 0) {
      first = false;
      const batch = state.pending.slice(0, BATCH);
      const resp = await transport.sync({
        endpoint_id: state.endpointId,
        since_rev: state.rev,
        ops: batch,
      });
      if (resp.resync) {
        await matchTree(api, state, await transport.tree());
        await store.save(state);
        return "ok";
      }
      state.pending = state.pending.slice(batch.length);
      state.rev = resp.rev;
      await applyRemote(api, state, resp.ops);
      const lost = new Set<string>();
      for (const op of batch) {
        const r = resp.rejected.find((x) => x.op_id === op.op_id);
        if (!r) continue;
        if (r.reason === "invalid") {
          const chrome = reverseMap(state.map).get(op.id);
          if (chrome) state.ignored[chrome] = true;
          for (const id of dropSubtree(state.base, op.id)) {
            const c = reverseMap(state.map).get(id);
            if (c) delete state.map[c];
          }
        } else {
          lost.add(op.id);
        }
      }
      await adopt(api, state, transport, lost);
      await store.save(state);
    }
    return "ok";
  } catch (e) {
    last.error = e;
    if (e instanceof HttpError && (e.status === 401 || e.status === 403)) return "auth";
    return "retry";
  }
}
