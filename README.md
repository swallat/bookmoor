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

```bash
cargo test
```

```bash
cd extension && npm install && npm run build
```

Then load `extension/dist` via `chrome://extensions` > Developer mode > Load unpacked.

## License

Apache-2.0, see [LICENSE](LICENSE). Do not copy code from Glance/Dynacat
(AGPL-3.0, to be verified); adapters must only use their files and APIs.
