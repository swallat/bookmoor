"use strict";
// Bookmarks page: browse and edit one profile's saved tree. Every edit goes
// to the server as a normal change, so all devices of the profile follow.
// Uses h, api, toast, field and form from app.js.

let bmProfile = null; // profile id being shown
let bmQuery = "";
const bmClosed = new Set(); // folder ids the user collapsed in the sidebar
let bmSel = "toolbar"; // folder shown in the main pane

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

  if (!ix.byId.has(bmSel) || ix.byId.get(bmSel).kind !== "folder") bmSel = ix.byId.has("toolbar") ? "toolbar" : ix.root.id;
  const list = h("div", { class: "list bm-list" });
  const side = h("nav", { class: "bm-side list", "aria-label": "Folders" });
  const crumbs = h("nav", { class: "bm-crumbs", "aria-label": "Folder path" });
  const fill = () => {
    const had = document.activeElement && document.activeElement.dataset ? document.activeElement.dataset.k : null;
    side.replaceChildren(...bmSide(ix));
    crumbs.replaceChildren(...bmCrumbs(ix));
    list.replaceChildren(...bmRows(ix, bmQuery.trim().toLowerCase()));
    if (had) { const el = document.querySelector('[data-k="' + CSS.escape(had) + '"]'); if (el) el.focus(); }
  };

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
        onchange: (e) => { bmProfile = e.target.value; bmQuery = ""; bmClosed.clear(); bmSel = "toolbar"; render(); } },
        profiles.map((p) => h("option", { value: p.id, text: p.name, selected: p.id === bmProfile })))),
    h("p", { class: "muted", text: "This is what the server has saved for the profile. Changes here reach every browser of the profile on its next sync. Deleting a folder that holds bookmarks saves the tree as the copy for Restore under Profiles, replacing an older copy, and Restore replaces everything." }),
    h("label", { class: "check bm-check" },
      h("input", { type: "checkbox", checked: bmIcons(),
        onchange: (e) => { bmSetIcons(e.target.checked); fill(); } }),
      h("span", { text: "Show site icons" }),
      h("span", { class: "hint", text: " Your browser then asks each site for its icon, so those sites see your IP address. Off by default." })),
    h("div", { class: "bm-tools" }, search,
      h("button", { class: "btn small", type: "button", text: "Add bookmark",
        onclick: () => bmDialog(ix, { kind: "bookmark", parent: bmSel }) }),
      h("button", { class: "btn quiet small", type: "button", text: "Add folder",
        onclick: () => bmDialog(ix, { kind: "folder", parent: bmSel }) }),
      count > 1 && h("button", { class: "btn quiet small", type: "button", text: "Find duplicates",
        title: "Review links that are saved more than once, then remove the copies you pick",
        onclick: () => bmDuplicates() })),
    h("div", { class: "bm-split" }, side, h("div", { class: "bm-main" }, crumbs, list)));
}

/** Count of bookmarks in a folder, including everything below it (memoized per index). */
function bmCount(ix, f) {
  ix.counts = ix.counts || new Map();
  if (ix.counts.has(f.id)) return ix.counts.get(f.id);
  let c = 0;
  for (const n of ix.kids.get(f.id) || []) c += n.kind === "folder" ? bmCount(ix, n) : 1;
  ix.counts.set(f.id, c);
  return c;
}

/** Show a folder: leave search, unfold the path to it. */
function bmSelect(ix, id) {
  bmSel = id;
  bmQuery = "";
  const box = document.getElementById("bm-search");
  if (box) box.value = "";
  for (let p = ix.byId.get(id); p; p = ix.byId.get(p.parent)) bmClosed.delete(p.id);
  bmFill();
}

/** Sidebar: the folder tree with counts. Click selects, the caret folds. */
function bmSide(ix) {
  const out = [];
  const walk = (f, depth) => {
    for (const n of ix.kids.get(f.id) || []) {
      if (n.kind !== "folder") continue;
      const sub = (ix.kids.get(n.id) || []).some((k) => k.kind === "folder");
      const open = !bmClosed.has(n.id);
      const row = h("div", { class: "bm-node" + (n.id === bmSel ? " sel" : "") });
      row.style.setProperty("--d", String(depth));
          row.append(
        sub ? h("button", { class: "bm-twist", type: "button", "data-k": "t" + n.id, "aria-label": (open ? "Fold " : "Unfold ") + bmTitle(n),
          "aria-expanded": String(open), text: open ? "▾" : "▸",
          onclick: () => { open ? bmClosed.add(n.id) : bmClosed.delete(n.id); bmFill(); } })
          : h("span", { class: "bm-twist" }),
        h("button", { class: "bm-pick", type: "button", "data-k": "p" + n.id, "aria-current": n.id === bmSel ? "true" : null,
          onclick: () => bmSelect(ix, n.id) },
          h("span", { class: "name", text: bmTitle(n) }),
          h("span", { class: "tag", text: String(bmCount(ix, n)) })));
      out.push(row);
      if (sub && open) walk(n, depth + 1);
    }
  };
  const all = h("div", { class: "bm-node" + (bmSel === ix.root.id ? " sel" : "") });
  all.append(h("span", { class: "bm-twist" }),
    h("button", { class: "bm-pick", type: "button", "data-k": "pall", onclick: () => bmSelect(ix, ix.root.id) },
      h("span", { class: "name", text: "All bookmarks" }), h("span", { class: "tag", text: String(bmCount(ix, ix.root)) })));
  out.push(all);
  walk(ix.root, 0);
  return out;
}

function bmCrumbs(ix) {
  const chain = [];
  for (let n = ix.byId.get(bmSel); n && n.parent != null; n = ix.byId.get(n.parent)) chain.unshift(n);
  if (!chain.length) return [h("strong", { text: "All bookmarks" })];
  const out = [];
  chain.forEach((n, i) => {
    if (i) out.push(h("span", { class: "muted", "aria-hidden": "true", text: " / " }));
    out.push(i === chain.length - 1
      ? h("strong", { text: bmTitle(n) })
      : h("button", { class: "bm-crumb", type: "button", text: bmTitle(n), onclick: () => bmSelect(ix, n.id) }));
  });
  return out;
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
  const kids = ix.kids.get(bmSel) || [];
  kids.forEach((n, i) => out.push(bmRow(ix, n, 0, false, kids, i)));
  if (!out.length) out.push(h("p", { class: "empty", text: "This folder is empty." }));
  return out;
}

function bmRow(ix, n, depth, searching, siblings, i) {
  const protectedNode = Object.hasOwn(ROLE_LABELS, n.id);
  const row = h("div", { class: "row bm-row" + (n.kind === "folder" ? " folder" : "") });
  const acts = h("div", { class: "actions" });
  let main;
  if (n.kind === "folder") {
    main = h("button", { class: "bm-fold", type: "button", title: "Open folder",
      onclick: () => bmSelect(ix, n.id) },
      h("span", { class: "bm-folder-icon", "aria-hidden": "true", text: "📁" }),
      h("span", { class: "name", text: bmTitle(n) }), " ",
      h("span", { class: "tag", text: String(bmCount(ix, n)) }));
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

/** Review links saved more than once; only the copies the user ticks are removed. */
async function bmDuplicates() {
  const profile = bmProfile; // the select can change it while the dialog is open
  let groups;
  try { groups = await api("GET", "profiles/" + profile + "/duplicates"); }
  catch (e) { toast(e.message); return; }
  if (!groups.length) { toast("No duplicates found"); return; }
  const dlg = h("dialog", { class: "bm-dialog bm-dups", "aria-label": "Duplicate bookmarks" });
  const close = () => { dlg.close(); dlg.remove(); };
  const boxes = []; // [{ id, group, box }]
  const summary = h("p", { class: "muted", role: "status" });
  const go = h("button", { class: "btn", type: "button" });
  const update = () => {
    const picked = boxes.filter((b) => b.box.checked);
    let blocked = 0;
    groups.forEach((g, gi) => {
      const mine = boxes.filter((b) => b.group === gi);
      const all = mine.every((b) => b.box.checked);
      mine.forEach((b) => b.row.classList.toggle("conflict", all));
      if (all) blocked++;
    });
    summary.textContent = picked.length + " of " + boxes.length + " copies will be removed"
      + (blocked ? ". Keep at least one copy of every link (" + blocked + (blocked === 1 ? " group has" : " groups have") + " none left)." : ".");
    go.textContent = picked.length ? "Remove " + picked.length + (picked.length === 1 ? " bookmark" : " bookmarks") : "Nothing selected";
    go.disabled = !picked.length || blocked > 0;
  };
  const body = groups.map((g, gi) => h("fieldset", { class: "bm-dup" },
    h("legend", { class: "mono small", text: g.url }),
    ...g.items.map((it) => {
      const box = h("input", { type: "checkbox", checked: it.id !== g.keep, onchange: update,
        "aria-label": "Remove " + (it.title || it.url) + (it.path.length ? " in " + it.path.join(" / ") : "") });
      const row = h("label", { class: "bm-dup-row" }, box,
        h("span", { class: "main" },
          h("span", { class: "name", text: it.title || "(no title)" }),
          it.id === g.keep && h("span", { class: "tag", text: "suggested to keep" }),
          h("span", { class: "muted small", text: " in " + (it.path.join(" / ") || "top level") }),
          it.url !== g.url && h("span", { class: "mono muted small bm-dup-url", text: it.url })));
      boxes.push({ id: it.id, group: gi, box, row });
      return row;
    })));
  go.addEventListener("click", async () => {
    go.disabled = true;
    try {
      const r = await api("POST", "profiles/" + profile + "/duplicates/remove", { ids: boxes.filter((b) => b.box.checked).map((b) => b.id) });
      close();
      toast("Removed " + r.removed + (r.removed === 1 ? " bookmark" : " bookmarks") + ". Restore under Profiles brings them back.");
      render();
    } catch (e) {
      // Most likely the tree changed since the list was made: look again.
      close(); toast(e.message); render();
    }
  });
  dlg.append(
    h("h2", { text: groups.length + (groups.length === 1 ? " link is" : " links are") + " saved more than once" }),
    h("p", { class: "muted", text: "Copies are ticked for removal except one per link. Change the ticks as you like; nothing is removed until you press the button. The tree as it is now is kept for Restore under Profiles, and every browser of the profile follows on its next sync." }),
    h("div", { class: "bm-dup-list" }, body),
    summary,
    h("div", { class: "actions" }, go, h("button", { class: "btn quiet", type: "button", text: "Cancel", onclick: close })));
  dlg.addEventListener("cancel", () => dlg.remove());
  document.body.append(dlg);
  update();
  dlg.showModal();
}
