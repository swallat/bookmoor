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
  /**
   * The server is behind a login page (Pangolin SSO, OIDC proxy): send the
   * browser's session cookie and report a redirect to the login as "sign in
   * needed" instead of a network error.
   */
  sso: boolean;
}

const KEY = "settings";
const EMPTY: Settings = { serverUrl: "", token: "", headers: [], query: [], sso: false };

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
  // A pasted token often carries a trailing space or newline, which is not a valid header value.
  const token = s.token.trim();
  if (token) h.set("Authorization", `Bearer ${token}`);
  for (const { name, value } of s.headers) {
    if (name.trim()) h.set(name.trim(), value);
  }
  return h;
}

/**
 * Host permission to ask for: the server's host without its port, which
 * browsers treat as covering every port (the end-to-end tests rely on it).
 */
export function originPattern(serverUrl: string): string {
  const u = new URL(serverUrl);
  return `${u.protocol}//${u.hostname}/*`;
}

/** Every pattern whose grant lets the extension reach the server: the port-less one and, from older versions, the one with the port. */
export function originPatterns(serverUrl: string): string[] {
  const u = new URL(serverUrl);
  return [...new Set([originPattern(serverUrl), `${u.protocol}//${u.host}/*`])];
}

/** Whether any grant covers the server; `contains` alone wants every pattern granted. */
export async function hasHostAccess(serverUrl: string): Promise<boolean> {
  for (const origin of originPatterns(serverUrl)) {
    if (await chrome.permissions.contains({ origins: [origin] })) return true;
  }
  return false;
}

/**
 * URL of an API path below the configured server URL. A path prefix in the
 * server URL (`https://host/bookmoor`, for a server mounted below a proxy
 * path) is kept, and query parameters in the server URL and the configured
 * extra ones are added to every request (a configured parameter replaces a
 * same-named one from the URL). Parameters travel in the URL, so proxies and
 * servers may log them; prefer headers where the proxy allows.
 */
export function endpointUrl(s: Pick<Settings, "serverUrl" | "query">, path: string): string {
  const u = new URL(s.serverUrl);
  u.hash = "";
  u.pathname = u.pathname.replace(/\/+$/, "") + path;
  for (const { name, value } of s.query ?? []) {
    if (name.trim()) u.searchParams.set(name.trim(), value);
  }
  return u.toString();
}
