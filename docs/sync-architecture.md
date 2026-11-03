# Sync architecture for browser bookmarks

Status: proposal (targets v0.1). Extends [architecture.md](architecture.md);
where the two differ, this document is the more detailed one and
architecture.md should be updated when it is accepted.

Scope: the path *browser <-> server*. Adapters (Dynacat, Dashy) reuse the same
engine but are covered in `docs/adapters/`.

## 1. Goals and non-goals

Goals

- Chrome bookmarks of several devices converge to one canonical tree on the
  self-hosted server, without data loss on concurrent edits.
- Offline-tolerant: a device that is offline for days syncs on reconnect.
- Cheap steady state: deltas, not full snapshots (the v0.0 extension pushes a
  snapshot; see `extension/src/background.ts`).
- Survives MV3 service-worker restarts and browser restarts at any point.

Non-goals (v0.1): multi-user tenancy, end-to-end encryption, real-time push
(polling is enough), syncing browser-specific fields (favicons, date added,
reading list).

## 2. Data model (`bookmoor-core`)

### 2.1 Current state

`model.rs` has `Tree { root: Folder }`, `Node = Folder | Bookmark`, string
`NodeId`s and `flatten()` into `id -> FlatNode { parent, kind, title, url }`.
`diff.rs` compares two trees by id and emits `Added / Removed / Renamed /
UrlChanged / Moved`. Sibling order is not modelled or diffed.

### 2.2 Required additions

1. **Order.** Chrome bookmarks are ordered (`index` in `create`/`move`). Add a
   per-node `position` to `FlatNode`, a **fractional index** string (e.g.
   `"a0"`, `"a0V"`). Inserting between two siblings generates a key between
   theirs, so a reorder is a single-node change that merges without rewriting
   siblings. Add `Change::Reordered { id, position }` (or fold it into
   `Moved`). Ties are broken by id so the order is always total.
2. **Root roles.** Chrome has three fixed roots (`1` Bookmarks bar, `2` Other
   bookmarks, `3` Mobile; the invisible root `0` is never synced). Browser ids
   differ across devices and browsers, so the canonical tree gets stable
   well-known children of the root: `toolbar`, `other`, `mobile`. The extension
   maps by *role*, never by the Chrome id. Firefox (`toolbar_____`, `unfiled_____`,
   `menu________`) maps to the same roles in v0.3; `menu` has no Chrome
   counterpart and is mapped to a subfolder of `other` there.
3. **Tombstones.** The canonical store keeps `Removed` nodes as
   `{ id, deleted_at, deleted_rev }` (already promised in architecture.md).
   They are purged after a retention window (default 90 days); a client whose
   cursor is older than the purge horizon must do a full resync (see 3.4).
4. **Revisions.** One monotonically increasing `rev: u64` per server
   (a global change counter). Every applied change gets a `rev`; each node
   additionally stores `rev` of its last change per field group
   (`title`, `url`, `placement` = parent+position, `deleted`). This is what makes
   per-field merging cheap.
5. **Ids.** Server ids are opaque, client-independent (`bk_` + ULID). A client
   proposes new ids for nodes it creates (so the request is idempotent and the
   id map can be written before the response arrives); the server accepts them
   if unused.
6. **Validation.** URL is a required, parseable string for bookmarks;
   `javascript:` and `data:` URLs are stored but never emitted to adapters that
   render them as links. Folder titles may be empty (Chrome allows it).

### 2.3 Per-endpoint state

For each endpoint (a device or an adapter) the server stores
`EndpointState { endpoint_id, kind, direction, strategy, cursor_rev, base: Tree }`.
`base` is the tree as of the last successful sync with that endpoint. For
browsers this can be derived from `cursor_rev` plus the tombstoned history
instead of being stored as a full copy; the first implementation may simply
store a copy and optimise later.

## 3. Sync protocol and server API (`bookmoor-server`)

### 3.1 Principles

- The server is the only serialisation point. All merges run inside one
  transaction per sync request, so there is no distributed conflict resolution.
- Clients send **operations relative to the server revision they last saw**
  (`since_rev`), and receive every canonical change after that revision.
- Requests are **idempotent**: each op carries a client-generated `op_id`; the
  server remembers recent `op_id`s per endpoint and returns the same result
  for a retried request.

### 3.2 Endpoints

Existing plan: `GET /health`, `GET /v1/tree`, `POST /v1/sync`. Proposed
v0.1 surface (all JSON, all under `/v1`, all but `/health` authenticated):

| Method / path | Purpose |
|---|---|
| `GET /health` | Liveness, no auth, no data. |
| `POST /v1/endpoints` | Register a device; returns `endpoint_id`. Requires the enrolment token (section 6). |
| `GET /v1/tree` | Full canonical tree + `rev`. Used for first sync and for resync after a purge. |
| `POST /v1/sync` | Delta exchange (below). |
| `DELETE /v1/endpoints/{id}` | Revoke a device and drop its base state. |

`POST /v1/sync`

```jsonc
// request
{
  "endpoint_id": "ep_...",
  "since_rev": 4211,                 // last server rev this client has applied
  "ops": [
    { "op_id": "01J...", "type": "add",    "id": "bk_new", "parent": "bk_f1",
      "position": "a0V", "kind": "bookmark", "title": "HN", "url": "https://news.ycombinator.com" },
    { "op_id": "01J...", "type": "update", "id": "bk_b1", "title": "GH" },
    { "op_id": "01J...", "type": "move",   "id": "bk_b2", "parent": "bk_f2", "position": "a1" },
    { "op_id": "01J...", "type": "remove", "id": "bk_b3" }
  ]
}
// response
{
  "rev": 4219,                       // new cursor
  "ops": [ /* canonical changes in (since_rev, rev], excluding the client's own */ ],
  "rejected": [ { "op_id": "01J...", "reason": "conflict_lost" | "invalid" | "gone" } ],
  "resync": false                    // true => discard local base, call GET /v1/tree
}
```

Notes

- The ops are the `Change` enum of `bookmoor-core::diff` plus `position`;
  the wire format should be generated from that type (serde) so the extension
  and server cannot drift. The extension's TypeScript types are generated from
  the same schema.
- The v0.0 `snapshot` body stays accepted until the delta path ships, then is
  removed (it is only for a first bootstrap, which `GET /v1/tree` and a
  one-off `"snapshot"` op in the first request cover).
- Ops are applied in order and **parent-first** for adds (matches the
  ordering guarantee in `diff()`).
- A large first sync is chunked (`ops` max e.g. 500 per request; the client
  loops until it has sent everything, keeping `since_rev` of the last response).
- Pulling without local changes is the same call with `ops: []`.
- In a response, an `add` for an id the client already has is an **upsert**
  of the full node (parent, position, title, url). The server uses it to
  restore nodes after a conflict (edit wins over delete, revived folders,
  re-parenting) and to re-assert nodes the client dropped together with a
  subtree that others changed concurrently. Clients apply response ops in
  order.
- Fields are last-write-wins by arrival order at the server; the engine
  tracks the endpoint and revision of the last change per field group of each
  node and compares it with the request's `since_rev` for edit-vs-delete
  decisions.
- Server-side limits: body size, ops per request, tree size; reject with
  `413` / `422` rather than truncating.
- HTTP status: `401/403` auth, `409` never (conflicts are resolved, not
  surfaced as errors), `429` rate limit with `Retry-After`.

### 3.3 Server pipeline per request

1. Authenticate, load `EndpointState`.
2. Deduplicate by `op_id`.
3. Apply each op to the canonical tree with the conflict rules of section 4,
   assigning `rev`s; collect `rejected`.
4. Compute the outgoing delta: canonical changes with `rev > since_rev`,
   minus changes that this very request produced from this client.
5. Commit; update `cursor_rev`; fan out to adapters asynchronously (adapters
   have their own `EndpointState` and run on their own schedule/watch).

Storage: SQLite (WAL) is the intended default for a self-hosted single
binary: tables `nodes`, `tombstones`, `endpoints`, `op_log(rev, ...)`,
`seen_ops`. The `op_log` is what makes "changes since rev N" cheap and is
pruned together with tombstones.

### 3.4 Resync

`resync: true` is returned when `since_rev` is older than the purge horizon,
the endpoint is unknown (database restored from backup), or the server
detects an inconsistent client (e.g. an op references a parent that never
existed). The client then fetches `GET /v1/tree`, **matches it to its local
tree** (section 5.4) and continues from the new `rev`. Resync never deletes
local bookmarks that are not on the server without going through the same
three-way logic; it is a rebase, not a wipe.

## 4. Conflict resolution

Two edits conflict when they touch the same node and both are newer than the
`since_rev` of the later sender. The engine merges **per field group**, so
most concurrent edits are not conflicts at all.

| Situation | Resolution |
|---|---|
| Different fields of one node (rename on A, URL change on B) | Both applied. |
| Same field on both sides | Per endpoint strategy: `newest` (default; compares server receive time, since client clocks cannot be trusted), `prefer-browser`, `prefer-adapter`. Loser's value is kept in the `op_log` for recovery. |
| Edit vs. delete | **Edit wins** (the node is resurrected with the edit), because losing a deliberately edited bookmark is worse than keeping a stale one. Exception: `prefer-*` strategies where the preferred side deleted. |
| Delete vs. delete | No-op. |
| Add vs. add of the same URL in the same folder | Both kept unless the endpoint enables `dedupe`; then merged by fingerprint (section 2 of architecture.md). |
| Move vs. move (same node) | Per-strategy winner, as for fields. |
| Move vs. delete of the target parent | The moved node is re-parented to the nearest surviving ancestor of the deleted folder (never silently deleted). |
| Move creating a cycle (A moves X into Y, B moves Y into X) | The later op is rejected as `conflict_lost` and the client receives the canonical placement. |
| Folder delete on A, new child added inside on B | The folder survives (it is resurrected as an ancestor of the new child); the other children stay deleted. |
| Concurrent reorder | Fractional positions make it a field conflict on one node; no sibling rewrites. |
| Concurrent insert at the same position | Positions collide; the tie-break is id order, then the server re-balances positions on the next write. |

Principles

- **Never lose data silently.** Every rejected or overwritten value stays in
  the `op_log` until the retention window ends; a future "history" view can
  restore it. v0.1 exposes it only through the database.
- **Deterministic.** The same set of ops yields the same tree regardless of
  arrival order wherever possible; the server's receive order is the final
  tie-break.
- **Echo suppression** is the server's job as well: an op that equals the
  canonical state (the client replaying a change it just received) is a no-op
  and does not bump `rev`.
- Adapters that cannot represent something (`Capabilities::max_depth`,
  identity strategies) are handled in their own conflict layer: the engine
  hands them diffs only (as the `Adapter` trait docs require), and lossy
  projections are recorded as "unrepresentable" instead of being fed back
  into the canonical tree.

## 5. Browser side: how the extension uses `chrome.bookmarks`

### 5.1 Permissions

`manifest.json` already declares `bookmarks`, `storage`, `alarms` and
`optional_host_permissions`. The host permission for the configured server
origin is requested at runtime from the options page (`originPattern()` in
`settings.ts`), which keeps the install-time warning list short and lets
cross-origin `fetch` from the service worker work without CORS headers on
the server. The extension should keep `fetch` as the only network API
(no content scripts, no `webRequest`).

### 5.2 Reading and writing

| Need | API |
|---|---|
| Initial snapshot, resync matching | `chrome.bookmarks.getTree()` (one call, full tree; fine up to tens of thousands of nodes) |
| Apply remote add | `bookmarks.create({ parentId, index, title, url })` |
| Apply remote rename / URL | `bookmarks.update(id, { title, url })` |
| Apply remote move / reorder | `bookmarks.move(id, { parentId, index })` |
| Apply remote delete | `bookmarks.remove(id)` for bookmarks and empty folders, `bookmarks.removeTree(id)` for folders |
| Local change detection | `onCreated`, `onChanged` (title/url only), `onMoved`, `onRemoved` (gives `parentId`, `index` and the removed node), `onChildrenReordered` |
| Bulk operations | `onImportBegan` / `onImportEnded` (suspend per-event handling, then diff once) |

Fractional positions from the server are converted to integer indices when
writing (`index` = rank among the folder's current children); on reading,
the extension computes positions from neighbours (new key between left and
right sibling) so it never has to rewrite siblings.

Unmodifiable nodes (managed bookmarks, `unmodifiable: "managed"`) and the
three root folders are never created, moved or deleted; they are synced
read-only or ignored. Chrome-specific quotas on bookmark writes exist; a first
sync of thousands of bookmarks must be throttled and resumable. The exact
limits should be verified against the current Chrome documentation before
implementing the applier.

### 5.3 Event handling in an MV3 service worker

The service worker is stopped after ~30 s idle and restarted for events, so:

- **Nothing is held in memory across events.** The browser-id <-> server-id
  map, the pending-op queue and `since_rev` live in `chrome.storage.local`
  (or IndexedDB once it grows large; the map is O(bookmarks)). Settings with
  secrets already live there (`settings.ts`).
- **Debounce with `chrome.alarms`,** as v0.0 does. Note that current Chrome
  clamps alarms to a minimum period of 30 seconds, so the `0.1` minutes in
  `background.ts` effectively becomes 30 s; document that, or batch via a
  short-lived in-worker timer plus an alarm as safety net.
- Each event handler only **appends a descriptor to a persisted queue** and
  schedules the alarm; the actual diff/ops are computed at flush time from
  `getTree()` against the stored base. This is robust against missed events
  (e.g. the worker was dead during an import) because the flush is a
  *state diff*, not an event replay. The events are only a trigger.
- A periodic alarm (default 5 min) triggers a pull with `ops: []`, and
  `runtime.onStartup` / `onInstalled` trigger a full reconcile.
- A single `sync` mutex (a lock record in storage with a lease) prevents two
  concurrent flushes if events arrive while one is running.

### 5.4 Id map and first-sync matching

`idMap: { [chromeId]: serverId }` plus the reverse map is the extension's
`base`. Chrome ids are local to a profile and stable across restarts, but not
across devices, and are not reused after deletion within a profile.

First sync or resync of a device with existing bookmarks must not duplicate
everything. Matching algorithm:

1. Map roots by role (section 2.2).
2. For each folder, match children by `(kind, normalised title, url)`
   (the fingerprint of `identity.rs`), pairing in sibling order for ties.
3. Matched pairs go into the id map; unmatched local nodes become `add` ops;
   unmatched server nodes become local creates. Nothing is deleted in this
   phase, since there is no base to tell "deleted" from "never existed".

### 5.5 Applying remote changes without echo

Writes made by the extension itself trigger the same `onCreated` etc. events.
Before applying a remote op the extension records `chromeId`/op in a
persisted `applying` set; the handler drops events matching it. Because
creation yields the Chrome id only *after* the call, `create` is performed
first, the id-map entry is written, and only then is the event allowed to be
processed (events are queued, not handled inline, see 5.3); the flush-time
state diff then sees no difference between the tree and the updated base,
which is the real loop prevention. The `applying` set is an optimisation, not
the correctness mechanism.

### 5.6 Failure behaviour

- Network or `5xx`: keep the queue, back off exponentially (max 15 min), try
  again on the next alarm. Never drop local changes.
- `401/403`: stop syncing, show a badge on the action icon, wait for the user
  to fix the token (options page).
- `resync: true`: run 3.4/5.4 matching.
- Remote op that no longer applies locally (parent deleted locally):
  skipped and reported at the next diff, which converges via normal conflict
  rules.

## 6. Authentication and authorisation

Two layers, as in architecture.md:

1. **Outer (optional):** a reverse proxy in front of the server (e.g. Pangolin
   header auth, Authelia, Cloudflare Access). The extension sends the
   user-configured static headers (`Settings.headers`). The server does
   not interpret them; it must work with or without them.
2. **Inner (mandatory):** bookmoor API tokens.

Token design

- Format: `bkm_<id>_<secret>` where `secret` is 256 random bits. The server
  stores `id`, `sha256(secret)` (a fast hash is adequate for high-entropy
  secrets), name, scope, `created_at`, `last_used_at`, `revoked_at`.
  Lookup by `id`, compare in constant time.
- Sent as `Authorization: Bearer ...`. Always TLS; the server refuses to
  start with a non-loopback bind and no TLS/forwarded-proto unless explicitly
  told (`--insecure-http`), because the token otherwise travels in clear text.
- **Enrolment:** the first admin token is printed once on first start (or set
  via an environment variable). An admin creates per-device tokens
  (`bookmoor-server token create --name laptop --scope sync`), and
  the extension is configured by pasting server URL + token. A QR/enrol-code flow is a
  later convenience.
- **Scopes:** `sync` (read/write the tree, own endpoint), `read` (tree only,
  for dashboards/read-only consumers), `admin` (tokens, endpoints). A token
  maps to exactly one endpoint, which makes revocation = "stop this device".
- Rotation: create a new token, update the device, revoke the old one. No
  automatic expiry in v0.1; `last_used_at` allows spotting stale ones.
- Rate limiting: a token bucket per token (burst 120, 10/s) and a separate
  bucket per peer IP that only failed authentications draw from (burst 10,
  then one per 5 s), so bad tokens get `429` with `Retry-After` while a valid
  token behind the same address (a reverse proxy) is unaffected. Auth
  failures otherwise return a uniform `401`. Buckets are in memory and
  bounded. Behind a proxy the peer IP is the proxy's, so the failed-auth
  bucket is then shared by all clients; `X-Forwarded-For` is deliberately not
  trusted.
- The extension stores the token in `chrome.storage.local`, not `sync`
  (already so in `settings.ts`). Chrome does not offer a secure secret store for
  extensions; the token is readable by anyone with access to the profile.
  The scope/revocation model is the mitigation, not encryption.
- Single user in v0.1: all tokens belong to one tree. Multi-user would add a
  `tree_id` to every table and token; keep it out of the API paths for now so
  it can be added without a breaking change.

## 7. Open questions

1. **Fractional index library** — *decided*: small own implementation in
   `bookmoor-core`, with shared test vectors that the extension also runs.
   No external dependency; Rust and TypeScript order identically.
2. **Edit-vs-delete policy** — *decided*: "edit wins" is fixed, not
   configurable per endpoint.
3. **Store `base` per endpoint as a copy or derive it from the op log?**
   The copy is simpler; the log is smaller. Decide after measuring with a
   realistic 20k-bookmark tree.
4. **Firefox menu folder** mapping (2.2) and Firefox's own ids/guids: Firefox
   bookmarks have stable `guid`s that could replace the fingerprint
   matching entirely; Chrome has `syncing`-managed GUIDs internally but does
   not expose them in the extension API.
5. **Wire schema** — *decided*: generate JSON Schema from Rust (`schemars`)
   and derive the TypeScript types from it. One source of truth.
6. **Real-time push** (SSE/WebSocket) as v0.2 follow-up; MV3 service workers
   make long-lived connections awkward, so polling stays the baseline.

## 8. Implementation order

1. Core: add `position` and `Reordered` to model/diff, with tests; add the
   three-way `merge(base, ours, theirs)` returning merged tree + rejected
   changes, using the table in section 4 as test cases.
2. Server: SQLite storage, token auth, `POST /v1/sync` and `GET /v1/tree`.
3. Extension: id map + persisted queue, flush-time state diff, applier with
   echo handling, first-sync matching, polling alarm.
4. End-to-end test harness: two simulated browsers (a fake `chrome.bookmarks`)
   against one in-process server, with property tests that random
   interleavings converge.
