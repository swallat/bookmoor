// Flush-time state diff: compare the browser's current tree with the base
// and produce ops. Events are only a trigger, so a missed event cannot lose
// a change (docs/sync-architecture.md, 5.3).

import { between } from "./position.ts";
import {
  type BaseNode,
  type LocalNode,
  type SyncState,
  ROLES,
  ROOT,
  kindOf,
  newServerId,
  nextOpId,
} from "./local.ts";
import type { Op, Op1 as OpBody } from "./wire.ts";

interface Seen {
  parent: string;
  node: LocalNode;
}

/** Longest strictly increasing subsequence; returns the kept indices. */
export function lis(keys: string[]): Set<number> {
  const tails: number[] = []; // index into keys, per length
  const prev: number[] = new Array(keys.length).fill(-1);
  keys.forEach((k, i) => {
    let lo = 0;
    let hi = tails.length;
    while (lo < hi) {
      const mid = (lo + hi) >> 1;
      if (keys[tails[mid]!]! < k) lo = mid + 1;
      else hi = mid;
    }
    if (lo > 0) prev[i] = tails[lo - 1]!;
    tails[lo] = i;
  });
  const out = new Set<number>();
  for (let i = tails.length > 0 ? tails[tails.length - 1]! : -1; i >= 0; i = prev[i]!) out.add(i);
  return out;
}

/** Apply one of our own ops to the base, as the server will. */
export function applyToBase(base: Record<string, BaseNode>, b: OpBody): void {
  switch (b.type) {
    case "add":
      base[b.id] = {
        parent: b.parent,
        position: b.position,
        kind: b.kind,
        title: b.title,
        url: b.url ?? null,
      };
      break;
    case "update": {
      const n = base[b.id];
      if (!n) return;
      if (b.title != null) n.title = b.title;
      if (b.url != null) n.url = b.url;
      break;
    }
    case "move": {
      const n = base[b.id];
      if (!n) return;
      n.parent = b.parent;
      n.position = b.position;
      break;
    }
    case "remove":
      dropSubtree(base, b.id);
      break;
  }
}

export function dropSubtree(base: Record<string, BaseNode>, id: string): string[] {
  const gone: string[] = [];
  const stack = [id];
  while (stack.length) {
    const cur = stack.pop()!;
    if (!(cur in base)) continue;
    delete base[cur];
    gone.push(cur);
    for (const [k, v] of Object.entries(base)) if (v.parent === cur) stack.push(k);
  }
  return gone;
}

/**
 * Ops that bring the base in line with `tree`. New nodes get server ids and
 * enter `state.map`; the base and the op counter advance as if the server had
 * accepted the ops (rejections are repaired by the caller). Returns the ops.
 */
export function computeOps(state: SyncState, tree: LocalNode[]): Op[] {
  const root = tree[0];
  if (!root) return [];
  const seen = new Map<string, Seen>(); // sid -> info
  const order: string[] = []; // preorder of sids, excluding roots
  const kids = new Map<string, string[]>(); // parent sid -> children sids in order

  const walk = (n: LocalNode, parentSid: string) => {
    for (const c of n.children ?? []) {
      if (c.unmodifiable || c.type === "separator" || state.ignored[c.id]) continue;
      let sid = state.map[c.id];
      if (!sid) {
        sid = newServerId();
        state.map[c.id] = sid;
      }
      seen.set(sid, { parent: parentSid, node: c });
      order.push(sid);
      const list = kids.get(parentSid) ?? [];
      list.push(sid);
      kids.set(parentSid, list);
      if (c.children) walk(c, sid);
    }
  };
  // The role folders are fixed; their sids come from the map.
  for (const c of root.children ?? []) {
    const sid = state.map[c.id];
    if (sid && sid !== ROOT && !c.unmodifiable) walk(c, sid);
  }

  // Positions per folder.
  const pos = new Map<string, string>();
  for (const [parent, list] of kids) {
    const keys = list.map((sid) => {
      const b = state.base[sid];
      return b && b.parent === parent ? b.position : "";
    });
    // Only nodes already in this folder can be kept in place.
    const candidates = list.map((_, i) => i).filter((i) => keys[i] !== "");
    const kept = new Set<number>();
    for (const i of lis(candidates.map((c) => keys[c]!))) kept.add(candidates[i]!);
    let prev: string | null = null;
    list.forEach((sid, i) => {
      if (kept.has(i)) {
        prev = state.base[sid]!.position;
        pos.set(sid, prev);
        return;
      }
      let next: string | null = null;
      for (let j = i + 1; j < list.length; j++) {
        if (kept.has(j)) {
          next = state.base[list[j]!]!.position;
          break;
        }
      }
      // `prev` is always below `next` because kept positions increase and
      // assigned ones sit between them.
      const p = between(prev, next);
      pos.set(sid, p);
      prev = p;
    });
  }

  const bodies: OpBody[] = [];
  for (const sid of order) {
    const s = seen.get(sid)!;
    const b = state.base[sid];
    const position = pos.get(sid)!;
    const title = s.node.title;
    const url = s.node.url ?? null;
    if (!b) {
      bodies.push({
        type: "add",
        id: sid,
        parent: s.parent,
        position,
        kind: kindOf(s.node),
        title,
        ...(url !== null ? { url } : {}),
      });
      // Later siblings and children see this node as known.
      state.base[sid] = {
        parent: s.parent,
        position,
        kind: kindOf(s.node),
        title,
        url,
      };
      continue;
    }
    if (b.parent !== s.parent || b.position !== position) {
      const m: OpBody = { type: "move", id: sid, parent: s.parent, position };
      bodies.push(m);
      applyToBase(state.base, m);
    }
    if (b.title !== title || (b.kind === "bookmark" && b.url !== url)) {
      const u: OpBody = {
        type: "update",
        id: sid,
        ...(b.title !== title ? { title } : {}),
        ...(b.kind === "bookmark" && b.url !== url && url !== null ? { url } : {}),
      };
      bodies.push(u);
      applyToBase(state.base, u);
    }
  }

  // Removals last: nodes moved out of a removed folder were moved above.
  const present = new Set<string>([ROOT, ...ROLES, ...seen.keys()]);
  for (const [sid, b] of Object.entries(state.base)) {
    if (present.has(sid)) continue;
    if (b.parent !== null && !present.has(b.parent) && b.parent in state.base) continue;
    bodies.push({ type: "remove", id: sid });
  }
  for (const b of bodies) if (b.type === "remove") applyToBase(state.base, b);

  // Forget browser nodes that are gone, so the persisted state stays small.
  const live = new Set<string>();
  const collect = (n: LocalNode) => {
    live.add(n.id);
    n.children?.forEach(collect);
  };
  collect(root);
  for (const [chrome, sid] of Object.entries(state.map)) {
    if (!(sid in state.base) || !live.has(chrome)) delete state.map[chrome];
  }
  for (const chrome of Object.keys(state.ignored)) if (!live.has(chrome)) delete state.ignored[chrome];

  return bodies.map((body) => ({ op_id: nextOpId(state), ...body }) as Op);
}
