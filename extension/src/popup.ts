import { STATUS_KEY, type SyncStatus } from "./status";

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;

function showSync(status: SyncStatus | undefined): void {
  const el = $("sync");
  el.textContent = status
    ? `${status.message} (${new Date(status.at).toLocaleString()})`
    : "No sync has run yet. Open the settings to enter the server and token.";
  el.className = status && status.outcome !== "ok" ? "bad" : "";
}

chrome.storage.onChanged.addListener((changes) => {
  const change = changes[STATUS_KEY];
  if (change) showSync(change.newValue as SyncStatus | undefined);
});
void chrome.storage.local.get(STATUS_KEY).then((stored) => showSync(stored[STATUS_KEY] as SyncStatus | undefined));

$("syncnow").addEventListener("click", async () => {
  $("sync").textContent = "Syncing…";
  $("sync").className = "";
  try {
    // The result arrives as the sync status (storage change above).
    await chrome.runtime.sendMessage({ type: "bookmoor-sync-now" });
  } catch (e) {
    $("sync").textContent = `Could not reach the background script: ${e instanceof Error ? e.message : String(e)}`;
    $("sync").className = "bad";
  }
});

$("settings").addEventListener("click", (e) => {
  e.preventDefault();
  void chrome.runtime.openOptionsPage().then(() => window.close());
});
