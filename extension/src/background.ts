import { flush, type Outcome } from "./flow.ts";
import { loadSettings } from "./settings.ts";
import { chromeBookmarks, chromeStateStore } from "./storage.ts";
import { httpTransport } from "./transport.ts";

// Events only trigger a sync; what to send is computed from the bookmark
// tree at flush time (docs/sync-architecture.md, 5.3). Chrome clamps alarms
// to 30 s, which doubles as the debounce.

const PUSH = "bookmoor-push";
const PULL = "bookmoor-pull";
const BACKOFF_MIN = 0.5;
const BACKOFF_MAX = 15;
const FAILURES = "bookmoor-failures";

function schedulePush(): void {
  chrome.alarms.create(PUSH, { delayInMinutes: 0.5 });
}

// One round at a time; the service worker may be asked twice at once.
let running: Promise<void> | null = null;

async function round(): Promise<void> {
  const settings = await loadSettings();
  if (!settings.serverUrl || !settings.token) return;
  const outcome: Outcome = await flush(
    chromeBookmarks,
    chromeStateStore,
    httpTransport(settings),
  );
  const stored = await chrome.storage.local.get(FAILURES);
  const failures = outcome === "retry" ? ((stored[FAILURES] as number | undefined) ?? 0) + 1 : 0;
  await chrome.storage.local.set({ [FAILURES]: failures });
  await chrome.action.setBadgeText({ text: outcome === "auth" ? "!" : "" });
  if (outcome === "retry") {
    const delay = Math.min(BACKOFF_MAX, BACKOFF_MIN * 2 ** (failures - 1));
    chrome.alarms.create(PUSH, { delayInMinutes: delay });
  }
}

function run(): void {
  running ??= round()
    .catch((e) => console.warn("bookmoor:", e))
    .finally(() => {
      running = null;
    });
}

chrome.bookmarks.onCreated.addListener(schedulePush);
chrome.bookmarks.onChanged.addListener(schedulePush);
chrome.bookmarks.onMoved.addListener(schedulePush);
chrome.bookmarks.onRemoved.addListener(schedulePush);
chrome.bookmarks.onChildrenReordered.addListener(schedulePush);
chrome.bookmarks.onImportEnded.addListener(schedulePush);
chrome.alarms.onAlarm.addListener((a) => {
  if (a.name === PUSH || a.name === PULL) run();
});
chrome.runtime.onStartup.addListener(run);
chrome.runtime.onInstalled.addListener(() => {
  chrome.alarms.create(PULL, { periodInMinutes: 5 });
  run();
});
