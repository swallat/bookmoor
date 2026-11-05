// Firefox has a fourth root, the bookmarks menu, which the server's fixed
// roles (toolbar, other, mobile) have no place for. This wrapper shows it to
// the diff as an ordinary folder "Bookmarks Menu" inside the "unfiled" root
// (which syncs as "other", Chrome's "Other bookmarks"). Everything inside
// then syncs like any folder, and Chrome simply sees a folder of that name.
//
// Only `getTree` (what the diff reads) shows the folder in "unfiled". Every
// other call sees Firefox's real layout, so indexes the applier computes for
// "unfiled" stay correct. The folder is marked `virtual`: the diff does not
// try to order it among its siblings.
//
// The folder itself cannot be renamed, moved or deleted in Firefox, so such
// changes arriving from the server are dropped here (a delete empties it);
// the next diff then sends the real state back, and the browsers converge.
// An edit of the folder on another browser is therefore reverted from Firefox.

import type { BookmarkApi, LocalNode } from "./local.ts";

export const MENU = "menu________";
const UNFILED = "unfiled_____";
/** Fixed, so the folder is the same whatever the browser's language. */
export const MENU_TITLE = "Bookmarks Menu";

function virtualise(root: LocalNode): void {
  const kids = root.children ?? [];
  const menu = kids.find((k) => k.id === MENU);
  const unfiled = kids.find((k) => k.id === UNFILED);
  if (!menu || !unfiled) return;
  root.children = kids.filter((k) => k !== menu);
  unfiled.children ??= [];
  menu.title = MENU_TITLE;
  menu.parentId = UNFILED;
  menu.index = unfiled.children.length;
  menu.virtual = true;
  unfiled.children.push(menu);
}

export function withMenu(api: BookmarkApi): BookmarkApi {
  return {
    getTree: async () => {
      const tree = await api.getTree();
      for (const root of tree) virtualise(root);
      return tree;
    },
    getSubTree: async (id) => {
      const out = await api.getSubTree(id);
      if (id === MENU && out[0]) out[0].title = MENU_TITLE;
      return out;
    },
    create: (a) => api.create(a),
    update: async (id, c) => {
      if (id === MENU) return;
      return api.update(id, c);
    },
    move: async (id, d) => {
      if (id === MENU) return;
      return api.move(id, d);
    },
    remove: async (id) => {
      if (id === MENU) return removeContent(api);
      return api.remove(id);
    },
    removeTree: async (id) => {
      if (id === MENU) return removeContent(api);
      return api.removeTree(id);
    },
  };
}

async function removeContent(api: BookmarkApi): Promise<void> {
  const [menu] = await api.getSubTree(MENU);
  for (const c of menu?.children ?? []) await api.removeTree(c.id);
}
