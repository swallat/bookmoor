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
  if (outcome === "decide") {
    return "The server already has bookmarks for this profile. Choose how to combine them with this browser's.";
  }
  if (outcome === "login") {
    return "The login in front of the server has expired. Sign in again, then sync resumes by itself.";
  }
  if (error instanceof HttpError && error.status === 401) {
    return "The server rejected the token (401). Check the token in these settings.";
  }
  if (error instanceof HttpError && error.status === 403) {
    return error.detail && error.detail !== "forbidden"
      ? `The server refuses this token (403): ${error.detail}.`
      : "The server refuses this token (403). Use a token with scope sync.";
  }
  const why = error instanceof Error ? error.message : String(error);
  if (error instanceof TypeError) {
    return `Could not reach the server (${why}). Is it running, is the URL right, and does the extension have access to it?`;
  }
  return `Sync failed, trying again later: ${why}`;
}
