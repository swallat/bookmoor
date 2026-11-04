"use strict";
// bookmoor admin page. Plain DOM, no framework; user data only ever reaches
// the page through textContent, never innerHTML.

const app = document.getElementById("app");
let me = null;
let tab = "devices";
let lastToken = null; // shown once, dropped when the page changes

function h(tag, attrs, ...kids) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (v === false || v == null) continue;
    if (k.startsWith("on")) el.addEventListener(k.slice(2), v);
    else if (k === "text") el.textContent = v;
    else el.setAttribute(k, v === true ? "" : v);
  }
  for (const kid of kids.flat()) {
    if (kid != null && kid !== false) el.append(kid);
  }
  return el;
}

async function api(method, path, body) {
  const res = await fetch("ui/api/" + path, {
    method,
    credentials: "same-origin",
    headers: { "X-Bookmoor-UI": "1", ...(body ? { "Content-Type": "application/json" } : {}) },
    body: body ? JSON.stringify(body) : undefined,
  });
  let data = null;
  try { data = await res.json(); } catch { /* empty body */ }
  if (!res.ok) {
    const wasSignedIn = Boolean(me);
    if (res.status === 401 && wasSignedIn) { me = null; await start(); }
    const err = new Error(
      res.status === 429 ? "Too many attempts. Wait a moment and try again."
        : res.status === 401 && wasSignedIn ? "Your session ended. Sign in again."
        : res.status === 401 ? (path === "setup" ? "That admin token is not valid." : "Name or password is wrong.")
        : (data && data.error) || "Something went wrong (" + res.status + ").");
    err.status = res.status;
    throw err;
  }
  return data;
}

function toast(msg) {
  document.querySelectorAll(".toast").forEach((t) => t.remove());
  const t = h("div", { class: "toast", role: "status", text: msg });
  document.body.append(t);
  setTimeout(() => t.remove(), 4000);
}

function field(id, label, attrs, hint) {
  return h("div", { class: "field" },
    h("label", { for: id, text: label }),
    h("input", { id, name: id, ...attrs }),
    hint && h("p", { class: "hint", id: id + "-hint", text: hint }));
}

/** A form whose submit button shows progress and whose errors land inline. */
function form(kids, submitLabel, onSubmit, extra) {
  const err = h("p", { class: "error", role: "alert" });
  const btn = h("button", { class: "btn", type: "submit", text: submitLabel });
  // An inline form keeps its button on the field row.
  if (extra && extra.inline) kids[0].append(btn);
  const f = h("form", { novalidate: true,
    onsubmit: async (e) => {
      e.preventDefault();
      err.textContent = "";
      btn.disabled = true;
      try { await onSubmit(new FormData(f), f); }
      catch (x) { err.textContent = x.message; }
      finally { btn.disabled = false; }
    } }, kids, err, !(extra && extra.inline) && btn);
  return f;
}

// ---- signed out -----------------------------------------------------------

function centered(title, lead, body) {
  app.replaceChildren(h("main", { class: "center" },
    h("img", { class: "logo", src: "ui/logo.svg", alt: "" }),
    h("h1", { text: title }),
    lead && h("p", { class: "muted", text: lead }),
    body));
}

function loginView() {
  document.title = "Sign in · bookmoor";
  centered("Sign in to bookmoor", null,
    form([
      h("div", { class: "stack-sm" },
        field("name", "User name", { type: "text", autocomplete: "username", required: true, autofocus: true }),
        field("password", "Password", { type: "password", autocomplete: "current-password", required: true })),
    ], "Sign in", async (fd) => {
      me = (await api("POST", "login", { name: fd.get("name"), password: fd.get("password") })).user;
      await start();
    }));
}

function setupView() {
  document.title = "Set up · bookmoor";
  centered("Set up bookmoor",
    "Nobody can sign in yet. Use the admin token to create the first admin. The server printed it when it first started; BOOKMOOR_ADMIN_TOKEN sets it.",
    form([
      h("div", { class: "stack-sm" },
        field("admin_token", "Admin token", { type: "password", autocomplete: "off", required: true }),
        field("name", "Your user name", { type: "text", autocomplete: "username", required: true },
          "Letters, digits, dot, dash, underscore. After an upgrade, use owner to keep your existing bookmarks."),
        field("password", "Password", { type: "password", autocomplete: "new-password", required: true }, "At least 10 characters.")),
    ], "Create admin", async (fd) => {
      me = (await api("POST", "setup", {
        admin_token: fd.get("admin_token"), name: fd.get("name"), password: fd.get("password"),
      })).user;
      await start();
    }));
}

// ---- signed in ------------------------------------------------------------

function shell(content) {
  const tabs = [["devices", "Devices"], ["profiles", "Profiles"], ...(me.admin ? [["users", "Users"]] : []), ["account", "Account"]];
  app.replaceChildren(h("div", { class: "shell" },
    h("header", { class: "bar" },
      h("img", { src: "ui/logo.svg", alt: "" }),
      h("span", { class: "brand", text: "bookmoor" }),
      h("div", { class: "who" },
        h("span", { class: "muted small", text: me.name }),
        h("button", { class: "btn quiet small", type: "button", text: "Sign out", onclick: signOut }))),
    h("nav", { class: "tabs", role: "tablist", "aria-label": "Sections" },
      tabs.map(([id, label]) => h("button", {
        class: "tab", role: "tab", type: "button", text: label,
        "aria-selected": String(tab === id),
        onclick: () => { tab = id; lastToken = null; render(); },
      }))),
    h("main", {}, content)));
}

async function signOut() {
  try { await api("POST", "logout"); } catch { /* already gone */ }
  me = null;
  lastToken = null;
  await start();
}

function confirmButton(label, sureLabel, action, cls) {
  const b = h("button", { class: "btn " + (cls || "danger") + " small", type: "button", text: label });
  b.addEventListener("click", () => {
    if (b.dataset.armed) return;
    b.dataset.armed = "1";
    const yes = h("button", { class: "btn sure small", type: "button", text: sureLabel, onclick: async () => {
      yes.disabled = true;
      try { await action(); } catch (e) { toast(e.message); yes.disabled = false; }
    } });
    const no = h("button", { class: "btn quiet small", type: "button", text: "Cancel", onclick: () => {
      yes.remove(); no.remove(); b.hidden = false; delete b.dataset.armed; b.focus();
    } });
    b.hidden = true;
    b.after(yes, no);
    yes.focus();
  });
  return b;
}

function serverUrl() {
  const u = new URL(".", location.href);
  return u.origin + u.pathname.replace(/\/$/, "");
}

async function devicesView() {
  const [list, profiles] = await Promise.all([api("GET", "devices"), api("GET", "profiles")]);
  const many = profiles.length > 1;
  const profileName = (id) => (profiles.find((p) => p.id === id) || {}).name || "";
  const rows = list.length
    ? list.map((d) => h("div", { class: "row" },
        h("div", { class: "main" },
          h("div", { class: "name", text: d.name }),
          h("div", { class: "muted small" },
            many && h("span", { class: "tag", text: profileName(d.profile_id) }), many && " ",
            d.scope === "sync" ? "Sync · " : "Read only · ",
            h("span", { class: "mono", text: d.endpoint_id || "token " + d.id.slice(0, 8) }))),
        h("div", { class: "actions" },
          confirmButton("Revoke", "Yes, revoke", async () => {
            await api("DELETE", "devices/" + encodeURIComponent(d.id));
            toast("Revoked " + d.name);
            render();
          }))))
    : [h("p", { class: "empty", text: "No devices yet. Create a token for each browser you want to sync." })];

  return h("div", { class: "stack" },
    h("div", { class: "stack-sm" },
      h("h2", { text: "Add a device" }),
      h("p", { class: "muted", text: many
        ? "Each browser gets its own sync token for one profile. Revoking it stops that browser and nothing else."
        : "Each browser gets its own sync token. Revoking it stops that browser and nothing else." }),
      form([
        h("div", { class: "form-row " + (many ? "four" : "three") },
          field("device-name", "Device name", { type: "text", placeholder: "Laptop, Firefox", required: true, maxlength: "64" }),
          many && h("div", { class: "field" },
            h("label", { for: "device-profile", text: "Profile" }),
            h("select", { id: "device-profile", name: "profile" },
              profiles.map((p) => h("option", { value: p.id, text: p.name })))),
          h("div", { class: "field" },
            h("label", { for: "device-scope", text: "Access" }),
            h("select", { id: "device-scope", name: "scope" },
              h("option", { value: "sync", text: "Sync (extension)" }),
              h("option", { value: "read", text: "Read only" }))),
          ),
      ], "Create token", async (fd, f) => {
        const r = await api("POST", "devices", {
          name: fd.get("device-name"), scope: fd.get("scope"), profile: fd.get("profile") || undefined,
        });
        lastToken = r;
        f.reset();
        render();
      }, { inline: true })),
    lastToken && tokenReveal(lastToken),
    h("div", { class: "stack-sm" },
      h("h2", { text: "Your devices" }),
      h("div", { class: "list" }, rows)));
}

async function profilesView() {
  const list = await api("GET", "profiles");
  const rows = list.map((p) => {
    const input = h("input", { type: "text", "aria-label": "New name for " + p.name, value: p.name, maxlength: "48", class: "inline-pw" });
    const rename = h("form", { class: "actions", hidden: true, onsubmit: async (e) => {
      e.preventDefault();
      try { await api("POST", "profiles/" + p.id + "/rename", { name: input.value }); toast("Renamed to " + input.value.trim()); render(); }
      catch (x) { toast(x.message); }
    } }, input, h("button", { class: "btn small", type: "submit", text: "Save" }));
    return h("div", { class: "row" },
      h("div", { class: "main" },
        h("div", { class: "name", text: p.name }),
        h("div", { class: "muted small", text: p.devices + (p.devices === 1 ? " device" : " devices") })),
      h("div", { class: "actions" },
        rename,
        h("button", { class: "btn quiet small", type: "button", text: "Rename", onclick: () => { rename.hidden = !rename.hidden; if (!rename.hidden) input.select(); } }),
        list.length > 1 && confirmButton("Remove", "Yes, delete bookmarks and devices", async () => {
          await api("DELETE", "profiles/" + p.id); toast("Removed " + p.name); render();
        })));
  });
  return h("div", { class: "stack" },
    h("div", { class: "stack-sm" },
      h("h2", { text: "Add a profile" }),
      h("p", { class: "muted", text: "A profile is a separate set of bookmarks, for example Private and Work. Profiles share nothing. A device syncs exactly one." }),
      form([
        h("div", { class: "form-row profile" },
          field("profile-name", "Profile name", { type: "text", placeholder: "Work", required: true, maxlength: "48" })),
      ], "Add profile", async (fd, f) => {
        await api("POST", "profiles", { name: fd.get("profile-name") });
        toast("Added " + fd.get("profile-name").trim());
        render();
      }, { inline: true })),
    h("div", { class: "stack-sm" },
      h("h2", { text: "Your profiles" }),
      h("div", { class: "list" }, rows)));
}

function tokenReveal(r) {
  const code = h("code", { id: "new-token", text: r.token });
  const copy = h("button", { class: "btn small", type: "button", text: "Copy token", onclick: async () => {
    try { await navigator.clipboard.writeText(r.token); copy.textContent = "Copied"; }
    catch { const sel = getSelection(); const rg = document.createRange(); rg.selectNodeContents(code); sel.removeAllRanges(); sel.addRange(rg); copy.textContent = "Selected, press Ctrl+C"; }
  } });
  return h("section", { class: "reveal", "aria-labelledby": "reveal-h" },
    h("h2", { id: "reveal-h", text: "Token for " + r.device.name }),
    h("p", { text: "Copy it now. It is shown once and cannot be recovered." }),
    code, copy,
    h("dl", {}, h("dt", { text: "Server URL" }), h("dd", { class: "mono", text: serverUrl() }),
      h("dt", { text: "Access" }), h("dd", { text: r.device.scope === "sync" ? "Sync" : "Read only" })));
}

async function usersView() {
  const list = await api("GET", "users");
  const rows = list.map((u) => {
    const self = u.id === me.id;
    const pw = h("input", { type: "password", "aria-label": "New password for " + u.name, autocomplete: "new-password", placeholder: "New password", class: "inline-pw" });
    const reset = h("form", { class: "actions", hidden: true, onsubmit: async (e) => {
      e.preventDefault();
      try { await api("POST", "users/" + u.id + "/password", { password: pw.value }); toast("Password changed for " + u.name); render(); }
      catch (x) { toast(x.message); }
    } }, pw, h("button", { class: "btn small", type: "submit", text: "Save" }));
    return h("div", { class: "row" },
      h("div", { class: "main" },
        h("div", { class: "name" }, u.name, self && h("span", { class: "muted", text: " (you)" })),
        h("div", { class: "small" },
          h("span", { class: "tag", text: u.admin ? "Admin" : "User" }), " ",
          !u.has_password && h("span", { class: "tag warn", text: "No password, cannot sign in" }), " ",
          h("span", { class: "muted", text: u.devices + (u.devices === 1 ? " device" : " devices") }))),
      h("div", { class: "actions" },
        reset,
        h("button", { class: "btn quiet small", type: "button", text: "Set password", onclick: (e) => { reset.hidden = !reset.hidden; if (!reset.hidden) pw.focus(); } }),
        !self && h("button", { class: "btn quiet small", type: "button", text: u.admin ? "Remove admin" : "Make admin", onclick: async () => {
          try { await api("POST", "users/" + u.id + "/admin", { admin: !u.admin }); render(); } catch (x) { toast(x.message); }
        } }),
        !self && confirmButton("Remove", "Yes, delete bookmarks too", async () => {
          await api("DELETE", "users/" + u.id); toast("Removed " + u.name); render();
        })));
  });
  return h("div", { class: "stack" },
    h("div", { class: "stack-sm" },
      h("h2", { text: "Add a user" }),
      h("p", { class: "muted", text: "Every user has their own bookmarks, devices and tokens." }),
      form([
        h("div", { class: "form-row user" },
          field("new-name", "User name", { type: "text", autocomplete: "off", required: true }),
          field("new-password", "Password", { type: "password", autocomplete: "new-password", required: true }),
          h("label", { class: "check" }, h("input", { type: "checkbox", id: "new-admin", name: "admin" }), "Admin")),
        h("p", { class: "hint", text: "Password: at least 10 characters." }),
      ], "Add user", async (fd) => {
        await api("POST", "users", { name: fd.get("new-name"), password: fd.get("new-password"), admin: fd.get("admin") === "on" });
        toast("Added " + fd.get("new-name"));
        render();
      }, { inline: true })),
    h("div", { class: "stack-sm" },
      h("h2", { text: "Users" }),
      h("div", { class: "list" }, rows)));
}

function accountView() {
  return h("div", { class: "stack" },
    h("div", { class: "stack-sm" },
      h("h2", { text: "Change password" }),
      h("p", { class: "muted", text: "Other browsers you are signed in on will be signed out." }),
      form([
        h("div", { class: "stack-sm narrow" },
          field("current", "Current password", { type: "password", autocomplete: "current-password", required: true }),
          field("new", "New password", { type: "password", autocomplete: "new-password", required: true }, "At least 10 characters.")),
      ], "Change password", async (fd, f) => {
        await api("POST", "password", { current: fd.get("current"), new: fd.get("new") });
        f.reset();
        toast("Password changed");
      })));
}

async function render() {
  const views = { devices: devicesView, profiles: profilesView, users: usersView, account: async () => accountView() };
  if (tab === "users" && !me.admin) tab = "devices";
  document.title = { devices: "Devices", profiles: "Profiles", users: "Users", account: "Account" }[tab] + " · bookmoor";
  let content;
  try { content = await views[tab](); }
  catch (e) { content = h("p", { class: "error", role: "alert", text: e.message }); }
  shell(content);
}

async function start() {
  const s = await api("GET", "session").catch(() => ({ state: "login" }));
  if (s.state === "user") { me = s.user; return render(); }
  me = null;
  return s.state === "setup" ? setupView() : loginView();
}

start();
