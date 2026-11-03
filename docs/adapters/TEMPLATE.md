# Adapter requirements analysis: <name>

Fill this in before writing the adapter. Record how each answer was verified
(version tested, date, what was done).

## Storage
- Where does the data live (file, API, database)? Who may write?
- Can the target reload on change, or does it need a restart?

## Edit support
- Does the target have an edit UI that writes back?
- Does the edit UI preserve unknown fields, comments and `$include`/anchor syntax?
- What formatting does it change (blank lines, key order, quoting)?

## Data model fit
- Maximum folder nesting. Supported fields (title, url, icon, tags, ...).
- What cannot be represented and must be preserved in the canonical tree.

## Change detection
- File watcher, polling or API events? How long does a reload take?

## Identity strategies (test each, in order)
| Strategy | Accepted by target? | Survives edit UI? | Survives reorder/remove? |
|---|---|---|---|
| field | | | |
| comment (trailing) | | | |
| fingerprint | always | n/a | loses renames and moves |

## Known limits
