# Karakeep (two-way)

Status: **implemented**, sharing the comparison and the runner with
[Linkwarden](linkwarden.md) (`bookmoor-core::linkwarden`,
`bookmoor-server::linkwarden`); `bookmoor-server::karakeep` holds the HTTP
client and the Karakeep-specific reading and writing. The calls follow
Karakeep's OpenAPI description (`/api/v1/lists`, `/lists/{id}/bookmarks`,
`/bookmarks`); the tests use a fake Karakeep over real HTTP. **Not yet tried
against a real instance.** A first run in the direction "Bookmoor to Karakeep"
against a test list is the careful way to start.

## Set up

Web UI, **Karakeep** tab (admins only, because the server makes the requests
with a stored key):

1. In Karakeep create an API key (Settings, API Keys).
2. Pick a profile (and optionally one folder of it), enter the Karakeep address,
   the key and the name of the list to use (made if missing).
3. Pick a direction and how often Karakeep is checked (every 5 minutes by
   default).

The address and key are tried when you save. The key is kept in the state file
like the other secrets and never shown again. A profile can also be connected
to Linkwarden at the same time.

## What maps to what

| Bookmoor | Karakeep |
|---|---|
| the profile (or the chosen folder) | the list you name |
| a folder | a list inside it, nested like the folders |
| a bookmark | a link bookmark in that list (title and address) |
| order | not synced; new bookmarks from Karakeep are appended |
| tags, notes, archives, favourites | stay as they are in Karakeep, never written |

How the merge works (field by field, Bookmoor wins when both changed, an edit
beats a delete, first-run matching by place and name/address), the directions
and the safety rules are the same as for Linkwarden.

## Where Karakeep differs

- **One bookmark per address.** Karakeep does not keep two bookmarks with the
  same address. A second bookmark in Bookmoor with an address that is already
  synced stays out (counted on the status line). If an address is already a
  bookmark in Karakeep outside the synced lists, it is put in the new list as
  well; if it is already in another synced list, the second copy stays out.
- **A bookmark can be in several lists.** Bookmoor's tree puts it in one place.
  Among the synced lists, the one it was last synced in counts (otherwise the
  first one). Moving it in Bookmoor takes it out of its other synced lists.
  Lists outside the bound list are never touched.
- **Deleting a bookmark in Bookmoor only takes it out of the synced lists.**
  The bookmark stays in Karakeep with its tags, notes and archive.
  Deleting a list does delete the list.
- **Only web addresses** (`http://` and `https://`) are synced; `chrome://`,
  `javascript:` and similar bookmarks stay out. Notes and uploaded files in
  Karakeep are not brought into Bookmoor; smart lists are ignored.
- **Titles:** a bookmark's title is the one set in Karakeep, otherwise the
  crawled page title, otherwise the address. List names are up to 100
  characters, titles up to 1000.
- Karakeep's text ids are mapped to numbers for the comparison; the table is
  part of the connection in the state file.

## Not covered

- Tags are neither read nor written.
- Lists shared with you by another user may refuse edits.
- No push from Karakeep, so changes there wait for the interval.
