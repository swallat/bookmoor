# Adapter requirements analysis: Dynacat

Status: **open**. Based on https://dynacat.artur.zone/ui-editor (docs only,
nothing tested against a running instance yet).

## Known from the docs
- A UI editor writes changes back into the YAML file the page came from,
  keeping comments and `$include` lines. Blank lines between entries are lost.
- Pages can live in their own files, linked with `$include`.
- Editing can be disabled (`ENABLE_EDITOR`, `allow-editing`) or restricted.
- The config is reloaded automatically after a change.

## To verify (run a local instance)
1. Does a `bookmarks` widget in an included file get edited by the UI editor,
   and in which file does the editor write?
2. Does an extra `id:` field on a link get accepted (strict parsing?) and does
   the editor keep it?
3. Does a trailing `# id:...` comment stay on the right entry after moving and
   deleting entries in the UI?
4. How does the widget represent groups and nesting (maximum depth, fields)?

Then fill in the identity-strategy table from `TEMPLATE.md`.
