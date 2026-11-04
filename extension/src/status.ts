import { HttpError, type Outcome } from "./flow.ts";

/** What the options page shows about the last sync round. */
export interface SyncStatus {
  /** ISO time of the round. */
  at: string;
  outcome: Outcome | "unconfigured";
  message: string;
}

export const STATUS_KEY = "bookmoor-status";

/** A sentence for the user; `error` is what made `flush` return `outcome`. */
export function describe(outcome: Outcome, error: unknown): string {
  if (outcome === "ok") return "Last sync succeeded.";
  if (error instanceof HttpError && error.status === 401) {
    return "The server rejected the token (401). Check the token in these settings.";
  }
  if (error instanceof HttpError && error.status === 403) {
    return error.detail && error.detail !== "forbidden"
      ? `The server refuses this token (403): ${error.detail}.`
      : "The server refuses this token (403). Use a token with scope sync.";
  }
  const why = error instanceof Error ? error.message : String(error);
  return `Sync failed, trying again later: ${why}`;
}
