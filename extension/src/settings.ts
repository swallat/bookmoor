export interface HeaderEntry {
  name: string;
  value: string;
}

export interface Settings {
  serverUrl: string;
  /** bookmoor API token (inner auth layer). */
  token: string;
  /** Extra headers sent with every request, e.g. a Pangolin header-auth pair (outer auth layer). */
  headers: HeaderEntry[];
  /** Extra query parameters sent with every request, e.g. a share-link token. */
  query: HeaderEntry[];
}

const KEY = "settings";
const EMPTY: Settings = { serverUrl: "", token: "", headers: [], query: [] };

// Secrets stay in storage.local on purpose: storage.sync would copy them to
// every device signed into the browser profile.
export async function loadSettings(): Promise<Settings> {
  const stored = await chrome.storage.local.get(KEY);
  return { ...EMPTY, ...(stored[KEY] as Partial<Settings> | undefined) };
}

export async function saveSettings(s: Settings): Promise<void> {
  await chrome.storage.local.set({ [KEY]: s });
}

export function buildHeaders(s: Settings): Headers {
  const h = new Headers({ "Content-Type": "application/json" });
  if (s.token) h.set("Authorization", `Bearer ${s.token}`);
  for (const { name, value } of s.headers) {
    if (name.trim()) h.set(name.trim(), value);
  }
  return h;
}

export function originPattern(serverUrl: string): string {
  const u = new URL(serverUrl);
  return `${u.protocol}//${u.host}/*`;
}

/**
 * URL of an API path below the configured server URL. A path prefix in the
 * server URL (`https://host/bookmoor`, for a server mounted below a proxy
 * path) is kept, and query parameters in the server URL and the configured
 * extra ones are added to every request.
 */
export function endpointUrl(s: Pick<Settings, "serverUrl" | "query">, path: string): string {
  const u = new URL(s.serverUrl);
  u.pathname = u.pathname.replace(/\/+$/, "") + path;
  for (const { name, value } of s.query ?? []) {
    if (name.trim()) u.searchParams.set(name.trim(), value);
  }
  return u.toString();
}
