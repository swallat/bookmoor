// GENERATED from schema/wire.schema.json by `npm run gen:types`. Do not edit.
/* eslint-disable */

/**
 * One client operation. `op_id` makes retried requests idempotent.
 */
export type Op = {
  op_id: string;
} & Op1;
export type Op1 =
  | {
      id: string;
      kind: Kind;
      parent: string;
      position: string;
      title: string;
      type: "add";
      url?: string | null;
    }
  | {
      id: string;
      title?: string | null;
      type: "update";
      url?: string | null;
    }
  | {
      id: string;
      parent: string;
      position: string;
      type: "move";
    }
  | {
      id: string;
      type: "remove";
    };
export type Kind = "folder" | "bookmark";
export type RejectReason = "conflict_lost" | "invalid" | "gone";

/**
 * All wire types in one schema document (`$defs`), so one generation step
 * covers the whole protocol.
 */
export interface Protocol {
  sync_request: SyncRequest;
  sync_response: SyncResponse;
  tree_response: TreeResponse;
}
export interface SyncRequest {
  endpoint_id: string;
  ops: Op[];
  /**
   * Last server revision this client has applied.
   */
  since_rev: number;
}
export interface SyncResponse {
  /**
   * Canonical changes in `(since_rev, rev]`, excluding the client's own.
   */
  ops: Op[];
  rejected: RejectedOp[];
  /**
   * `true`: discard the local base and call `GET /v1/tree`.
   */
  resync: boolean;
  /**
   * New cursor.
   */
  rev: number;
}
export interface RejectedOp {
  op_id: string;
  reason: RejectReason;
}
/**
 * `GET /v1/tree`: the full canonical tree, flattened, parent-first.
 */
export interface TreeResponse {
  nodes: WireNode[];
  rev: number;
}
/**
 * A node of the canonical tree as sent over the wire.
 */
export interface WireNode {
  id: string;
  kind: Kind;
  /**
   * `null` only for the tree root.
   */
  parent?: string | null;
  position: string;
  title: string;
  /**
   * Required for bookmarks, absent for folders.
   */
  url?: string | null;
}
