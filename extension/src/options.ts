import { STATUS_KEY, type SyncStatus } from "./status";
import { signInWithServer } from "./connect";
import { loadSettings, originPattern, hasHostAccess, saveSettings, type HeaderEntry } from "./settings";

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;

function pairRow(entry: HeaderEntry = { name: "", value: "" }, what = "Header"): HTMLDivElement {
  const row = document.createElement("div");
  row.className = "row";
  const name = Object.assign(document.createElement("input"), { placeholder: `${what} name`, value: entry.name });
  const value = Object.assign(document.createElement("input"), {
    type: "password",
    placeholder: `${what} value`,
    value: entry.value,
  });
  const remove = Object.assign(document.createElement("button"), { type: "button", textContent: "Remove" });
  remove.addEventListener("click", () => row.remove());
  row.append(name, value, remove);
  return row;
}

function readPairs(container: string): HeaderEntry[] {
  return [...$(container).querySelectorAll<HTMLDivElement>(".row")]
    .map((row) => {
      const [name, value] = row.querySelectorAll("input");
      return { name: name?.value ?? "", value: value?.value ?? "" };
    })
    .filter((h) => h.name.trim() !== "");
}

function showSync(status: SyncStatus | undefined): void {
  const el = $("sync");
  el.textContent = status
    ? `${status.message} (${new Date(status.at).toLocaleString()})`
    : "No sync has run yet. Save the settings and change a bookmark.";
  el.className = status && status.outcome !== "ok" ? "bad" : "";
  $("signin").hidden = status?.outcome !== "login";
}

/** What is stored, without showing the secret: lets the user see that the token arrived intact. */
function showToken(token: string): void {
  const el = $("tokeninfo");
  if (!token) {
    el.textContent = "Not signed in yet.";
    el.className = "hint";
  } else if (!token.startsWith("bkm_")) {
    el.textContent = `Stored: ${token.length} characters. Tokens normally start with "bkm_", check that you pasted the whole token.`;
    $<HTMLDetailsElement>("advanced").open = true;
    el.className = "hint bad";
  } else {
    el.textContent = `Signed in. Device token ${token.slice(0, 8)}\u2026 is stored.`;
    el.className = "hint";
  }
}

/** Ask the background script for an immediate sync; its result arrives as the sync status. */
async function syncNow(): Promise<void> {
  $("sync").textContent = "Syncing\u2026";
  $("sync").className = "";
  try {
    await chrome.runtime.sendMessage({ type: "bookmoor-sync-now" });
  } catch (e) {
    $("sync").textContent = `Could not reach the background script: ${e instanceof Error ? e.message : String(e)}`;
    $("sync").className = "bad";
  }
}

async function init(): Promise<void> {
  chrome.storage.onChanged.addListener((changes) => {
    const change = changes[STATUS_KEY];
    if (change) showSync(change.newValue as SyncStatus | undefined);
  });
  void chrome.storage.local.get(STATUS_KEY).then((stored) => showSync(stored[STATUS_KEY] as SyncStatus | undefined));
  const s = await loadSettings();
  // Do not overwrite what was typed while the settings were loading.
  const url = $<HTMLInputElement>("url");
  const token = $<HTMLInputElement>("token");
  if (!url.value) url.value = s.serverUrl;
  if (!token.value) token.value = s.token;
  showToken(s.token);
  if (s.headers.length || s.query.length) $<HTMLDetailsElement>("advanced").open = true;
  for (const h of s.headers) $("headers").append(pairRow(h));
  for (const q of s.query) $("query").append(pairRow(q, "Parameter"));
  $("add").addEventListener("click", () => $("headers").append(pairRow()));
  $("addq").addEventListener("click", () => $("query").append(pairRow(undefined, "Parameter")));
  $("save").addEventListener("click", async () => {
    const status = $("status");
    try {
      const serverUrl = $<HTMLInputElement>("url").value.trim();
      // Must run inside the click handler: permission requests need a user gesture.
      const granted = await chrome.permissions.request({ origins: [originPattern(serverUrl)] });
      // Some browsers answer yes without keeping the grant, so ask again.
      if (!granted || !(await hasHostAccess(serverUrl))) {
        throw new Error(
          `The browser did not give the extension access to ${new URL(serverUrl).host}. ` +
            "Allow it for this extension (Firefox: about:addons > bookmoor > Permissions; Chrome: Details > Site access) and save again.",
        );
      }
      await saveSettings({
        serverUrl,
        token: $<HTMLInputElement>("token").value.trim(),
        headers: readPairs("headers"),
        query: readPairs("query"),
      });
      showToken($<HTMLInputElement>("token").value.trim());
      status.textContent = "Saved.";
      void syncNow();
    } catch (e) {
      status.textContent = `Error: ${e instanceof Error ? e.message : String(e)}`;
    }
  });
}

/** Ask for confirmation, then let the background script start over. */
async function startOver(mode: "adopt" | "overwrite"): Promise<void> {
  const ok = confirm(
    mode === "adopt"
      ? "Replace this browser's synced bookmarks with the server's? Links that only exist here are removed from this browser."
      : "Replace the server's bookmarks with this browser's? Links that only exist on the server are removed from the profile and from its other devices (the server keeps a copy).",
  );
  if (!ok) return;
  $("startover").textContent = "Working\u2026";
  try {
    const r = (await chrome.runtime.sendMessage({ type: "bookmoor-start-over", mode })) as
      | { ok: boolean; message: string }
      | undefined;
    $("startover").textContent = r?.ok ? "Done." : `Not done: ${r?.message || "no answer from the background script"}`;
    $("startover").className = r?.ok ? "" : "bad";
  } catch (e) {
    $("startover").textContent = `Could not reach the background script: ${e instanceof Error ? e.message : String(e)}`;
  }
}

$("signinserver").addEventListener("click", async () => {
  const status = $("status");
  try {
    const serverUrl = $<HTMLInputElement>("url").value.trim();
    if (!serverUrl) throw new Error("Enter the server URL first.");
    // Must run inside the click handler: permission requests need a user gesture.
    if (!(await chrome.permissions.request({ origins: [originPattern(serverUrl)] }))) {
      throw new Error(`The browser did not give the extension access to ${new URL(serverUrl).host}.`);
    }
    const token = await signInWithServer(serverUrl);
    $<HTMLInputElement>("token").value = token;
    showToken(token);
    status.textContent = "Signed in. Saving\u2026";
    $("save").click();
  } catch (e) {
    status.textContent = `Error: ${e instanceof Error ? e.message : String(e)}`;
  }
});
$("syncnow").addEventListener("click", () => void syncNow());
$("adopt").addEventListener("click", () => void startOver("adopt"));
$("overwrite").addEventListener("click", () => void startOver("overwrite"));
$("signin").addEventListener("click", () => void chrome.runtime.sendMessage({ type: "bookmoor-sign-in" }));

void init();
