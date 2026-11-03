# Architecture

## Components

1. **Extension** (TypeScript, WebExtension standard). Reads and writes the
   browser's bookmarks, keeps a map browser-id <-> server-id.
2. **Server** (Rust). Holds the canonical tree with stable ids and a revision
   counter, runs the sync engine and the adapters.
3. **Adapters.** Bidirectional endpoints for tools with their own edit UI.

## Sync model

- The server stores, per endpoint, the **last synced state** (base).
- On a change the engine diffs endpoint vs. base, merges the diff into the
  canonical tree (three-way) and fans the result out to the other endpoints.
- Deletions are kept as tombstones so "deleted here" and "added there" differ.
- Per endpoint direction: `push`, `pull` or `both`.
- Conflict strategy per endpoint: `prefer-browser`, `prefer-adapter`, `newest`.
- When the engine writes a file it remembers the hash, so the resulting watch
  event is not treated as an external change (loop prevention).

## Identity of entries in text files

Target files (YAML) usually have no stable ids. Each adapter lists which of
these strategies survive its target's edit UI. The user-configured order
defaults to:

1. **field**: a dedicated data field (`id: bk_...`)
2. **comment**: a trailing same-line comment (`# id:bk_...`). Comments on the
   line above an entry are orphaned when the entry is removed, so they are
   not used.
3. **fingerprint**: group path + title + URL. Always available, cannot follow
   renames or moves.

IDs are an optimisation, not a requirement. Entries without id (for example
added via the target's UI) are matched by fingerprint and receive an id on the
next write.

## Authentication

- Outer layer: reverse proxy (e.g. Pangolin header auth). The extension sends
  user-configured extra headers.
- Inner layer: per-device API tokens in the server (only hashes stored),
  optionally scoped.

## Plugin model

Stage 1: built-in adapters behind the `Adapter` trait, enabled in the server
config. Stage 2: a generic exec/webhook adapter as an escape hatch. Stage 3
(only if there is demand): WASM plugins.

## Milestones

- **v0.1** core engine (three-way merge), Chrome extension, Dynacat adapter (`push`).
- **v0.2** Dynacat `both` (watch + read), conflict strategies.
- **v0.3** Firefox, Dashy, exec adapter.
