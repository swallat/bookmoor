import { HttpError, type Transport } from "./flow.ts";
import { buildHeaders, type Settings } from "./settings.ts";
import type { SyncRequest, SyncResponse, TreeResponse } from "./wire.ts";

/** The server API over `fetch`. */
export function httpTransport(settings: Settings, fetchImpl: typeof fetch = fetch): Transport {
  const url = (path: string) => new URL(path, settings.serverUrl).toString();
  const call = async <T>(method: string, path: string, body?: unknown): Promise<T> => {
    const res = await fetchImpl(url(path), {
      method,
      headers: buildHeaders(settings),
      ...(body === undefined ? {} : { body: JSON.stringify(body) }),
    });
    if (!res.ok) throw new HttpError(res.status);
    return (await res.json()) as T;
  };
  return {
    endpoint: async () => (await call<{ endpoint_id: string }>("POST", "/v1/endpoints")).endpoint_id,
    sync: (req: SyncRequest) => call<SyncResponse>("POST", "/v1/sync", req),
    tree: () => call<TreeResponse>("GET", "/v1/tree"),
  };
}
