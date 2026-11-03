import type { SyncState } from "./local.ts";
import type { BookmarkApi, LocalNode } from "./local.ts";
import type { StateStore } from "./flow.ts";

const KEY = "sync-state";

/** Sync state in chrome.storage.local (the map is O(bookmarks)). */
export const chromeStateStore: StateStore = {
  async load() {
    const v = await chrome.storage.local.get(KEY);
    return (v[KEY] as SyncState | undefined) ?? null;
  },
  async save(s) {
    await chrome.storage.local.set({ [KEY]: s });
  },
};

export const chromeBookmarks: BookmarkApi = {
  getTree: () => chrome.bookmarks.getTree() as Promise<LocalNode[]>,
  getSubTree: (id) => chrome.bookmarks.getSubTree(id) as Promise<LocalNode[]>,
  create: (a) => chrome.bookmarks.create(a) as Promise<LocalNode>,
  update: (id, c) => chrome.bookmarks.update(id, c),
  move: (id, d) => chrome.bookmarks.move(id, d),
  remove: (id) => chrome.bookmarks.remove(id),
  removeTree: (id) => chrome.bookmarks.removeTree(id),
};
