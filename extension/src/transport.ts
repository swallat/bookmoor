import { HttpError, LoginRequired, type Transport } from "./flow.ts";
import { buildHeaders, endpointUrl, type Settings } from "./settings.ts";
import type { SyncRequest, SyncResponse, TreeResponse } from "./wire.ts";

/** A redirect or a non-JSON page where the API answers JSON: the proxy's login. */
export function isLoginPage(res: Pick<Response, "type" | "status" | "headers">): boolean {
  if (res.type === "opaqueredirect" || (res.status >= 300 && res.status < 400)) return true;
  const type = res.headers.get("content-type") ?? "";
  return /text\/html/i.test(type);
}

/** The server API over `fetch`. */
export function httpTransport(settings: Settings, fetchImpl: typeof fetch = fetch): Transport {
  const url = (path: string) => endpointUrl(settings, path);
  const call = async <T>(method: string, path: string, body?: unknown): Promise<T> => {
    const res = await fetchImpl(url(path), {
      method,
      headers: buildHeaders(settings),
      // Behind an SSO proxy the session cookie is the outer login. A redirect
      // to the login page must not be followed (it is cross-origin HTML).
      ...(settings.sso ? { credentials: "include" as const, redirect: "manual" as const } : {}),
      ...(body === undefined ? {} : { body: JSON.stringify(body) }),
    });
    if (settings.sso && isLoginPage(res)) throw new LoginRequired();
    if (!res.ok) {
      const body = (await res.json().catch(() => null)) as { error?: unknown } | null;
      throw new HttpError(res.status, typeof body?.error === "string" ? body.error : undefined);
    }
    if (res.status === 204) return undefined as T;
    return (await res.json()) as T;
  };
  return {
    endpoint: async () => (await call<{ endpoint_id: string }>("POST", "/v1/endpoints")).endpoint_id,
    sync: (req: SyncRequest) => call<SyncResponse>("POST", "/v1/sync", req),
    tree: () => call<TreeResponse>("GET", "/v1/tree"),
    reset: () => call<void>("POST", "/v1/tree/reset"),
  };
}
