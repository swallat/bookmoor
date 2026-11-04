import { STATUS_KEY, type SyncStatus } from "./status";
import { loadSettings, originPattern, saveSettings, type HeaderEntry } from "./settings";

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
}

async function init(): Promise<void> {
  const stored = await chrome.storage.local.get(STATUS_KEY);
  showSync(stored[STATUS_KEY] as SyncStatus | undefined);
  chrome.storage.onChanged.addListener((changes) => {
    const change = changes[STATUS_KEY];
    if (change) showSync(change.newValue as SyncStatus | undefined);
  });
  const s = await loadSettings();
  $<HTMLInputElement>("url").value = s.serverUrl;
  $<HTMLInputElement>("token").value = s.token;
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
      if (!granted) throw new Error("Permission for the server origin was denied.");
      await saveSettings({
        serverUrl,
        token: $<HTMLInputElement>("token").value,
        headers: readPairs("headers"),
        query: readPairs("query"),
      });
      status.textContent = "Saved.";
    } catch (e) {
      status.textContent = `Error: ${e instanceof Error ? e.message : String(e)}`;
    }
  });
}

void init();
