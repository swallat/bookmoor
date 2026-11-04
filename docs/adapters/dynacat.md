# Adapter requirements analysis: Dynacat

Status: **v0.1 `push` implemented** (`bookmoor-core::dynacat`,
`bookmoor-server --dynacat-file`). Based on the Dynacat docs
(https://dynacat.artur.zone/, UI editor and bookmarks widget); not yet tested
against a running instance.

## How it works

The server writes one file that contains only a `bookmarks` widget and keeps it
current after every change to the canonical tree (and once at startup). The
file is generated from scratch each time, so there is no merge, no entry
identity to keep and nothing for Dynacat's UI editor to fight with. Unchanged
content is not rewritten, so Dynacat does not reload for nothing.

```
bookmoor-server --state /data/state.json --dynacat-file /dynacat/config/bookmoor.yml
# or BOOKMOOR_DYNACAT_FILE=...
```

In the Dynacat config, put the widget wherever it should appear:

```yaml
widgets:
  - $include: bookmoor.yml
```

The file holds the widget itself (`type: bookmarks`, `groups: ...`), the way
Dynacat/Glance include files hold the content of the entry they replace.

Mount the **directory**, not the single file, into the Dynacat container: the
file is replaced by rename, which a single-file bind mount does not follow.

## Mapping

Dynacat groups are flat (a title, optional color, a list of links).

| Bookmoor | Dynacat |
|---|---|
| bookmarks directly under the root | one leading group without title |
| folder | group titled with its path, e.g. `Bookmarks Bar / Dev` |
| subfolder | its own group, listed after its parent (depth first) |
| bookmark | link with `title` and `url` |
| empty folder, bookmark without http(s) URL | left out |
| bookmark without title | URL used as title |

Icons, descriptions, colors and the exact folder nesting are not represented.
They are not lost in Bookmoor, only not shown in Dynacat.

## Known from the docs
- A UI editor writes changes back into the YAML file the page came from,
  keeping comments and `$include` lines. Blank lines between entries are lost.
- Pages can live in their own files, linked with `$include`.
- Editing can be disabled (`ENABLE_EDITOR`, `allow-editing`) or restricted.
- The config is reloaded automatically after a change.
- Bookmarks widget: `groups[]` with optional `title`/`color`, each with
  `links[]` (`title`, `url` required; `description`, `icon`, `same-tab`,
  `hide-arrow`, `target` optional). No nesting.

## To verify (run a local instance)
1. Does `- $include:` accept a file that holds a single widget mapping?
2. Does the UI editor refuse or corrupt the included widget file if someone
   edits the widget in the browser (it writes to the included file, which
   Bookmoor then overwrites)?
3. Reload latency after the file is replaced.

## Later (v0.2 `both`)
Reading changes made in Dynacat back needs identity: whether Dynacat accepts an
extra `id:` field on links or keeps a trailing `# id:` comment through the UI
editor is still open. Until then the widget file is output only.
