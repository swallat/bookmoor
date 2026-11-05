"use strict";
// bookmoor admin page. Plain DOM, no framework; user data only ever reaches
// the page through textContent, never innerHTML.

const app = document.getElementById("app");
let me = null;
let sso = null; // { label } while the server offers single sign-on
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
        : res.status === 401 ? (path === "setup" ? "That admin token is not valid."
            : path === "login/mfa" ? "That code is not right, or the sign-in took too long. Try again or start over."
            : "Name or password is wrong.")
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

const SSO_ERRORS = {
  unlinked: "That account is not linked to a bookmoor user. Sign in with your password and link it under Account.",
  taken: "That account is already linked to another user.",
  denied: "The identity provider refused the sign-in.",
  expired: "The sign-in took too long or was started in another browser. Try again.",
  provider: "Could not reach the identity provider or its answer was not valid.",
  rate: "Too many attempts. Wait a moment and try again.",
  create: "Could not create your user.",
  session: "Your session ended. Sign in again.",
};

/** Shown once after coming back from the identity provider. */
function ssoNotice() {
  const q = new URLSearchParams(location.search);
  const code = q.get("sso_error");
  if (!code) return;
  q.delete("sso_error");
  history.replaceState(null, "", location.pathname + (q.toString() ? "?" + q : ""));
  toast(code === "linked" ? "Single sign-on account linked." : SSO_ERRORS[code] || "Single sign-on failed.");
}

/** Link to the identity provider; keeps the extension's sign-in request alive across it. */
function ssoHref(link) {
  const q = new URLSearchParams();
  if (link) q.set("link", "1");
  if (location.search.includes("connect=")) q.set("return", location.search);
  return "ui/api/oidc/start" + (q.toString() ? "?" + q : "");
}

function loginView() {
  document.title = "Sign in · bookmoor";
  centered("Sign in to bookmoor", null, [
    sso && h("p", {}, h("a", { class: "btn", href: ssoHref(false), text: "Sign in with " + sso.label })),
    sso && h("p", { class: "muted", text: "or with a password" }),
    form([
      h("div", { class: "stack-sm" },
        field("name", "User name", { type: "text", autocomplete: "username", required: true, autofocus: true }),
        field("password", "Password", { type: "password", autocomplete: "current-password", required: true })),
    ], "Sign in", async (fd) => {
      const r = await api("POST", "login", { name: fd.get("name"), password: fd.get("password") });
      if (r.state === "mfa") return mfaLoginView(r.ticket);
      me = r.user;
      await start();
    }),
  ]);
}

/** Second step of a password login: a code from the authenticator app, or a recovery code. */
function mfaLoginView(ticket) {
  document.title = "Two-factor code · bookmoor";
  centered("Enter your code",
    "Open your authenticator app and type the 6-digit code for bookmoor. Lost your phone? Use one of your recovery codes.",
    [form([
      h("div", { class: "stack-sm" },
        field("code", "Code", { type: "text", inputmode: "text", autocomplete: "one-time-code", required: true, autofocus: true, maxlength: "16", spellcheck: "false" })),
    ], "Sign in", async (fd) => {
      me = (await api("POST", "login/mfa", { ticket, code: fd.get("code") })).user;
      await start();
    }),
    h("p", {}, h("button", { class: "btn quiet small", type: "button", text: "Start over", onclick: () => start() }))]);
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
  const tabs = me.mfa_setup_needed ? [["account", "Account"]]
    : [["devices", "Devices"], ["profiles", "Profiles"], ...(me.admin ? [["users", "Users"]] : []), ["account", "Account"]];
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

/** "Import": pick a downloaded file; the profile's bookmarks are replaced (Restore brings the old ones back). */
function importButton(p) {
  const input = h("input", { type: "file", accept: "application/json,.json", hidden: true, "aria-label": "Bookmark file for " + p.name, onchange: async () => {
    const file = input.files[0];
    input.value = "";
    if (!file) return;
    if (!confirm("Replace the bookmarks of " + p.name + " with the file " + file.name + "? Its devices follow. You can undo this with Restore.")) return;
    try {
      await api("POST", "profiles/" + p.id + "/import", JSON.parse(await file.text()));
      toast("Imported " + file.name); render();
    } catch (x) { toast(x instanceof SyntaxError ? "That is not a JSON file" : x.message); }
  } });
  return h("span", {}, input, h("button", { class: "btn quiet small", type: "button", text: "Import", onclick: () => input.click() }));
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
        h("div", { class: "muted small", text: p.devices + (p.devices === 1 ? " device" : " devices") + ", " + p.bookmarks + (p.bookmarks === 1 ? " bookmark" : " bookmarks") })),
      h("div", { class: "actions" },
        p.bookmarks > 0 && h("button", { class: "btn quiet small", type: "button", text: "Tidy up", title: "Merge folders of the same name and drop repeated links", onclick: async () => {
          try {
            const r = await api("POST", "profiles/" + p.id + "/dedupe");
            toast(r.bookmarks_removed + r.folders_merged === 0 ? "Nothing to tidy up in " + p.name
              : "Removed " + r.bookmarks_removed + " repeated links, merged " + r.folders_merged + " folders");
            render();
          } catch (x) { toast(x.message); }
        } }),
        p.backup && confirmButton("Restore", "Yes, bring back " + p.backup.bookmarks + " bookmarks from " + p.backup.at.replace("T", " ").replace("Z", " UTC"), async () => {
          await api("POST", "profiles/" + p.id + "/restore"); toast("Restored " + p.name); render();
        }, "quiet"),
        h("button", { class: "btn quiet small", type: "button", text: "Download", title: "Save this profile's bookmarks as a file", onclick: async () => {
          try {
            const res = await fetch("ui/api/profiles/" + p.id + "/export", { credentials: "same-origin", headers: { "X-Bookmoor-UI": "1" } });
            if (!res.ok) throw new Error("Download failed (" + res.status + ")");
            const url = URL.createObjectURL(await res.blob());
            const a = h("a", { href: url, download: "bookmoor-" + p.name.replace(/[^A-Za-z0-9]+/g, "-") + ".json" });
            document.body.append(a); a.click(); a.remove(); setTimeout(() => URL.revokeObjectURL(url), 1000);
          } catch (x) { toast(x.message); }
        } }),
        importButton(p),
        p.bookmarks > 0 && confirmButton("Clear", "Yes, remove all bookmarks", async () => {
          await api("POST", "profiles/" + p.id + "/clear"); toast("Cleared " + p.name); render();
        }),
        rename,
        h("button", { class: "btn quiet small", type: "button", text: "Rename", onclick: () => { rename.hidden = !rename.hidden; if (!rename.hidden) input.select(); } }),
        list.length > 1 && confirmButton("Remove", "Yes, delete bookmarks and devices", async () => {
          await api("DELETE", "profiles/" + p.id); toast("Removed " + p.name); render();
        })));
  });
  return h("div", { class: "stack" },
    h("div", { class: "stack-sm" },
      h("h2", { text: "Add a profile" }),
      h("p", { class: "muted", text: "A profile is a separate set of bookmarks, for example Private and Work. Profiles share nothing. A device syncs exactly one. If a browser synced into the wrong profile: give it a token of the right profile first, then clear the wrong one here. Clear and Tidy up keep a copy you can bring back with Restore." }),
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
    const nm = h("input", { type: "text", "aria-label": "New name for " + u.name, value: u.name, maxlength: "32", class: "inline-pw" });
    const rename = h("form", { class: "actions", hidden: true, onsubmit: async (e) => {
      e.preventDefault();
      try { await api("POST", "users/" + u.id + "/edit", { name: nm.value }); toast("Renamed to " + nm.value.trim()); render(); }
      catch (x) { toast(x.message); }
    } }, nm, h("button", { class: "btn small", type: "submit", text: "Save" }));
    const q = (label, fn) => h("button", { class: "btn quiet small", type: "button", text: label, onclick: async () => {
      try { await fn(); render(); } catch (x) { toast(x.message); }
    } });
    return h("div", { class: "row" + (u.disabled ? " off" : "") },
      h("div", { class: "main" },
        h("div", { class: "name" }, u.name, self && h("span", { class: "muted", text: " (you)" })),
        h("div", { class: "small" },
          h("span", { class: "tag", text: u.admin ? "Admin" : "User" }), " ",
          u.disabled && h("span", { class: "tag warn", text: "Disabled" }), u.disabled && " ",
          !u.has_password && !u.oidc && h("span", { class: "tag warn", text: "No password, cannot sign in" }), !u.has_password && !u.oidc && " ",
          u.mfa && h("span", { class: "tag", text: "Two-factor on" }), u.mfa && " ",
          u.mfa_required && !u.mfa && h("span", { class: "tag warn", text: "Two-factor required, not set up" }), u.mfa_required && !u.mfa && " ",
          h("span", { class: "muted", text: u.devices + (u.devices === 1 ? " device" : " devices") }))),
      h("div", { class: "actions" },
        reset, rename,
        h("button", { class: "btn quiet small", type: "button", text: "Rename", onclick: () => { rename.hidden = !rename.hidden; if (!rename.hidden) nm.select(); } }),
        h("button", { class: "btn quiet small", type: "button", text: "Set password", onclick: () => { reset.hidden = !reset.hidden; if (!reset.hidden) pw.focus(); } }),
        q(u.mfa_required ? "Don't require two-factor" : "Require two-factor", () =>
          api("POST", "users/" + u.id + "/mfa", { required: !u.mfa_required })),
        u.mfa && !self && confirmButton("Reset two-factor", "Yes, switch it off for " + u.name, async () => {
          await api("POST", "users/" + u.id + "/mfa/reset"); toast("Two-factor reset for " + u.name); render();
        }, "quiet"),
        !self && q(u.admin ? "Remove admin" : "Make admin", () => api("POST", "users/" + u.id + "/admin", { admin: !u.admin })),
        !self && q(u.disabled ? "Enable" : "Disable", () => api("POST", "users/" + u.id + "/edit", { disabled: !u.disabled })),
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

function ssoSection() {
  if (!sso) return null;
  return h("div", { class: "stack-sm" },
    h("h2", { text: "Single sign-on" }),
    me.oidc
      ? [h("p", { class: "muted", text: "Your account is linked to " + sso.label + ". You can sign in with it." }),
         h("button", { class: "btn quiet", type: "button", text: "Unlink",
           onclick: async () => {
             try { await api("POST", "oidc/unlink"); me.oidc = false; toast("Unlinked"); render(); }
             catch (e) { toast(e.message); }
           } })]
      : [h("p", { class: "muted", text: "Link " + sso.label + " to sign in without a password." }),
         h("a", { class: "btn", href: ssoHref(true), text: "Link " + sso.label })]);
}

/** Recovery codes, shown once. */
function recoveryReveal(codes) {
  const text = codes.join("\n");
  const copy = h("button", { class: "btn small", type: "button", text: "Copy codes", onclick: async () => {
    try { await navigator.clipboard.writeText(text); copy.textContent = "Copied"; } catch { copy.textContent = "Select and copy by hand"; }
  } });
  return h("section", { class: "reveal", "aria-labelledby": "rec-h" },
    h("h2", { id: "rec-h", text: "Your recovery codes" }),
    h("p", { text: "Keep these somewhere safe. Each works once when you cannot use your app. They are shown only now." }),
    h("pre", { class: "codes mono", text: text }), copy);
}

let mfaFresh = null; // recovery codes to show once after setup or renewal
let mfaSetup = null; // { secret, uri } while an authenticator is being added

async function mfaSection() {
  const m = await api("GET", "mfa");
  me.mfa = m.enabled;
  const intro = m.required && !m.enabled
    ? h("p", { class: "notice", role: "status", text: "An admin requires two-factor authentication for your account. Set it up to continue." })
    : null;
  if (!m.enabled) {
    if (!mfaSetup) {
      return h("div", { class: "stack-sm" },
        h("h2", { text: "Two-factor authentication" }), intro,
        h("p", { class: "muted", text: "Add a second step to the password login with an authenticator app. Signing in with single sign-on is not affected." }),
        h("button", { class: "btn", type: "button", text: "Set up", onclick: async () => {
          try { mfaSetup = await api("POST", "mfa/begin"); render(); } catch (e) { toast(e.message); }
        } }));
    }
    return h("div", { class: "stack-sm" },
      h("h2", { text: "Set up two-factor authentication" }), intro,
      h("p", { class: "muted", text: "Add this key to your authenticator app (Aegis, Google Authenticator, 1Password and others), then type the 6-digit code it shows." }),
      h("p", {}, h("code", { id: "mfa-secret", class: "mono", text: mfaSetup.secret.replace(/(.{4})/g, "$1 ").trim() })),
      h("p", { class: "small" }, h("a", { href: mfaSetup.uri, text: "Open in an authenticator app on this device" })),
      form([
        h("div", { class: "stack-sm narrow" },
          field("mfa-code", "Code from the app", { type: "text", inputmode: "numeric", autocomplete: "one-time-code", required: true, maxlength: "7", spellcheck: "false" })),
      ], "Turn on", async (fd) => {
        const r = await api("POST", "mfa/enable", { code: fd.get("mfa-code") });
        mfaSetup = null; mfaFresh = r.recovery_codes; me.mfa = true; me.mfa_setup_needed = false;
        toast("Two-factor authentication is on");
        render();
      }),
      h("button", { class: "btn quiet small", type: "button", text: "Cancel", onclick: () => { mfaSetup = null; render(); } }));
  }
  const fresh = mfaFresh; mfaFresh = null;
  return h("div", { class: "stack-sm" },
    h("h2", { text: "Two-factor authentication" }),
    h("p", { class: "muted", text: "On. Signing in with your password asks for a code from your app. " + m.recovery_left + (m.recovery_left === 1 ? " recovery code is" : " recovery codes are") + " left." }),
    fresh && recoveryReveal(fresh),
    h("details", {},
      h("summary", { text: "New recovery codes or turn off" }),
      h("div", { class: "stack-sm narrow" },
        h("p", { class: "muted small", text: "Enter a code from your app (or a recovery code)." }),
        form([
          h("div", { class: "stack-sm" },
            field("mfa-code2", "Code", { type: "text", autocomplete: "one-time-code", required: true, maxlength: "16", spellcheck: "false" }),
            field("mfa-pw", "Password (to turn off)", { type: "password", autocomplete: "current-password" })),
        ], "New recovery codes", async (fd) => {
          const r = await api("POST", "mfa/recovery", { code: fd.get("mfa-code2") });
          mfaFresh = r.recovery_codes; render();
        }),
        !m.required && h("button", { class: "btn danger small", type: "button", text: "Turn off two-factor", onclick: async () => {
          const code = document.getElementById("mfa-code2").value, password = document.getElementById("mfa-pw").value;
          try { await api("POST", "mfa/disable", { code, password }); me.mfa = false; toast("Two-factor authentication is off"); render(); }
          catch (e) { toast(e.message); }
        } }),
        m.required && h("p", { class: "muted small", text: "An admin requires it, so it cannot be turned off here." }))));
}

async function accountView() {
  const mfaPart = await mfaSection();
  if (me.mfa_setup_needed) return h("div", { class: "stack" }, mfaPart);
  return h("div", { class: "stack" },
    ssoSection(),
    mfaPart,
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
  const views = { devices: devicesView, profiles: profilesView, users: usersView, account: accountView };
  if (tab === "users" && !me.admin) tab = "devices";
  if (me.mfa_setup_needed) tab = "account"; // nothing else works until it is set up
  document.title = { devices: "Devices", profiles: "Profiles", users: "Users", account: "Account" }[tab] + " · bookmoor";
  let content;
  try { content = await views[tab](); }
  catch (e) { content = h("p", { class: "error", role: "alert", text: e.message }); }
  shell(content);
}

// The extension's sign-in opens this page as /?connect=<its redirect URL>&state=...&name=...
const connectAsk = (() => {
  const q = new URLSearchParams(location.search);
  return q.has("connect") && q.has("state")
    ? { redirect: q.get("connect"), state: q.get("state"), name: q.get("name") || "Browser" }
    : null;
})();

async function connectView() {
  document.title = "Connect browser · bookmoor";
  const profiles = await api("GET", "profiles");
  const select = profiles.length > 1
    ? h("div", { class: "field" },
        h("label", { for: "profile", text: "Profile" }),
        h("select", { id: "profile", name: "profile" },
          profiles.map((p) => h("option", { value: p.id, text: p.name }))))
    : null;
  centered("Connect this browser?",
    "The bookmoor extension asks to sync as " + me.name + ". This creates a new device token; you can revoke it on the Devices tab.",
    form([
      h("div", { class: "stack-sm" },
        field("devname", "Device name", { type: "text", required: true, value: connectAsk.name, autocomplete: "off" }),
        select),
    ], "Connect", async (fd) => {
      const r = await api("POST", "connect", {
        redirect: connectAsk.redirect, state: connectAsk.state,
        name: String(fd.get("devname") || "").trim() || "Browser",
        profile: select ? fd.get("profile") : undefined,
      });
      location.href = r.url;
    }));
}

async function start() {
  const s = await api("GET", "session").catch(() => ({ state: "login" }));
  sso = s.sso || null;
  if (s.state === "user") { me = s.user; return connectAsk && !me.mfa_setup_needed ? connectView() : render(); }
  me = null;
  return s.state === "setup" ? setupView() : loginView();
}

start().then(ssoNotice);
