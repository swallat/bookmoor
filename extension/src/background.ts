import { buildHeaders, loadSettings } from "./settings";

// v0.0: push a full snapshot after the bookmarks change. v0.1 replaces this
// with deltas, the browser-id <-> server-id map and the pull direction.

const DEBOUNCE_ALARM = "bookmoor-push";

function schedulePush(): void {
  // Alarms survive service-worker restarts; setTimeout would not.
  chrome.alarms.create(DEBOUNCE_ALARM, { delayInMinutes: 0.1 });
}

async function pushSnapshot(): Promise<void> {
  const settings = await loadSettings();
  if (!settings.serverUrl) return;
  const tree = await chrome.bookmarks.getTree();
  const res = await fetch(new URL("/v1/sync", settings.serverUrl), {
    method: "POST",
    headers: buildHeaders(settings),
    body: JSON.stringify({ client: "chrome", snapshot: tree }),
  });
  if (!res.ok) console.warn(`bookmoor: push failed with HTTP ${res.status}`);
}

chrome.bookmarks.onCreated.addListener(schedulePush);
chrome.bookmarks.onChanged.addListener(schedulePush);
chrome.bookmarks.onMoved.addListener(schedulePush);
chrome.bookmarks.onRemoved.addListener(schedulePush);
chrome.alarms.onAlarm.addListener((a) => {
  if (a.name === DEBOUNCE_ALARM) void pushSnapshot().catch((e) => console.warn("bookmoor:", e));
});
