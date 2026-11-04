<p align="center">
  <img src="assets/logo/bookmoor-icon.svg" alt="bookmoor logo" width="128" height="128">
</p>

<h1 align="center">bookmoor</h1>

<p align="center">
  <strong>Self-hosted bookmark sync. Your bookmarks, moored on your own server.</strong>
</p>

<p align="center">
  <a href="https://github.com/swallat/bookmoor/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/swallat/bookmoor/actions/workflows/ci.yml/badge.svg"></a>
  <a href="LICENSE"><img alt="License: Apache-2.0" src="https://img.shields.io/badge/license-Apache--2.0-1D4E89"></a>
  <img alt="Status: early alpha" src="https://img.shields.io/badge/status-early%20alpha-FFB703">
  <img alt="Rust" src="https://img.shields.io/badge/server-Rust-1D4E89">
  <img alt="TypeScript" src="https://img.shields.io/badge/extension-TypeScript-1D4E89">
</p>

bookmoor keeps the bookmarks of all your browsers in one canonical tree on a
server you run yourself. No account with a third party, no cloud, one small
binary and one JSON file.

> **Status: early alpha (v0.0).** The sync engine, the server and the
> extension work and are covered by tests, including end-to-end tests against
> the real server binary. CI also runs the real extension in two real
> Chromium (and Firefox) profiles against the real server, but it has seen
> little manual use yet. Expect rough edges and
> do not make bookmoor your only copy of anything important yet.

## Why bookmoor

- **Yours.** The server stores everything in a single JSON file you can back
  up with `cp`. Only token hashes are stored, never the tokens.
- **Convergent.** Several devices edit concurrently and end up with the same
  tree. Changes merge per field with a three-way diff, so a rename on one
  laptop and a new URL on another both survive.
- **Safe by design.** Edit beats delete and moved nodes are never silently
  deleted. Keeping overwritten values for recovery is part of the design but
  not implemented yet. See
  [conflict resolution](docs/sync-architecture.md#4-conflict-resolution).
- **Offline-tolerant.** A device that was away for days catches up with
  deltas when it reconnects, and survives MV3 service-worker restarts.
- **Proxy-friendly.** Run it behind your reverse proxy. The extension can send
  extra headers (for example Pangolin header auth) and the server supports a
  path prefix. On top of that sit per-device API tokens with scopes and a
  rate limit.

## How it works

```mermaid
flowchart LR
    C["Chrome extension"] <--> S[("bookmoor server<br/>canonical tree")]
    F["Firefox extension"] <--> S
    S -. planned .-> A["Adapters<br/>Dynacat, Dashy"]
```

Every endpoint is a peer. The extension reports what changed since the last
sync, the server merges it into the canonical tree, assigns stable ids and
revisions, and hands every other device only the delta. Positions use
fractional indexes, so reordering never rewrites siblings.

Details: [docs/sync-architecture.md](docs/sync-architecture.md) and
[docs/architecture.md](docs/architecture.md).

## What works today

| Area | State |
|---|---|
| Core: data model, three-way diff and merge, fractional positions | Done, tested |
| Server: HTTP API, JSON snapshot persistence, device tokens, rate limit | Done, tested |
| Chrome extension: delta sync, id map, echo suppression | Done, tested against a fake API and the real server |
| Firefox: root folder mapping, manifest (`extension/dist-firefox`) | Done, untested in a real Firefox |
| Real-browser E2E (two Chromium / Firefox profiles, `npm run test:e2e`) | Done, in CI |
| Multi-instance service-worker lease | Open |
| Adapters for Dynacat and Dashy | Planned, requirements in [docs/adapters](docs/adapters) |

## Quick start

You need [mise](https://mise.jdx.dev) (or Rust and Node by hand).

```bash
git clone https://github.com/swallat/bookmoor && cd bookmoor
mise install
mise run build
mise run server        # prints an admin token once, state in ./state.json
```

Then load `extension/dist` via `chrome://extensions` > Developer mode > Load
unpacked, and enter the server URL and a device token in the extension
options. Create device tokens as described under [Tokens](#tokens).

## Layout

| Path | Content |
|---|---|
| `crates/bookmoor-core` | Data model, adapter trait, identity strategies, diff and merge (Rust) |
| `crates/bookmoor-server` | The self-hosted server (Rust) |
| `extension/` | WebExtension in TypeScript (Chrome first) |
| `schema/` | JSON Schema generated from the Rust types |
| `docs/` | Architecture, ADRs, per-adapter requirements analyses |

## Roadmap

- **v0.1** Chrome extension against a real browser, hardening, first release.
- **v0.2** Dynacat adapter, conflict strategies per endpoint.
- **v0.3** Firefox, Dashy, a generic exec adapter.

Not planned for v0.1: multi-user tenancy, end-to-end encryption, real-time
push, and browser-specific fields such as favicons or the reading list.

## Development

Tools and build commands run through [mise](https://mise.jdx.dev):

```bash
mise install          # Rust and Node
mise run build        # server (target/release), extension/dist (Chrome), extension/dist-firefox
mise run check        # what CI checks
mise run server       # start the server, state in ./state.json
mise tasks            # all tasks
```

Without mise: `cargo test`, and `cd extension && npm ci && npm run build`.
Chrome: load `extension/dist` via `chrome://extensions` > Developer mode > Load
unpacked. Firefox: `about:debugging` > This Firefox > Load Temporary Add-on,
then pick `extension/dist-firefox/manifest.json` (gone after a restart).

### Log

The server writes one line per request to stderr: time (UTC), client address,
token name (`-` if unauthenticated), method, path, status and duration. Query
strings are left out because proxy setups may carry secrets there; `/health`
is not logged. Behind a reverse proxy the address is the proxy's.

### Tokens

The first start with a new state file prints an admin token once. Only a hash
is stored, so a lost token cannot be shown again. Device tokens come from
`POST /v1/tokens` (admin token required) or, with the server stopped, from the
command line:

```bash
mise run token:create -- --name laptop --scope sync
# or: bookmoor-server token create --name laptop --scope sync --state state.json
```

The command refuses to run while a server holds the state file (an OS lock on
`state.json.lock`, released by the system even after a crash; the empty file
itself may stay), asks you to retype the name unless you pass `--yes`, and
needs `--allow-admin` for an admin token. The token is printed once. To
replace a lost admin token, start the server with `BOOKMOOR_ADMIN_TOKEN` set to
a new `bkm_<id>_<64 hex characters>`, or use the command above with
`--scope admin --allow-admin`.

## Contributing

Issues and pull requests are welcome. Run `mise run check` before opening a PR;
it runs what CI runs.

## License

Apache-2.0, see [LICENSE](LICENSE). Do not copy code from Glance/Dynacat
(AGPL-3.0, to be verified); adapters must only use their files and APIs.
