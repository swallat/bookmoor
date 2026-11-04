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
  /** The server's own explanation (`error` in its JSON body), if any. */
  detail: string | undefined;
  constructor(status: number, detail?: string) {
    super(detail ? `HTTP ${status}: ${detail}` : `HTTP ${status}`);
    this.status = status;
    this.detail = detail;
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
 * Pair the browser with the server's tree without deleting anything the
 * server did not delete: nodes with the same kind, title and URL under paired
 * folders are the same node; server nodes without a partner are created
 * here, browser nodes without a partner are offered to the server by the next
 * diff (5.4). On a resync (`rebase`) existing pairings whose server node
 * still exists are kept, so renames and moves made elsewhere are not mistaken
 * for new nodes, and nodes the server dropped are removed here as a remote
 * delete would.
 */
export async function matchTree(
  api: BookmarkApi,
  state: SyncState,
  tree: TreeResponse,
  rebase = false,
): Promise<void> {
  const server = new Map(tree.nodes.map((n) => [n.id, n]));
  if (rebase) {
    const roots = new Set<string>([ROOT, ...ROLES]);
    const gone = Object.values(state.map).filter((sid) => !server.has(sid) && !roots.has(sid));
    // Top-most only; removing a folder takes its (synced) content along.
    const top = gone.filter((sid) => {
      const p = state.base[sid]?.parent;
      return !(p && gone.includes(p));
    });
    await applyRemote(
      api,
      state,
      top.map((id) => ({ op_id: "rebase", type: "remove" as const, id })),
    );
  }
  const local = await api.getTree();
  const byChrome = new Map<string, LocalNode>();
  const index = (n: LocalNode) => {
    byChrome.set(n.id, n);
    n.children?.forEach(index);
  };
  local.forEach(index);

  const kept: Record<string, string> = {};
  if (rebase) {
    for (const [chrome, sid] of Object.entries(state.map)) {
      if (server.has(sid) && byChrome.has(chrome)) kept[chrome] = sid;
    }
  }
  state.rev = tree.rev;
  state.pending = [];
  state.map = kept;
  state.base = {};
  mapRoots(state, local);
  for (const sid of Object.values(state.map)) state.base[sid] = nodeFromWire(server.get(sid)!);
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
  const pairedChrome = new Map(Object.entries(state.map).map(([c, s]) => [s, c]));
  const walk = (sid: string, chrome: LocalNode) => {
    const used = new Set<string>();
    const locals = (chrome.children ?? []).filter((c) => !c.unmodifiable && c.type !== "separator" && !(c.id in state.map));
    for (const kid of [...(kidsOf.get(sid) ?? [])].sort(bySibling)) {
      const s = server.get(kid)!;
      const already = pairedChrome.get(kid);
      if (already) {
        const n = byChrome.get(already);
        if (n && s.kind === "folder") walk(kid, n);
        continue;
      }
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

/** Stop offering a node the server will not take. */
function ignoreNode(state: SyncState, sid: string): void {
  const chrome = reverseMap(state.map).get(sid);
  if (chrome) state.ignored[chrome] = true;
  const rev = reverseMap(state.map);
  for (const id of dropSubtree(state.base, sid)) {
    const c = rev.get(id);
    if (c) delete state.map[c];
  }
}

/**
 * One sync round. `identity` names the server and token in use; when it
 * differs from the saved state's the device starts afresh (a new token is
 * bound to a new endpoint).
 */
export async function flush(
  api: BookmarkApi,
  store: StateStore,
  transport: Transport,
  identity = "",
): Promise<Outcome> {
  try {
    let state = await store.load();
    if (state && state.identity !== identity) state = null;
    if (!state) {
      state = emptyState(await transport.endpoint(), identity);
      await matchTree(api, state, await transport.tree());
      await store.save(state);
    }
    const save = () => store.save(state!);
    const tree = await api.getTree();
    mapRoots(state, tree);
    state.pending.push(...computeOps(state, tree));
    await store.save(state);

    let size = BATCH;
    let first = true;
    while (first || state.pending.length > 0) {
      first = false;
      const batch = state.pending.slice(0, size);
      let resp: SyncResponse;
      try {
        resp = await transport.sync({
          endpoint_id: state.endpointId,
          since_rev: state.rev,
          ops: batch,
        });
      } catch (e) {
        // The server refused the request itself (too big, malformed).
        const refused = e instanceof HttpError && (e.status === 400 || e.status === 413 || e.status === 422);
        if (!refused) throw e;
        if (batch.length > 1) {
          size = Math.ceil(batch.length / 2);
          continue;
        }
        // A single op that can never be accepted must not block the queue.
        const op = batch[0]!;
        state.pending.shift();
        if (op.type === "add") ignoreNode(state, op.id);
        else await adopt(api, state, transport, new Set([op.id]));
        await save();
        continue;
      }
      if (resp.resync) {
        await matchTree(api, state, await transport.tree(), true);
        await store.save(state);
        return "ok";
      }
      state.pending = state.pending.slice(batch.length);
      state.rev = resp.rev;
      const redo = await applyRemote(api, state, resp.ops, save);
      for (const op of batch) {
        const r = resp.rejected.find((x) => x.op_id === op.op_id);
        if (!r) continue;
        if (r.reason === "invalid" && op.type === "add") ignoreNode(state, op.id);
        else redo.add(op.id);
      }
      await adopt(api, state, transport, redo);
      await store.save(state);
    }
    return "ok";
  } catch (e) {
    last.error = e;
    if (e instanceof HttpError && (e.status === 401 || e.status === 403)) return "auth";
    return "retry";
  }
}
