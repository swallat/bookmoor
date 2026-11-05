// Firefox has a fourth root, the bookmarks menu, which the server's fixed
// roles (toolbar, other, mobile) have no place for. This wrapper shows it to
// the sync code as an ordinary folder "Bookmarks Menu" inside the "unfiled"
// root (which syncs as "other", Chrome's "Other bookmarks"). Everything inside
// then syncs like any folder, and Chrome simply sees a folder of that name.
//
// The folder itself cannot be renamed, moved or deleted in Firefox, so such
// changes arriving from the server are dropped here (a delete empties it);
// the next diff then sends the real state back, and the browsers converge.

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
  unfiled.children.push(menu);
}

function walk(n: LocalNode, f: (n: LocalNode) => void): void {
  f(n);
  n.children?.forEach((c) => walk(c, f));
}

export function withMenu(api: BookmarkApi): BookmarkApi {
  const view = async (nodes: LocalNode[]): Promise<LocalNode[]> => {
    for (const n of nodes) {
      if (n.id === MENU) n.title = MENU_TITLE;
      // Only the whole tree has the roots side by side.
      if (n.parentId === undefined && n.children?.some((c) => c.id === MENU)) virtualise(n);
    }
    return nodes;
  };
  return {
    getTree: async () => view(await api.getTree()),
    getSubTree: async (id) => {
      const out = await api.getSubTree(id);
      if (id === UNFILED && out[0]) {
        const [menu] = await api.getSubTree(MENU).catch(() => []);
        if (menu) {
          menu.title = MENU_TITLE;
          menu.parentId = UNFILED;
          menu.index = out[0].children?.length ?? 0;
          out[0].children = [...(out[0].children ?? []), menu];
        }
      }
      for (const n of out) walk(n, (x) => x.id === MENU && (x.title = MENU_TITLE));
      return out;
    },
    create: (a) => api.create(a),
    update: async (id, c) => {
      if (id === MENU) return;
      return api.update(id, c);
    },
    move: async (id, d) => {
      if (id === MENU) return;
      // The menu folder counts as the last child of "unfiled" here; the real
      // folder has no such child, so an index past the others means the end.
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
