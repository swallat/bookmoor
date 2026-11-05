import { flush, last, startOver, type Outcome, type Restart } from "./flow.ts";
import { loadSettings, hasHostAccess } from "./settings.ts";
import { chromeBookmarks, chromeStateStore } from "./storage.ts";
import { describe, STATUS_KEY, type SyncStatus } from "./status.ts";
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

async function setStatus(outcome: SyncStatus["outcome"], message: string): Promise<void> {
  const status: SyncStatus = { at: new Date().toISOString(), outcome, message };
  await chrome.storage.local.set({ [STATUS_KEY]: status });
}

// One round at a time; the service worker may be asked twice at once.
let running: Promise<void> | null = null;

async function round(restart?: Restart): Promise<void> {
  const settings = await loadSettings();
  if (!settings.serverUrl || !settings.token) {
    await setStatus("unconfigured", "Enter the server URL and a token in these settings.");
    return;
  }
  // Without host access the browser blocks the request and the error says
  // little ("NetworkError"), so check it first and say what is missing.
  if (!(await hasHostAccess(settings.serverUrl).catch(() => true))) {
    await setStatus(
      "auth",
      `The extension has no access to ${new URL(settings.serverUrl).host}. ` +
        "Open the settings and save again to grant it, or allow it under Permissions in the browser's add-on page.",
    );
    await chrome.action.setBadgeText({ text: "!" });
    return;
  }
  const identity = `${settings.serverUrl}\n${settings.token}\n${JSON.stringify(settings.query)}`;
  const outcome: Outcome = restart
    ? await startOver(chromeBookmarks, chromeStateStore, httpTransport(settings), identity, restart)
    : await flush(chromeBookmarks, chromeStateStore, httpTransport(settings), identity, true);
  await setStatus(outcome, describe(outcome, last.error));
  const stored = await chrome.storage.local.get(FAILURES);
  const failures = outcome === "retry" ? ((stored[FAILURES] as number | undefined) ?? 0) + 1 : 0;
  await chrome.storage.local.set({ [FAILURES]: failures });
  await chrome.action.setBadgeText({
    text: outcome === "auth" || outcome === "login" || outcome === "decide" ? "!" : "",
  });
  if (outcome === "retry") {
    const delay = Math.min(BACKOFF_MAX, BACKOFF_MIN * 2 ** (failures - 1));
    chrome.alarms.create(PUSH, { delayInMinutes: delay });
  }
}

function run(restart?: Restart): Promise<void> {
  running ??= round(restart)
    .catch((e) => console.warn("bookmoor:", e))
    .finally(() => {
      running = null;
    });
  return running;
}

/** Start over once no other round runs, and say how it went. */
async function startOverExclusively(mode: Restart): Promise<{ ok: boolean; message: string }> {
  // A round in progress must not see the work half done, and another one must
  // not take this one's place: check and take the slot in one step.
  while (running) await running;
  await run(mode);
  const status = (await chrome.storage.local.get(STATUS_KEY))[STATUS_KEY] as SyncStatus | undefined;
  return { ok: status?.outcome === "ok", message: status?.message ?? "" };
}

chrome.bookmarks.onCreated.addListener(schedulePush);
chrome.bookmarks.onChanged.addListener(schedulePush);
chrome.bookmarks.onMoved.addListener(schedulePush);
chrome.bookmarks.onRemoved.addListener(schedulePush);
// Chrome-only events: Firefox has no such properties, and calling
// addListener on undefined would abort the whole script (found by the
// Firefox end-to-end test).
chrome.bookmarks.onChildrenReordered?.addListener(schedulePush);
chrome.bookmarks.onImportEnded?.addListener(schedulePush);
chrome.alarms.onAlarm.addListener((a) => {
  if (a.name === PUSH || a.name === PULL) run();
});
// Alarms may be gone after a browser restart, so make sure on every start.
void chrome.alarms.get(PULL).then((a) => {
  if (!a) chrome.alarms.create(PULL, { periodInMinutes: 5 });
});
// "Sync now" for the end-to-end tests, which cannot wait for the 30 s alarm.
// Only the extension's own pages can send this (onMessage, not
// onMessageExternal).
chrome.runtime.onMessage.addListener((msg: unknown, _sender, respond) => {
  const m = msg as { type?: string; mode?: string } | null;
  if (m?.type === "bookmoor-sign-in") {
    void signIn().then(() => respond({ ok: true }));
    return true;
  }
  if (m?.type === "bookmoor-sync-now") {
    void run().then(() => respond({ ok: true }));
    return true;
  }
  if (m?.type === "bookmoor-start-over" && (m.mode === "adopt" || m.mode === "overwrite" || m.mode === "merge")) {
    const mode = m.mode;
    void startOverExclusively(mode).then(respond);
    return true;
  }
  return false;
});

// Behind an SSO proxy the user signs in in a normal tab; the proxy sets its
// session cookie there and the extension's requests carry it from then on.
// Closing that tab is the signal to try again. The tab id lives in storage
// because the service worker may be restarted while the tab is open.
const LOGIN_TAB = "bookmoor-login-tab";

async function signIn(): Promise<void> {
  const settings = await loadSettings();
  if (!settings.serverUrl) return;
  const tab = await chrome.tabs.create({ url: settings.serverUrl });
  if (tab.id !== undefined) await chrome.storage.local.set({ [LOGIN_TAB]: tab.id });
}

chrome.tabs.onRemoved.addListener(async (tabId) => {
  const stored = await chrome.storage.local.get(LOGIN_TAB);
  if (stored[LOGIN_TAB] !== tabId) return;
  await chrome.storage.local.remove(LOGIN_TAB);
  await run();
});
chrome.runtime.onStartup.addListener(() => void run());
chrome.runtime.onInstalled.addListener(() => void run());
