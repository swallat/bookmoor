// The browser side as the sync code sees it: a small interface over
// chrome.bookmarks (so tests can use an in-memory fake) and the persisted
// sync state.

import type { Kind, Op, WireNode } from "./wire.ts";

/** The subset of chrome.bookmarks.BookmarkTreeNode the sync code needs. */
export interface LocalNode {
  id: string;
  parentId?: string;
  index?: number;
  title: string;
  url?: string;
  children?: LocalNode[];
  unmodifiable?: string;
  /** Set by Firefox; separators have no counterpart on the server. */
  type?: string;
}

export interface BookmarkApi {
  /** `[root]`, the full tree. */
  getTree(): Promise<LocalNode[]>;
  getSubTree(id: string): Promise<LocalNode[]>;
  create(a: { parentId: string; index?: number; title: string; url?: string }): Promise<LocalNode>;
  update(id: string, changes: { title?: string; url?: string }): Promise<unknown>;
  move(id: string, dest: { parentId: string; index?: number }): Promise<unknown>;
  remove(id: string): Promise<void>;
  removeTree(id: string): Promise<void>;
}

/** What we last agreed on with the server, in server terms. */
export interface BaseNode {
  parent: string | null;
  position: string;
  kind: Kind;
  title: string;
  url: string | null;
}

export interface SyncState {
  endpointId: string;
  /** Last server revision applied. */
  rev: number;
  /** Counter for op ids; never reused. */
  counter: number;
  /** chrome id -> server id. */
  map: Record<string, string>;
  /** server id -> node. Includes the effect of `pending`. */
  base: Record<string, BaseNode>;
  /** Sent (or about to be) but not yet acknowledged; resent as is. */
  pending: Op[];
  /** Server and token this state belongs to; a change starts afresh. */
  identity: string;
  /** Whether `move` counts the moved node itself; learnt on the first real move down. */
  preRemoval?: boolean;
  /** Browser nodes the server refused (invalid); never offered again. */
  ignored: Record<string, true>;
}

export const ROOT = "root";
export const ROLES = ["toolbar", "other", "mobile"] as const;

export function emptyState(endpointId: string, identity = ""): SyncState {
  return { endpointId, identity, rev: 0, counter: 0, map: {}, base: {}, pending: [], ignored: {} };
}

export function kindOf(n: LocalNode): Kind {
  return n.url === undefined ? "folder" : "bookmark";
}

const CHROME_ROLES: Record<string, string> = { "1": "toolbar", "2": "other", "3": "mobile" };
// Firefox has a fourth folder, the bookmarks menu, which the server's fixed
// set of roles has no place for; menu.ts presents it as a folder in "other".
const FIREFOX_ROLES: Record<string, string> = {
  toolbar_____: "toolbar",
  unfiled_____: "other",
  mobile______: "mobile",
};

/** Pair the browser's root and its role folders with the server's. */
export function mapRoots(state: SyncState, tree: LocalNode[]): void {
  const root = tree[0];
  if (!root) return;
  state.map[root.id] = ROOT;
  const kids = root.children ?? [];
  const firefox = kids.some((c) => c.id in FIREFOX_ROLES);
  const byId = firefox ? FIREFOX_ROLES : CHROME_ROLES;
  kids.forEach((c, i) => {
    // Unknown ids in other Chromium browsers: the first three in order.
    const role = byId[c.id] ?? (firefox ? undefined : ROLES[i]);
    if (role) state.map[c.id] = role;
  });
}

export function nodeFromWire(n: WireNode): BaseNode {
  return {
    parent: n.parent ?? null,
    position: n.position,
    kind: n.kind,
    title: n.title,
    url: n.url ?? null,
  };
}

export function reverseMap(map: Record<string, string>): Map<string, string> {
  return new Map(Object.entries(map).map(([c, s]) => [s, c]));
}

export function newServerId(): string {
  const b = crypto.getRandomValues(new Uint8Array(9));
  return "n_" + Array.from(b, (x) => x.toString(16).padStart(2, "0")).join("");
}

export function nextOpId(state: SyncState): string {
  state.counter += 1;
  return `${state.endpointId}-${state.counter}`;
}
