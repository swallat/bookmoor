# ADR 0001: Rust for the server

Status: accepted

## Context
The adapters edit YAML files that users and the target tools' UIs also edit.
Comments and formatting must survive, so a lossless YAML editor was the key
risk for choosing Rust.

## Evidence
`yaml-edit` 0.3.6 (Apache-2.0, based on `rowan`) was tested against a
bookmarks-like document:

- Unmodified round trip is byte-identical (also with CRLF line endings).
- Changing fields, adding an `id:` field (also to flow-style `{ ... }`
  entries), appending and removing entries keep the surrounding comments
  and blank lines.
- Trailing same-line comments travel with their entry on removal; comments
  on the line above stay behind. Identity-in-comment therefore only uses
  trailing comments.
- Anchors, aliases and tags such as `!include` parse and write back unchanged.
- Minor: removing an entry can leave an extra blank line.

## Risks
`yaml-edit` is pre-1.0 with a single main maintainer. Mitigation: wrap it in
our own small trait and keep golden-file tests per adapter using real
configs of the target tools.

## Alternatives
TypeScript (mature `yaml` library, shared code with the extension) and Go
(`yaml.v3` node API) were not tested.
