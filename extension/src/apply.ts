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

/** Whether a local subtree holds anything the server does not know yet. */
function unsynced(n: LocalNode, state: SyncState): boolean {
  if (!(n.id in state.map)) return true;
  return (n.children ?? []).some((c) => unsynced(c, state));
}

/** Save the state every this many ops, so a killed worker loses little. */
const SAVE_EVERY = 25;

class Applier {
  private rev: Map<string, string>;
  /** Child chrome ids per folder, loaded once and kept current for creates. */
  private kids = new Map<string, string[]>();
  /** Whether `move` counts the moved node itself; learnt on first use. */
  private preRemoval: boolean | null = null;

  private api: BookmarkApi;
  private state: SyncState;

  constructor(api: BookmarkApi, state: SyncState) {
    this.api = api;
    this.state = state;
    this.rev = reverseMap(state.map);
  }

  private async children(parent: string): Promise<string[]> {
    let k = this.kids.get(parent);
    if (!k) {
      const [sub] = await this.api.getSubTree(parent);
      k = (sub?.children ?? []).map((c) => c.id);
      this.kids.set(parent, k);
    }
    return k;
  }

  /**
   * Where a node at `position` goes among the local children of `parent`:
   * `pre` counts the node itself if it is already there (what Chrome's
   * `move` expects when moving down), `final` is its index afterwards.
   */
  private async target(
    parent: string,
    sid: string,
    position: string,
    self?: string,
  ): Promise<{ pre: number; final: number; same: boolean }> {
    const kids = await this.children(parent);
    let pre = 0;
    kids.forEach((cid, i) => {
      const csid = this.state.map[cid];
      const b = csid ? this.state.base[csid] : undefined;
      if (!csid || csid === sid || !b) return;
      if (compareSiblings([b.position, csid], [position, sid]) < 0) pre = i + 1;
    });
    pre = Math.min(pre, kids.length);
    const selfIdx = self ? kids.indexOf(self) : -1;
    const same = selfIdx >= 0;
    return { pre, final: same && selfIdx < pre ? pre - 1 : pre, same };
  }

  /**
   * Move within or into a folder. Browsers disagree on what `index` means
   * when a node moves down inside its own folder (Chrome counts the node
   * itself, Firefox does not), so the first such move is checked and the
   * answer remembered.
   */
  private async moveTo(chrome: string, parent: string, sid: string, position: string) {
    const t = await this.target(parent, sid, position, chrome);
    const down = t.same && t.final !== t.pre;
    const index = down && this.preRemoval === true ? t.pre : t.final;
    await this.api.move(chrome, { parentId: parent, index });
    this.kids.clear();
    if (down && this.preRemoval === null) {
      const now = await this.children(parent);
      if (now.indexOf(chrome) === t.final) {
        this.preRemoval = false;
      } else {
        this.preRemoval = true;
        await this.api.move(chrome, { parentId: parent, index: t.pre });
        this.kids.clear();
      }
    }
  }

  private async removeLocal(sid: string): Promise<void> {
    const chrome = this.rev.get(sid);
    if (chrome) {
      const [sub] = await this.api.getSubTree(chrome).catch(() => [undefined]);
      if (sub && !unsynced(sub, this.state)) {
        await this.api.removeTree(chrome);
      } else if (sub) {
        // Edit beats delete: what the server has not seen stays, with the
        // folders leading to it (they lose their map entry, see below).
        const purge = async (n: LocalNode): Promise<boolean> => {
          let remains = !(n.id in this.state.map);
          for (const c of n.children ?? []) if (await purge(c)) remains = true;
          if (!remains) await this.api.remove(n.id);
          return remains;
        };
        await purge(sub);
      }
      this.kids.clear();
    }
    for (const id of dropSubtree(this.state.base, sid)) {
      const c = this.rev.get(id);
      if (c) delete this.state.map[c];
      this.rev.delete(id);
    }
  }

  /** Overwrite title/url only while the browser still holds the base value. */
  private async fields(
    chrome: string,
    b: BaseNode,
    title: string | null | undefined,
    url: string | null | undefined,
  ): Promise<void> {
    const [sub] = await this.api.getSubTree(chrome);
    if (!sub) return;
    const changes: { title?: string; url?: string } = {};
    if (title != null && sub.title === b.title) changes.title = title;
    if (url != null && (sub.url ?? null) === b.url) changes.url = url;
    if (changes.title !== undefined || changes.url !== undefined) {
      await this.api.update(chrome, changes);
    }
    if (changes.title !== undefined) b.title = changes.title;
    if (changes.url !== undefined) b.url = changes.url;
  }

  async apply(op: Op): Promise<void> {
    const { state, api } = this;
    switch (op.type) {
      case "add": {
        if (op.id === ROOT || (ROLES as readonly string[]).includes(op.id)) return;
        const parent = this.rev.get(op.parent);
        if (!parent) return; // parent unknown here; a later resync repairs it
        const have = this.rev.get(op.id);
        const b = state.base[op.id];
        if (have && b) {
          // Upsert of a node we already have.
          await this.fields(have, b, op.title, op.url);
          await this.moveTo(have, parent, op.id, op.position);
          b.parent = op.parent;
          b.position = op.position;
        } else {
          const { pre: index } = await this.target(parent, op.id, op.position);
          const made = await api.create({
            parentId: parent,
            index,
            title: op.title,
            ...(op.kind === "bookmark" && op.url ? { url: op.url } : {}),
          });
          this.kids.get(parent)?.splice(index, 0, made.id);
          state.map[made.id] = op.id;
          this.rev.set(op.id, made.id);
          state.base[op.id] = nodeFromWire({
            id: op.id,
            kind: op.kind,
            parent: op.parent,
            position: op.position,
            title: op.title,
            url: op.url,
          });
        }
        return;
      }
      case "update": {
        const chrome = this.rev.get(op.id);
        const b = state.base[op.id];
        if (chrome && b) await this.fields(chrome, b, op.title, op.url);
        return;
      }
      case "move": {
        const chrome = this.rev.get(op.id);
        const parent = this.rev.get(op.parent);
        const b = state.base[op.id];
        if (!chrome || !parent || !b) return;
        await this.moveTo(chrome, parent, op.id, op.position);
        b.parent = op.parent;
        b.position = op.position;
        return;
      }
      case "remove":
        await this.removeLocal(op.id);
        return;
    }
  }
}

/**
 * Apply server ops in order. Local edits not yet flushed win: a field is only
 * overwritten while the browser still holds the base value. An op the browser
 * refuses (say a move that would make a cycle with an unsynced local move)
 * does not stop the rest; its node id is returned so the caller can fetch the
 * server's version instead. `save` is called every few ops.
 */
export async function applyRemote(
  api: BookmarkApi,
  state: SyncState,
  ops: Op[],
  save?: () => Promise<void>,
): Promise<Set<string>> {
  const a = new Applier(api, state);
  const failed = new Set<string>();
  let n = 0;
  for (const op of ops) {
    try {
      await a.apply(op);
    } catch {
      failed.add(op.id);
    }
    if (save && ++n % SAVE_EVERY === 0) await save();
  }
  return failed;
}
