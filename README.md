# bookmoor

Self-hosted bookmark sync: a browser extension plus a small server that keeps
your bookmarks in sync with the dashboards and tools you actually use.

> Status: **scaffold (v0.0)**. Nothing is usable yet.

## Idea

```
Chrome / Firefox extension  <->  bookmoor server (canonical tree)  <->  adapters
                                                                      (Dynacat, Dashy, ...)
```

- Every endpoint, browser or adapter, is a peer. The server merges changes
  with a three-way diff against the last synced state.
- Adapters talk to tools that have their own edit UI (Dynacat, Dashy), so
  sync works in both directions.
- Authentication has two layers: an optional reverse-proxy layer (custom
  headers, e.g. Pangolin header auth) and bookmoor's own per-device API tokens.

See [docs/architecture.md](docs/architecture.md) and [docs/sync-architecture.md](docs/sync-architecture.md).

## Layout

| Path | Content |
|---|---|
| `crates/bookmoor-core` | Data model, adapter trait, identity strategies, diff (Rust) |
| `crates/bookmoor-server` | The self-hosted server (Rust, scaffold only) |
| `extension/` | WebExtension in TypeScript (Chrome first) |
| `docs/` | Architecture, ADRs, per-adapter requirements analyses |

## Development

Tools and build commands run through [mise](https://mise.jdx.dev):

```bash
mise install          # Rust and Node
mise run build        # server (target/release) and extension (extension/dist)
mise run check        # what CI checks
mise run server       # start the server, state in ./state.json
mise tasks            # all tasks
```

Without mise: `cargo test`, and `cd extension && npm ci && npm run build`.
Load `extension/dist` via `chrome://extensions` > Developer mode > Load unpacked.

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

## License

Apache-2.0, see [LICENSE](LICENSE). Do not copy code from Glance/Dynacat
(AGPL-3.0, to be verified); adapters must only use their files and APIs.
