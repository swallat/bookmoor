"use strict";
// Bookmarks page: browse and edit one profile's saved tree. Every edit goes
// to the server as a normal change, so all devices of the profile follow.
// Uses h, api, toast, field and form from app.js.

let bmProfile = null; // profile id being shown
let bmQuery = "";
const bmClosed = new Set(); // folder ids the user collapsed

// Site icons are off until the user turns them on: each one is fetched by the
// browser from the bookmarked site itself (/favicon.ico), which tells that
// site you have the link. Remembered per browser.
function bmIcons() {
  try { return localStorage.getItem("bookmoor.icons") === "1"; } catch { return false; }
}
function bmSetIcons(on) {
  try { localStorage.setItem("bookmoor.icons", on ? "1" : "0"); } catch { /* not remembered */ }
}

function bmIcon(url) {
  let origin;
  try { const u = new URL(url); if (u.protocol !== "https:") return null; origin = u.origin; } catch { return null; }
  const img = h("img", { class: "bm-icon", src: origin + "/favicon.ico", alt: "", width: "20", height: "20",
    loading: "lazy", referrerpolicy: "no-referrer", decoding: "async" });
  img.addEventListener("error", () => img.classList.add("missing"));
  return img;
}

const ROLE_LABELS = { toolbar: "Bookmarks bar", other: "Other bookmarks", mobile: "Mobile bookmarks" };

function bmIndex(nodes) {
  const byId = new Map();
  const kids = new Map();
  for (const n of nodes) {
    byId.set(n.id, n);
    if (n.parent != null) {
      if (!kids.has(n.parent)) kids.set(n.parent, []);
      kids.get(n.parent).push(n);
    }
  }
  // The server sends parent-first and in order, so siblings are already sorted.
  return { byId, kids, root: nodes.find((n) => n.parent == null) };
}

function bmTitle(n) {
  return (Object.hasOwn(ROLE_LABELS, n.id) ? ROLE_LABELS[n.id] : null) || n.title || (n.kind === "folder" ? "(unnamed folder)" : n.url);
}

function bmHost(url) {
  try { return new URL(url).host || url; } catch { return url; }
}

function bmSafeHref(url) {
  try {
    const u = new URL(url);
    return u.protocol === "https:" || u.protocol === "http:" ? u.href : null;
  } catch { return null; }
}

/** Folder path of a node, for search results and the move list. */
function bmPath(ix, n) {
  const parts = [];
  for (let p = ix.byId.get(n.parent); p && p.parent != null; p = ix.byId.get(p.parent)) parts.unshift(bmTitle(p));
  return parts.join(" / ");
}

async function bookmarksView() {
  const profiles = await api("GET", "profiles");
  if (!profiles.length) return h("p", { class: "muted", text: "No profile yet." });
  if (!profiles.some((p) => p.id === bmProfile)) bmProfile = profiles[0].id;
  const tree = await api("GET", "profiles/" + bmProfile + "/tree");
  const ix = bmIndex(tree.nodes);
  const count = tree.nodes.filter((n) => n.kind === "bookmark").length;

  const list = h("div", { class: "list bm-list" });
  const fill = () => list.replaceChildren(...bmRows(ix, bmQuery.trim().toLowerCase()));

  const edit = async (body, done) => {
    try { await api("POST", "profiles/" + bmProfile + "/edit", body); if (done) toast(done); render(); }
    catch (e) { toast(e.message); render(); }
  };
  bmEdit = edit;
  bmFill = fill;
  fill();

  const search = h("input", { id: "bm-search", type: "text", placeholder: "Search titles and URLs", value: bmQuery,
    autocomplete: "off", "aria-label": "Search bookmarks",
    oninput: (e) => { bmQuery = e.target.value; fill(); } });

  return h("div", { class: "stack" },
    h("div", { class: "head" },
      h("h2", { text: count + (count === 1 ? " bookmark" : " bookmarks") }),
      profiles.length > 1 && h("select", { class: "bm-profile", "aria-label": "Profile",
        onchange: (e) => { bmProfile = e.target.value; bmQuery = ""; bmClosed.clear(); render(); } },
        profiles.map((p) => h("option", { value: p.id, text: p.name, selected: p.id === bmProfile })))),
    h("p", { class: "muted", text: "This is what the server has saved for the profile. Changes here reach every browser of the profile on its next sync. Deleting a folder that holds bookmarks saves the tree as the copy for Restore under Profiles, replacing an older copy, and Restore replaces everything." }),
    h("label", { class: "check" },
      h("input", { type: "checkbox", checked: bmIcons(),
        onchange: (e) => { bmSetIcons(e.target.checked); fill(); } }),
      h("span", { text: "Show site icons" }),
      h("span", { class: "hint", text: " Your browser then asks each site for its icon, so those sites see your IP address. Off by default." })),
    h("div", { class: "bm-tools" }, search,
      h("button", { class: "btn small", type: "button", text: "Add bookmark",
        onclick: () => bmDialog(ix, { kind: "bookmark", parent: "toolbar" }) }),
      h("button", { class: "btn quiet small", type: "button", text: "Add folder",
        onclick: () => bmDialog(ix, { kind: "folder", parent: "toolbar" }) })),
    list);
}

let bmEdit = null;
let bmFill = null;

function bmRows(ix, q) {
  const out = [];
  if (q) {
    const hits = [...ix.byId.values()].filter((n) => n.kind === "bookmark"
      && ((n.title || "").toLowerCase().includes(q) || n.url.toLowerCase().includes(q)));
    if (!hits.length) return [h("p", { class: "empty", text: "Nothing matches “" + bmQuery.trim() + "”." })];
    for (const n of hits.slice(0, 200)) out.push(bmRow(ix, n, 0, true));
    if (hits.length > 200) out.push(h("p", { class: "empty", text: "Showing 200 of " + hits.length + ". Narrow the search." }));
    return out;
  }
  const walk = (folder, depth) => {
    const kids = ix.kids.get(folder.id) || [];
    kids.forEach((n, i) => {
      out.push(bmRow(ix, n, depth, false, kids, i));
      if (n.kind === "folder" && !bmClosed.has(n.id)) walk(n, depth + 1);
    });
  };
  walk(ix.root, 0);
  if (!out.length) out.push(h("p", { class: "empty", text: "No bookmarks saved yet." }));
  return out;
}

function bmRow(ix, n, depth, searching, siblings, i) {
  const protectedNode = Object.hasOwn(ROLE_LABELS, n.id);
  const row = h("div", { class: "row bm-row" + (n.kind === "folder" ? " folder" : "") });
  row.style.paddingLeft = (16 + depth * 20) + "px";
  const acts = h("div", { class: "actions" });
  let main;
  if (n.kind === "folder") {
    const open = !bmClosed.has(n.id);
    const nkids = (ix.kids.get(n.id) || []).length;
    main = h("button", { class: "bm-fold", type: "button", "aria-expanded": String(open),
      onclick: () => { open ? bmClosed.add(n.id) : bmClosed.delete(n.id); bmFill(); } },
      h("span", { class: "bm-caret", "aria-hidden": "true", text: open ? "▾" : "▸" }),
      h("span", { class: "name", text: bmTitle(n) }), " ",
      h("span", { class: "tag", text: String(nkids) }));
    acts.append(
      h("button", { class: "btn quiet small", type: "button", text: "Add here", title: "Add a bookmark to this folder",
        onclick: () => bmDialog(ix, { kind: "bookmark", parent: n.id }) }));
  } else {
    const href = bmSafeHref(n.url);
    main = h("div", { class: "main" },
      bmIcons() && bmIcon(n.url),
      href
        ? h("a", { class: "name", href, target: "_blank", rel: "noopener noreferrer", text: bmTitle(n) })
        : h("span", { class: "name", text: bmTitle(n) }),
      h("p", { class: "bm-url muted small", title: n.url, text: bmHost(n.url) }),
      h("p", { class: "bm-full mono muted", text: n.url }),
      searching && bmPath(ix, n) && h("p", { class: "muted small", text: "in " + bmPath(ix, n) }));
  }
  if (!protectedNode) {
    if (!searching && siblings) {
      acts.append(
        h("button", { class: "btn quiet small", type: "button", text: "↑", "aria-label": "Move up", disabled: i === 0,
          onclick: () => bmEdit({ type: "move", id: n.id, parent: n.parent, before: siblings[i - 1].id }) }),
        h("button", { class: "btn quiet small", type: "button", text: "↓", "aria-label": "Move down", disabled: i === siblings.length - 1,
          onclick: () => bmEdit({ type: "move", id: n.id, parent: n.parent, before: i + 2 < siblings.length ? siblings[i + 2].id : null }) }));
    }
    acts.append(
      h("button", { class: "btn quiet small", type: "button", text: "Edit", onclick: () => bmDialog(ix, { edit: n }) }),
      confirmButton("Delete", "Delete?", () => bmEdit({ type: "remove", id: n.id }, "Deleted " + bmTitle(n))));
  }
  row.append(main, acts);
  return row;
}

/** Folders as a flat, indented list, without `skip` and what is inside it. */
function bmFolders(ix, skip) {
  const out = [];
  const walk = (f, depth) => {
    for (const n of ix.kids.get(f.id) || []) {
      if (n.kind !== "folder" || n.id === skip) continue;
      out.push({ id: n.id, label: " ".repeat(depth) + bmTitle(n) });
      walk(n, depth + 1);
    }
  };
  walk(ix.root, 0);
  return out;
}

/** Add (init.kind, init.parent) or edit (init.edit) a bookmark or folder. */
function bmDialog(ix, init) {
  const n = init.edit;
  const kind = n ? n.kind : init.kind;
  const isBookmark = kind === "bookmark";
  const dlg = h("dialog", { class: "bm-dialog", "aria-label": (n ? "Edit " : "Add ") + (isBookmark ? "bookmark" : "folder") });
  const folders = bmFolders(ix, n && n.kind === "folder" ? n.id : null);
  const parent = n ? n.parent : init.parent;
  const close = () => { dlg.close(); dlg.remove(); };
  const f = form([
    h("h2", { text: (n ? "Edit " : "Add ") + (isBookmark ? "bookmark" : "folder") }),
    h("div", { class: "stack-sm" },
      field("bm-title", "Title", { type: "text", maxlength: "1000", value: n ? n.title : "", autofocus: true }),
      isBookmark && field("bm-url", "URL", { type: "text", inputmode: "url", maxlength: "2000", required: true,
        value: n ? n.url : "", placeholder: "https://example.com", autocomplete: "off" }),
      h("div", { class: "field" },
        h("label", { for: "bm-parent", text: "Folder" }),
        h("select", { id: "bm-parent", name: "bm-parent" },
          folders.map((o) => h("option", { value: o.id, text: o.label, selected: o.id === parent }))))),
  ], n ? "Save" : "Add", async (fd) => {
    const title = fd.get("bm-title").trim();
    const url = isBookmark ? fd.get("bm-url").trim() : undefined;
    const target = fd.get("bm-parent");
    if (isBookmark && !url) throw new Error("Enter a URL.");
    if (!isBookmark && !title) throw new Error("Enter a folder name.");
    if (n) {
      const body = { type: "update", id: n.id };
      if (title !== n.title) body.title = title;
      if (isBookmark && url !== n.url) body.url = url;
      if (body.title !== undefined || body.url !== undefined) await api("POST", "profiles/" + bmProfile + "/edit", body);
      if (target !== n.parent) {
        try { await api("POST", "profiles/" + bmProfile + "/edit", { type: "move", id: n.id, parent: target }); }
        catch (e) { close(); toast("Saved the changes, but moving failed: " + e.message); render(); return; }
      }
    } else {
      await api("POST", "profiles/" + bmProfile + "/edit", { type: "add", parent: target, kind, title, url });
    }
    close();
    toast(n ? "Saved" : "Added");
    render();
  });
  f.append(h("button", { class: "btn quiet", type: "button", text: "Cancel", onclick: close }));
  dlg.append(f);
  dlg.addEventListener("cancel", () => dlg.remove());
  document.body.append(dlg);
  dlg.showModal();
}
