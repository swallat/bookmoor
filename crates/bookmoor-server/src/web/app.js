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
            : path.startsWith("login/passkey") || path.startsWith("login/mfa/passkey") ? "That passkey did not work. Try again or use your password."
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
  disabled: "This account is disabled. Ask an admin.",
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

// ---- passkeys -------------------------------------------------------------

const b64u = {
  dec: (t) => Uint8Array.from(atob(t.replace(/-/g, "+").replace(/_/g, "/")), (c) => c.charCodeAt(0)),
  enc: (buf) => btoa(String.fromCharCode(...new Uint8Array(buf))).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, ""),
};

function passkeysSupported() {
  return Boolean(window.PublicKeyCredential && navigator.credentials);
}

/** Ask the browser for a passkey; turns the server's JSON options into what the browser wants. */
async function passkeyGet(options) {
  const cred = await navigator.credentials.get({ publicKey: {
    ...options,
    challenge: b64u.dec(options.challenge),
    allowCredentials: (options.allowCredentials || []).map((c) => ({ ...c, id: b64u.dec(c.id) })),
  } });
  if (!cred) throw new Error("No passkey was chosen.");
  return {
    id: b64u.enc(cred.rawId),
    client_data_json: b64u.enc(cred.response.clientDataJSON),
    authenticator_data: b64u.enc(cred.response.authenticatorData),
    signature: b64u.enc(cred.response.signature),
  };
}

async function passkeyCreate(options) {
  const cred = await navigator.credentials.create({ publicKey: {
    ...options,
    challenge: b64u.dec(options.challenge),
    user: { ...options.user, id: b64u.dec(options.user.id) },
    excludeCredentials: (options.excludeCredentials || []).map((c) => ({ ...c, id: b64u.dec(c.id) })),
  } });
  if (!cred) throw new Error("No passkey was created.");
  return {
    client_data_json: b64u.enc(cred.response.clientDataJSON),
    attestation_object: b64u.enc(cred.response.attestationObject),
  };
}

/** Browser errors are cryptic; a cancelled prompt is not a failure worth shouting about. */
function passkeyError(e) {
  if (e && e.name === "NotAllowedError") return new Error("The passkey prompt was cancelled or timed out.");
  if (e && e.name === "InvalidStateError") return new Error("That passkey is already registered.");
  return e;
}

function loginView() {
  document.title = "Sign in · bookmoor";
  centered("Sign in to bookmoor", null, [
    sso && h("p", {}, h("a", { class: "btn", href: ssoHref(false), text: "Sign in with " + sso.label })),
    passkeysSupported() && h("p", {}, h("button", { class: "btn", type: "button", text: "Sign in with a passkey", onclick: async (e) => {
      e.currentTarget.disabled = true;
      try {
        const begin = await api("POST", "login/passkey/begin");
        const answer = await passkeyGet(begin.options);
        me = (await api("POST", "login/passkey/finish", { ceremony: begin.ceremony, ...answer })).user;
        await start();
      } catch (x) { toast(passkeyError(x).message); e.currentTarget.disabled = false; }
    } })),
    (sso || passkeysSupported()) && h("p", { class: "muted or", text: "or with a password" }),
    form([
      h("div", { class: "stack-sm" },
        field("name", "User name", { type: "text", autocomplete: "username", required: true, autofocus: true }),
        field("password", "Password", { type: "password", autocomplete: "current-password", required: true })),
    ], "Sign in", async (fd) => {
      const r = await api("POST", "login", { name: fd.get("name"), password: fd.get("password") });
      if (r.state === "mfa") return mfaLoginView(r);
      me = r.user;
      await start();
    }),
  ]);
}

/** Second step of a password login: a passkey, or a code from the authenticator app (or a recovery code). */
function mfaLoginView(r) {
  const ticket = r.ticket;
  const usePasskey = r.passkey && passkeysSupported();
  document.title = "Second step · bookmoor";
  const passkeyBtn = usePasskey && h("button", { class: "btn", type: "button", text: "Use a passkey", onclick: async (e) => {
    e.currentTarget.disabled = true;
    try {
      const begin = await api("POST", "login/mfa/passkey/begin", { ticket });
      const answer = await passkeyGet(begin.options);
      me = (await api("POST", "login/mfa/passkey/finish", { ticket, ceremony: begin.ceremony, ...answer })).user;
      await start();
    } catch (x) { toast(passkeyError(x).message); e.currentTarget.disabled = false; }
  } });
  const codeForm = r.totp && form([
    h("div", { class: "stack-sm" },
      field("code", "Code", { type: "text", inputmode: "text", autocomplete: "one-time-code", required: true, autofocus: !usePasskey, maxlength: "16", spellcheck: "false" })),
  ], "Sign in", async (fd) => {
    me = (await api("POST", "login/mfa", { ticket, code: fd.get("code") })).user;
    await start();
  });
  const text = r.totp
    ? "Open your authenticator app and type the 6-digit code for bookmoor. Lost your phone? Use one of your recovery codes." + (usePasskey ? " Or use a passkey." : "")
    : "Confirm it is you with one of your passkeys.";
  centered(usePasskey && !r.totp ? "Use your passkey" : "Enter your code", text, [
    passkeyBtn && h("p", {}, passkeyBtn),
    passkeyBtn && codeForm && h("p", { class: "muted or", text: "or with a code" }),
    codeForm,
    !usePasskey && !r.totp && h("p", { class: "error", role: "alert", text: "This browser cannot use passkeys. Sign in on a device that can, or ask an admin to reset them." }),
    h("p", { class: "start-over" }, h("button", { class: "btn quiet small", type: "button", text: "Start over", onclick: () => start() }))]);
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
    : [["devices", "Devices"], ["profiles", "Profiles"], ["bookmarks", "Bookmarks"], ...(me.admin ? [["users", "Users"], ["sso", "Single sign-on"], ["dynacat", "Dynacat"]] : []), ["account", "Account"]];
  app.replaceChildren(h("div", { class: "shell" + (tab === "bookmarks" ? " wide" : "") },
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
    return h("div", { class: "row stacked" },
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
      try { await api("POST", "users/" + u.id + "/password", { password: (document.getElementById("mfa-start-pw") || {}).value || "" }); toast("Password changed for " + u.name); render(); }
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
          u.passkeys > 0 && h("span", { class: "tag", text: u.passkeys + (u.passkeys === 1 ? " passkey" : " passkeys") }), u.passkeys > 0 && " ",
          u.mfa_required && !u.mfa && !u.passkeys && h("span", { class: "tag warn", text: "Two-factor required, not set up" }), u.mfa_required && !u.mfa && !u.passkeys && " ",
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
        u.passkeys > 0 && !self && confirmButton("Reset passkeys", "Yes, delete passkeys of " + u.name, async () => {
          await api("POST", "users/" + u.id + "/passkeys/reset"); toast("Passkeys reset for " + u.name); render();
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

const callbackFor = (url) => url.trim().replace(/\/+$/, "") + "/ui/api/oidc/callback";

/** Admins: set up the OpenID Connect provider without touching the server's environment. */
async function ssoAdminView() {
  const cur = await api("GET", "sso");
  const c = cur.config || {};
  if (cur.source === "environment") {
    return h("div", { class: "stack" },
      h("div", { class: "stack-sm" },
        h("h2", { text: "Single sign-on" }),
        h("p", { class: "muted", text: "Set by the server's environment variables or flags (BOOKMOOR_OIDC_*). Those win over this page, so remove them to manage single sign-on here." }),
        h("dl", {},
          h("dt", { text: "Issuer" }), h("dd", { class: "mono", text: c.issuer }),
          h("dt", { text: "Client ID" }), h("dd", { class: "mono", text: c.client_id }),
          h("dt", { text: "Button text" }), h("dd", { text: c.label }),
          h("dt", { text: "Redirect URL" }), h("dd", { class: "mono", text: cur.callback_url }),
          h("dt", { text: "Create users" }), h("dd", { text: c.create_users ? "Yes" : "No" }))));
  }
  const url = h("input", { id: "sso-callback", class: "mono", type: "text", readonly: true, "aria-label": "Redirect URL" });
  const showUrl = () => {
    const v = document.getElementById("sso-public_url");
    url.value = v && v.value.trim() ? callbackFor(v.value) : "";
  };
  const read = () => {
    const v = (id) => document.getElementById("sso-" + id).value;
    return {
      issuer: v("issuer").trim(), client_id: v("client_id").trim(), public_url: v("public_url").trim(),
      label: v("label").trim(), create_users: document.getElementById("sso-create_users").checked,
      client_secret: v("client_secret") || undefined,
      clear_secret: Boolean(document.getElementById("sso-clear_secret")?.checked),
    };
  };
  const result = h("p", { class: "hint", role: "status" });
  const test = h("button", { class: "btn quiet gap-r", type: "button", text: "Test connection", onclick: async (e) => {
    e.target.disabled = true;
    result.className = "hint";
    result.textContent = "Contacting the provider…";
    try {
      await api("POST", "sso/test", read());
      result.textContent = "The provider answered. These settings can reach it.";
    } catch (x) { result.className = "error"; result.textContent = x.message; }
    finally { e.target.disabled = false; }
  } });
  const secret = () => document.getElementById("sso-client_secret");
  const clearBox = () => document.getElementById("sso-clear_secret");
  const secretHint = c.has_secret ? "A secret is saved. Leave empty to keep it." : "Leave empty for a public client.";
  const f = form([
    h("div", { class: "stack-sm" },
      field("sso-issuer", "Issuer URL", { type: "url", value: c.issuer || "", required: true, autocomplete: "off", placeholder: "https://id.example.com/application/o/bookmoor/" },
        "The provider's address; bookmoor finds the rest at /.well-known/openid-configuration."),
      field("sso-client_id", "Client ID", { type: "text", value: c.client_id || "", required: true, autocomplete: "off" }),
      field("sso-client_secret", "Client secret", { type: "password", autocomplete: "new-password", placeholder: c.has_secret ? "••••••••" : "",
        oninput: () => { if (clearBox() && secret().value) clearBox().checked = false; } }, secretHint),
      c.has_secret && h("label", { class: "check" }, h("input", { type: "checkbox", id: "sso-clear_secret",
        onchange: (e) => { if (e.target.checked) secret().value = ""; } }), "Remove the saved secret"),
      field("sso-public_url", "Public address of bookmoor", { type: "url", value: c.public_url || location.origin, required: true, autocomplete: "off", oninput: showUrl },
        "Where people reach this web UI, as the provider's browser sees it."),
      field("sso-label", "Button text", { type: "text", value: c.label && cur.source === "settings" ? c.label : "", maxlength: "40", autocomplete: "off", placeholder: "single sign-on" },
        "Shown as “Sign in with …”."),
      h("label", { class: "check" }, h("input", { type: "checkbox", id: "sso-create_users", checked: Boolean(c.create_users) }),
        "Create a user when someone new signs in"),
      h("p", { class: "hint", text: "Anyone who can sign in at the provider then gets a user. Restrict the application there." })),
  ], cur.source === "settings" ? "Save changes" : "Turn on single sign-on", async () => {
    await api("PUT", "sso", read());
    sso = (await api("GET", "session")).sso || null;
    toast("Single sign-on saved");
    render();
  });
  f.querySelector("button[type=submit]").before(test);
  f.append(result);
  setTimeout(showUrl);
  return h("div", { class: "stack" },
    h("div", { class: "stack-sm" },
      h("h2", { text: "Single sign-on" }),
      h("p", { class: "muted", text: cur.source === "settings"
        ? "On. Changes apply at once, no restart needed."
        : "Let people sign in with an OpenID Connect provider (Authentik, Keycloak, Authelia, Pocket ID, Google, …). Takes effect at once." }),
      h("div", { class: "field" },
        h("label", { for: "sso-callback", text: "Redirect URL to register with the provider" }),
        h("div", { class: "copy-row" }, url,
          h("button", { class: "btn quiet small", type: "button", text: "Copy", onclick: async (e) => {
            try { await navigator.clipboard.writeText(url.value); e.target.textContent = "Copied"; setTimeout(() => (e.target.textContent = "Copy"), 2000); }
            catch { url.select(); }
          } }))),
      f,
      cur.source === "settings" && confirmButton("Turn off single sign-on", "Yes, turn off", async () => {
        await api("DELETE", "sso");
        sso = null;
        toast("Single sign-on is off");
        render();
      }),
      cur.broken && h("p", { class: "error", role: "alert", text: "The saved settings are not valid, so single sign-on is off. Fix them below and save (the server log says what is wrong)." }),
      cur.source === "settings" && h("p", { class: "hint", text: "People who only have a linked provider account and no password cannot sign in while it is off." })));
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
    ? h("p", { class: "notice", role: "status", text: "An admin requires two-factor authentication for your account. Add a passkey or set up an authenticator app to continue." })
    : null;
  if (!m.enabled) {
    if (!mfaSetup) {
      return h("div", { class: "stack-sm" },
        h("h2", { text: "Two-factor authentication" }), intro,
        h("p", { class: "muted", text: "Add a second step to the password login with an authenticator app. Signing in with single sign-on is not affected." }),
        me.has_password && h("div", { class: "stack-sm narrow" }, field("mfa-start-pw", "Your password", { type: "password", autocomplete: "current-password" }, "To make sure it is you.")),
        h("button", { class: "btn", type: "button", text: "Set up", onclick: async () => {
          try { mfaSetup = await api("POST", "mfa/begin", { password: (document.getElementById("mfa-start-pw") || {}).value || "" }); render(); } catch (e) { toast(e.message); }
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

function dynStyleText(st) {
  if (!st) return "";
  const bits = [];
  if (st.title) bits.push("title “" + st.title + "”");
  if (st.names === "name") bits.push("short group names");
  if (st.depth != null) bits.push("depth " + st.depth);
  if (st.max_title) bits.push("titles ≤ " + st.max_title);
  if (st.icons) bits.push("icons");
  if (st.hide_arrow) bits.push("no arrow");
  if (st.same_tab) bits.push("same tab");
  return bits.length ? " · " + bits.join(", ") : "";
}

let dynSeq = 0;

/** The form for one Dynacat output; `init` (a row) preselects its values when editing. */
function outputForm(info, init, label, save) {
  const k = "dyn" + (++dynSeq) + "-";
  const v = init || {};
  const st = v.style || {};
  const folder = h("select", { id: k + "folder", name: "folder" }, h("option", { value: "", text: "Whole tree" }));
  const profile = h("select", { id: k + "profile", name: "profile" },
    info.profiles.map((p) => h("option", { value: p.id, text: p.label })));
  if (v.profile_id) profile.value = v.profile_id;
  const load = async (want) => {
    folder.replaceChildren(h("option", { value: "", text: "Whole tree" }));
    try {
      const paths = await api("GET", "dynacat/folders?profile=" + encodeURIComponent(profile.value));
      for (const p of paths) folder.append(h("option", { value: p, text: p }));
      if (want) folder.value = want;
    } catch (e) { toast(e.message); }
  };
  // The widget title starts as the profile's name until the admin types
  // something of their own.
  const titleField = field(k + "title", "Widget title", { type: "text", placeholder: "e.g. Work", maxlength: "80" }, "Shown above the bookmarks. Starts as the profile name so profiles can be told apart; change or clear it.");
  const titleInput = titleField.querySelector("input");
  let titleEdited = !!init;
  titleInput.addEventListener("input", () => { titleEdited = true; });
  const prefill = () => {
    if (titleEdited) return;
    const p = info.profiles.find((x) => x.id === profile.value);
    titleInput.value = p ? p.name.slice(0, 80) : "";
  };
  if (init) titleInput.value = st.title || "";
  profile.addEventListener("change", () => { load(); prefill(); });
  load(v.folder);
  prefill();
  const fileField = field(k + "file", "File name", { type: "text", placeholder: "dev.yml", required: true, maxlength: "64", autocapitalize: "off" });
  if (init) fileField.querySelector("input").value = v.file;
  const names = h("select", { id: k + "names", name: "names" },
    h("option", { value: "path", text: "Full path (toolbar / Dev)" }),
    h("option", { value: "name", text: "Folder name only (Dev)" }));
  names.value = st.names || "path";
  const depth = field(k + "depth", "Group depth", { type: "number", min: "0", max: "32", placeholder: "no limit" }, "Folders deeper than this are merged into their parent group: fewer, longer groups.");
  if (st.depth != null) depth.querySelector("input").value = st.depth;
  const max = field(k + "max", "Shorten titles to", { type: "number", min: "8", max: "500", placeholder: "no limit" }, "Characters; longer link titles end in …");
  if (st.max_title) max.querySelector("input").value = st.max_title;
  const icons = h("input", { type: "checkbox", id: k + "icons", "aria-describedby": k + "icons-hint" });
  const arrow = h("input", { type: "checkbox", id: k + "arrow" });
  const tab = h("input", { type: "checkbox", id: k + "tab" });
  icons.checked = !!st.icons; arrow.checked = !!st.hide_arrow; tab.checked = !!st.same_tab;
  return form([
    h("div", { class: "form-row fields" },
      fileField,
      h("div", { class: "field" }, h("label", { for: k + "profile", text: "Profile" }), profile),
      h("div", { class: "field" }, h("label", { for: k + "folder", text: "Folder" }), folder)),
    h("div", { class: "form-row fields" },
      titleField,
      h("div", { class: "field" }, h("label", { for: k + "names", text: "Group titles" }), names),
      depth),
    h("div", { class: "form-row fields" },
      max,
      h("div", { class: "field-check" },
        h("label", { class: "check" }, icons, h("span", { text: "Site icons" })),
        h("p", { class: "hint", id: k + "icons-hint", text: "Dynacat's browser asks each site for /favicon.ico, so those sites see the viewer's IP address." })),
      h("div", { class: "stack-sm" },
        h("label", { class: "check" }, arrow, h("span", { text: " Hide the arrow" })),
        h("label", { class: "check" }, tab, h("span", { text: " Open in the same tab" })))),
  ], label, async (fd, f) => {
    const num = (el, what) => {
      const val = el.querySelector("input").value.trim();
      if (val === "") return null;
      if (!/^\d+$/.test(val)) throw new Error(what + " must be a whole number");
      return Number(val);
    };
    const style = {
      title: titleInput.value.trim() || null,
      names: names.value,
      depth: num(depth, "Group depth"),
      max_title: num(max, "Shorten titles to"),
      icons: icons.checked,
      hide_arrow: arrow.checked,
      same_tab: tab.checked,
    };
    await save({ file: fileField.querySelector("input").value, profile: profile.value, folder: folder.value, style }, f);
  });
}

/** The form for the collecting page (shown when the page is not fixed on the command line). */
function pageForm(info, onDone) {
  const pg = info.page && info.page.editable ? info.page : null;
  const k = "dynpage-";
  const file = field(k + "file", "File name", { type: "text", placeholder: "bookmoor-page.yml", required: true, maxlength: "64", autocapitalize: "off" }, "Written into " + info.dir + ". Include it once in dynacat.yml.");
  const name = field(k + "name", "Page name", { type: "text", maxlength: "80", value: pg ? pg.name : "Bookmoor" }, "Shown in Dynacat's navigation (not used for one page per profile).");
  const columns = h("select", { id: k + "columns", name: "columns" }, [1, 2, 3].map((n) => h("option", { value: String(n), text: String(n) })));
  const source = h("select", { id: k + "source", name: "source" },
    h("option", { value: "outputs", text: "One page with a widget per output" }),
    h("option", { value: "profiles", text: "One page per profile" }));
  const icons = h("select", { id: k + "icons", name: "icons" },
    h("option", { value: "known", text: "Well-known services only" }),
    h("option", { value: "none", text: "None" }),
    h("option", { value: "google", text: "Also Google favicons (Google sees the sites)" }),
    h("option", { value: "site", text: "Also /favicon.ico (viewers contact each site)" }));
  file.querySelector("input").value = pg ? pg.file : "bookmoor-page.yml";
  columns.value = String(pg ? pg.columns : 2);
  source.value = pg ? pg.source : "outputs";
  icons.value = pg ? pg.icons : "known";
  return form([
    h("div", { class: "form-row fields" }, file, name,
      h("div", { class: "field" }, h("label", { for: k + "columns", text: "Columns" }), columns)),
    h("div", { class: "form-row fields" },
      h("div", { class: "field" }, h("label", { for: k + "source", text: "Content" }), source),
      h("div", { class: "field" }, h("label", { for: k + "icons", text: "Link icons (profile pages)" }), icons)),
  ], pg ? "Save page" : "Set up page", async () => {
    await api("PUT", "dynacat/page", {
      file: file.querySelector("input").value,
      name: name.querySelector("input").value,
      columns: Number(columns.value),
      source: source.value,
      icons: icons.value,
    });
    toast("Page saved");
    onDone();
  });
}

async function dynacatView() {
  const info = await api("GET", "dynacat");
  const rows = info.outputs.map((o) => {
    let editor = null;
    const row = h("div", { class: "row" },
      h("div", { class: "main" },
        h("div", { class: "name mono", text: o.path || o.file }),
        h("div", { class: "muted small", text: o.profile + " · " + (o.folder ? o.folder : "whole tree") + dynStyleText(o.style) + (o.written ? " · file last changed " + new Date(o.written * 1000).toLocaleString() : o.error ? "" : " · not written yet") }),
        o.error && h("div", { class: "error small", role: "alert", text: o.error })),
      h("div", { class: "actions" },
        o.id
          ? [h("button", { class: "btn quiet small", type: "button", text: "Edit", onclick: () => {
              if (editor) { editor.remove(); editor = null; return; }
              editor = outputForm(info, o, "Save output", async (body) => {
                await api("PUT", "dynacat/" + encodeURIComponent(o.id), body);
                toast("Saved " + body.file + (body.file !== o.file ? ". The old file stays on disk." : ""));
                render();
              });
              row.after(editor);
            } }),
            confirmButton("Remove", "Yes, remove", async () => {
              await api("DELETE", "dynacat/" + encodeURIComponent(o.id));
              toast("Removed " + o.file + ". The file itself stays on disk.");
              render();
            })]
          : h("span", { class: "tag", text: "Command line" })));
    return row;
  });

  // Without a directory the form cannot work, so say how to switch it on. A
  // command-line output already shows which directory Dynacat reads.
  const cli = info.outputs.find((o) => !o.id);
  let guess = cli ? cli.file.replace(/[\\/][^\\/]*$/, "") : "";
  if (!guess || guess === cli.file) guess = "/dynacat";
  let create = h("div", { class: "notice stack-sm", role: "note" },
    h("p", { text: "Adding and changing outputs here is switched off: the server does not know which directory Dynacat reads." }),
    h("p", { class: "small" }, "Set ", h("code", { text: "BOOKMOOR_DYNACAT_DIR=" + guess }),
      " (Docker: under environment; otherwise ", h("code", { text: "--dynacat-dir " + guess }),
      "), make that directory writable by the server and restart it. ",
      cli ? "Outputs from the command line, like the one below, stay read-only: change them where the server is started." : ""));
  if (info.dir) {
    create = outputForm(info, null, "Add output", async (body, f) => {
      await api("POST", "dynacat", body);
      toast("Added " + body.file);
      f.reset();
      render();
    });
  }
  const pg = info.page;
  const pageText = (p) => (p.source === "profiles" ? "One page per profile" : "Page \"" + p.name + "\"") + " · " + p.columns + (p.columns === 1 ? " column" : " columns") + (p.written ? " · file last changed " + new Date(p.written * 1000).toLocaleString() : p.error ? "" : " · not written yet");
  let pageEditor = null;
  const pageRow = (pg || info.dir) && h("div", { class: "stack-sm" },
    h("h2", { text: "Collecting page" }),
    h("p", { class: "muted", text: "One Dynacat file, so a single $include is enough and new outputs or profiles show up by themselves: either one page with a widget for every output below, or one page per profile. It is rewritten after every sync when its content changes." }),
    pg && h("div", { class: "list" }, h("div", { class: "row" },
      h("div", { class: "main" },
        h("div", { class: "name mono", text: pg.path }),
        h("div", { class: "muted small", text: pageText(pg) }),
        pg.error && h("div", { class: "error small", role: "alert", text: pg.error })),
      h("div", { class: "actions" }, pg.editable
        ? [h("button", { class: "btn quiet small", type: "button", text: "Edit", onclick: (e) => {
            const row = e.target.closest(".row");
            if (pageEditor) { pageEditor.remove(); pageEditor = null; return; }
            pageEditor = pageForm(info, render);
            row.after(pageEditor);
          } }),
          confirmButton("Remove", "Yes, remove", async () => {
            await api("DELETE", "dynacat/page");
            toast("Page removed. The file itself stays on disk.");
            render();
          })]
        : h("span", { class: "tag", text: "Command line" })))),
    !pg && info.dir && pageForm(info, render),
    pg && !pg.editable && h("p", { class: "hint", text: "This page is set with BOOKMOOR_DYNACAT_PAGE (or --dynacat-page); change it where the server is started." }));
  return h("div", { class: "stack" },
    h("div", { class: "stack-sm" },
      h("h2", { text: "Dynacat outputs" }),
      h("p", { class: "muted", text: "Each output is a file with a Dynacat bookmarks widget, kept up to date after every sync. Include it in a Dynacat page with $include. Choose a profile and, if you like, one folder of it. Several outputs of the same profile (each with its own folder) make several pages; give each a widget title." }),
      info.dir && h("p", { class: "hint", text: "Each output is written into " + info.dir + " when you add it and checked after every sync of its profile; it is only rewritten when its content changes." }),
      create),
    pageRow,
    h("div", { class: "stack-sm" },
      h("h2", { text: "Outputs" }),
      h("div", { class: "list" }, rows.length ? rows : [h("p", { class: "empty", text: "No outputs yet." })])));
}


let passkeyList = null; // fetched on every visit to the Account tab

function when(secs) {
  return secs ? new Date(secs * 1000).toLocaleDateString(undefined, { year: "numeric", month: "short", day: "numeric" }) : "never";
}

async function passkeysSection() {
  passkeyList = await api("GET", "passkeys");
  me.passkeys = passkeyList.length;
  const refresh = async (list) => {
    passkeyList = list; me.passkeys = list.length;
    // Having a passkey can satisfy a required second factor.
    me = (await api("GET", "session")).user;
    render();
  };
  const rows = passkeyList.map((p) => {
    const rename = h("form", { class: "actions", hidden: true, onsubmit: async (e) => {
      e.preventDefault();
      try { await refresh(await api("POST", "passkeys/" + p.id + "/rename", { name: nm.value })); toast("Renamed"); }
      catch (x) { toast(x.message); }
    } });
    const nm = h("input", { type: "text", value: p.name, maxlength: "40", "aria-label": "Passkey name", class: "inline-pw" });
    rename.append(nm, h("button", { class: "btn small", type: "submit", text: "Save" }));
    return h("div", { class: "row" },
      h("div", { class: "main" },
        h("div", { class: "name", text: p.name }),
        h("div", { class: "muted small", text: "Added " + when(p.created) + " · last used " + when(p.last_used) })),
      h("div", { class: "actions" },
        rename,
        h("button", { class: "btn quiet small", type: "button", text: "Rename", onclick: () => { rename.hidden = !rename.hidden; if (!rename.hidden) nm.select(); } }),
        confirmButton("Delete", "Yes, delete it", async () => {
          await refresh(await api("DELETE", "passkeys/" + p.id));
          toast("Passkey deleted");
        })));
  });
  const supported = passkeysSupported();
  return h("div", { class: "stack-sm" },
    h("h2", { text: "Passkeys" }),
    h("p", { class: "muted", text: "Sign in with your fingerprint, face, PIN or a security key instead of a password. A passkey also works as the second step after a password. Single sign-on is not affected." }),
    h("div", { class: "list" }, rows.length ? rows : [h("p", { class: "empty", text: "No passkeys yet." })]),
    !supported && h("p", { class: "hint", text: "This browser cannot create passkeys." }),
    supported && form([
      h("div", { class: "stack-sm narrow" },
        field("pk-name", "Name", { type: "text", maxlength: "40", placeholder: "Laptop, phone, security key" }, "So you can tell them apart."),
        me.has_password && field("pk-pw", "Your password", { type: "password", autocomplete: "current-password" }, "To make sure it is you.")),
    ], "Add a passkey", async (fd, f) => {
      let list;
      try {
        const begin = await api("POST", "passkeys/begin", { password: fd.get("pk-pw") || "" });
        const answer = await passkeyCreate(begin.options);
        list = await api("POST", "passkeys/finish", { ceremony: begin.ceremony, name: fd.get("pk-name") || "", ...answer });
      } catch (x) { throw passkeyError(x); }
      f.reset();
      toast("Passkey added");
      await refresh(list);
    }));
}

async function accountView() {
  const mfaPart = await mfaSection();
  const passkeyPart = await passkeysSection();
  if (me.mfa_setup_needed) return h("div", { class: "stack" }, mfaPart, passkeyPart);
  return h("div", { class: "stack" },
    ssoSection(),
    passkeyPart,
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
  const views = { devices: devicesView, profiles: profilesView, bookmarks: bookmarksView, users: usersView, sso: ssoAdminView, dynacat: dynacatView, account: accountView };
  if ((tab === "users" || tab === "sso" || tab === "dynacat") && !me.admin) tab = "devices";
  if (me.mfa_setup_needed) tab = "account"; // nothing else works until it is set up
  document.title = { devices: "Devices", profiles: "Profiles", bookmarks: "Bookmarks", users: "Users", sso: "Single sign-on", dynacat: "Dynacat", account: "Account" }[tab] + " · bookmoor";
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
  const ver = document.getElementById("version");
  if (ver && s.version) ver.textContent = "bookmoor " + s.version + (s.commit ? " (" + s.commit + ")" : "");
  if (s.state === "user") { me = s.user; return connectAsk && !me.mfa_setup_needed ? connectView() : render(); }
  me = null;
  return s.state === "setup" ? setupView() : loginView();
}

start().then(ssoNotice);
