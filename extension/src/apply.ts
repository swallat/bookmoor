// Applying server changes to the browser. State (map and base) is updated
// together with each browser call, so the next flush-time diff sees no
// difference and nothing echoes back (docs/sync-architecture.md, 5.5).

import { dropSubtree } from "./diff.ts";
import { compareSiblings } from "./position.ts";
import {
  type BaseNode,
  type BookmarkApi,
  type LocalNode,
  type SyncState,
  ROLES,
  ROOT,
  nodeFromWire,
  reverseMap,
} from "./local.ts";
import type { Op } from "./wire.ts";

/** Index for a node among the local children of `parentChrome`. */
async function indexFor(
  api: BookmarkApi,
  state: SyncState,
  parentChrome: string,
  sid: string,
  position: string,
): Promise<number> {
  const [sub] = await api.getSubTree(parentChrome);
  const children = sub?.children ?? [];
  let idx = 0;
  for (const c of children) {
    const csid = state.map[c.id];
    const b = csid ? state.base[csid] : undefined;
    if (!csid || csid === sid || !b) continue;
    if (compareSiblings([b.position, csid], [position, sid]) < 0) {
      idx = children.indexOf(c) + 1;
    }
  }
  return Math.min(idx, children.length);
}

/** Whether a local subtree holds anything the server does not know yet. */
function unsynced(n: LocalNode, state: SyncState): boolean {
  if (!(n.id in state.map)) return true;
  return (n.children ?? []).some((c) => unsynced(c, state));
}

/**
 * Remove a node the server deleted. Edit beats delete: whatever the server
 * has not seen yet stays, together with the folders leading to it; those
 * folders lose their map entry so the next diff adds them again.
 */
async function removeLocal(api: BookmarkApi, state: SyncState, sid: string): Promise<void> {
  const rev = reverseMap(state.map);
  const chrome = rev.get(sid);
  if (chrome) {
    const [sub] = await api.getSubTree(chrome).catch(() => [undefined]);
    if (sub && !unsynced(sub, state)) {
      await api.removeTree(chrome);
    } else if (sub) {
      const purge = async (n: LocalNode): Promise<boolean> => {
        let remains = !(n.id in state.map);
        for (const c of n.children ?? []) if (await purge(c)) remains = true;
        if (!remains) await api.remove(n.id);
        return remains;
      };
      await purge(sub);
    }
  }
  for (const id of dropSubtree(state.base, sid)) {
    const c = rev.get(id);
    if (c) delete state.map[c];
  }
}

/** Overwrite title/url only while the browser still holds the base value. */
async function applyFields(
  api: BookmarkApi,
  chrome: string,
  b: BaseNode,
  title: string | null | undefined,
  url: string | null | undefined,
): Promise<void> {
  const [sub] = await api.getSubTree(chrome);
  if (!sub) return;
  const changes: { title?: string; url?: string } = {};
  if (title != null && sub.title === b.title) changes.title = title;
  if (url != null && (sub.url ?? null) === b.url) changes.url = url;
  if (changes.title !== undefined || changes.url !== undefined) {
    await api.update(chrome, changes);
  }
  if (changes.title !== undefined) b.title = changes.title;
  if (changes.url !== undefined) b.url = changes.url;
}

/**
 * Apply server ops in order. Local edits not yet flushed win: a field is only
 * overwritten while the browser still holds the base value.
 */
export async function applyRemote(api: BookmarkApi, state: SyncState, ops: Op[]): Promise<void> {
  for (const op of ops) {
    const rev = reverseMap(state.map);
    switch (op.type) {
      case "add": {
        if (op.id === ROOT || (ROLES as readonly string[]).includes(op.id)) break;
        const parentChrome = rev.get(op.parent);
        if (!parentChrome) break; // parent unknown here; a later resync repairs it
        const have = rev.get(op.id);
        const index = await indexFor(api, state, parentChrome, op.id, op.position);
        if (have && state.base[op.id]) {
          // Upsert of a node we already have.
          await applyFields(api, have, state.base[op.id]!, op.title, op.url);
          await api.move(have, { parentId: parentChrome, index });
          state.base[op.id]!.parent = op.parent;
          state.base[op.id]!.position = op.position;
        } else {
          const made = await api.create({
            parentId: parentChrome,
            index,
            title: op.title,
            ...(op.kind === "bookmark" && op.url ? { url: op.url } : {}),
          });
          state.map[made.id] = op.id;
          state.base[op.id] = nodeFromWire({
            id: op.id,
            kind: op.kind,
            parent: op.parent,
            position: op.position,
            title: op.title,
            url: op.url,
          });
        }
        break;
      }
      case "update": {
        const chrome = rev.get(op.id);
        const b = state.base[op.id];
        if (chrome && b) await applyFields(api, chrome, b, op.title, op.url);
        break;
      }
      case "move": {
        const chrome = rev.get(op.id);
        const parentChrome = rev.get(op.parent);
        const b = state.base[op.id];
        if (!chrome || !parentChrome || !b) break;
        const index = await indexFor(api, state, parentChrome, op.id, op.position);
        await api.move(chrome, { parentId: parentChrome, index });
        b.parent = op.parent;
        b.position = op.position;
        break;
      }
      case "remove":
        await removeLocal(api, state, op.id);
        break;
    }
  }
}
