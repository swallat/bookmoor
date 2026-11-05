// "Sign in with bookmoor": instead of pasting a token, the options page opens
// the server's web UI in the browser's auth window. After the user signs in
// (and passes any login in front of the server, in the same window) and
// confirms, the server hands a fresh device token back in the URL fragment
// of the extension's redirect address.

/** The page the auth window opens. `redirect` is the extension's own redirect URL. */
export function connectUrl(serverUrl: string, redirect: string, state: string, name: string): string {
  const u = new URL(serverUrl);
  u.hash = "";
  u.search = "";
  u.pathname = u.pathname.replace(/\/+$/, "") + "/";
  u.searchParams.set("connect", redirect);
  u.searchParams.set("state", state);
  u.searchParams.set("name", name);
  return u.toString();
}

/** The token from the redirect URL, or throws if it is not the answer to our request. */
export function tokenFromRedirect(url: string, state: string): string {
  const hash = url.split("#")[1] ?? "";
  const p = new URLSearchParams(hash);
  if (p.get("state") !== state) throw new Error("The answer did not match this request. Try again.");
  const token = p.get("token");
  if (!token) throw new Error("The server did not return a token.");
  return token;
}

/** A readable default name for this device, editable on the confirmation page. */
export function deviceName(userAgent: string, platform: string): string {
  const browser = /Firefox\//.test(userAgent)
    ? "Firefox"
    : /Edg\//.test(userAgent)
      ? "Edge"
      : /Chrome\//.test(userAgent)
        ? "Chrome"
        : "Browser";
  const os = /Win/i.test(platform) ? "Windows" : /Mac/i.test(platform) ? "macOS" : /Linux|X11/i.test(platform) ? "Linux" : "";
  return os ? `${browser} on ${os}` : browser;
}

export function newState(): string {
  const raw = new Uint8Array(16);
  crypto.getRandomValues(raw);
  return [...raw].map((b) => b.toString(16).padStart(2, "0")).join("");
}

/** Run the sign-in and return the new token. Needs a user gesture (the auth window). */
export async function signInWithServer(serverUrl: string): Promise<string> {
  const state = newState();
  const redirect = chrome.identity.getRedirectURL();
  const result = await chrome.identity.launchWebAuthFlow({
    url: connectUrl(serverUrl, redirect, state, deviceName(navigator.userAgent, navigator.platform)),
    interactive: true,
  });
  if (!result) throw new Error("Sign-in was cancelled.");
  return tokenFromRedirect(result, state);
}
