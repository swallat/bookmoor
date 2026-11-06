# Linkwarden (two-way)

Status: **implemented** (`bookmoor-core::linkwarden` for the comparison,
`bookmoor-server::linkwarden` for the HTTP client and the runner). The API
calls follow Linkwarden's source (`/api/v1/collections`, `/api/v1/search` with
`/api/v1/links` as fallback, `/api/v1/links/{id}`); the tests use a fake
Linkwarden over real HTTP. **Not yet tried against a real instance.**

## Set up

Web UI, **Linkwarden** tab (admins only, because the server makes the
requests with a stored token):

1. In Linkwarden create an access token (Settings, Access Tokens).
2. In bookmoor pick a profile and, if you like, one folder of it, enter the
   Linkwarden address, the token and the name of the collection to use.
3. Pick a direction (both ways by default) and how often Linkwarden is checked
   for changes (every 5 minutes by default).

The address and token are tried when you save. The first sync starts at once.
The token is kept in the state file like the other secrets (see
[deployment](../deployment.md)), the web UI never shows it again.

## What maps to what

| Bookmoor | Linkwarden |
|---|---|
| the profile (or the chosen folder) | the collection you name (made if missing) |
| a folder | a collection inside it, nested like the folders |
| a bookmark | a link (title is the name, address is the URL) |
| order | not synced; new links from Linkwarden are appended |
| tags, notes, archives, pins | stay as they are in Linkwarden, never overwritten |

A profile's top-level folders `toolbar`, `other` and `mobile` become
collections like any other. They are the browser's own folders, so Linkwarden
cannot rename, move or delete them; the next run puts the collection right.
Bookmarks whose address Linkwarden cannot hold (spaces, no scheme) are left
out and counted on the connection's status line. Links without an address
(PDF, image) are not brought into Bookmoor.

## How changes are merged

After every run both sides' state is remembered. The next run compares each
item's title, address and folder with that state, field by field:

- changed on one side only: the other side follows;
- changed on both sides: **Bookmoor wins** (same as an edit on one device
  against another: the server tree is the truth);
- deleted on one side and unchanged on the other: deleted there too;
- deleted on one side and **edited on the other: the edit wins**, the item
  comes back (a folder counts as edited when anything new or changed is in it).

On the first run (or when the pairing is lost), a folder or link that both
sides have, in the same place and with the same name or address, is paired
instead of doubled.

Changes in Bookmoor go over a few seconds after a sync; changes in Linkwarden
are picked up at the configured interval and by **Sync now**. Something taken
over from Linkwarden reaches the browsers through the normal sync. Items
Bookmoor removes because of Linkwarden are saved as the profile's backup
(**Restore** on the Profiles tab).

## Directions

- **Both ways** (default): as above.
- **Bookmoor to Linkwarden**: Linkwarden is made to match for everything that
  is paired (edits there are overwritten); links only Linkwarden has are left
  alone.
- **Linkwarden to Bookmoor**: the mirror image; bookmarks only Bookmoor has
  are left alone.

## Safety

- A run stops with "Paused" when it would delete more than half of the (at
  least 10) items it knows, or when one side has become empty while at least 3
  items are known (an emptied profile, a collection or listing that came back
  empty). **Sync anyway** goes ahead.
- Bookmarks whose address one side cannot hold are left out, and the copy on the
  other side is neither deleted nor rewritten for that.
- Edits made in Bookmoor while a run is going are never overwritten: the run
  then only adds, and the rest follows in the next one.
- The stored token is only sent to the address it was saved with; changing the
  address asks for the token again.
- If the collection is gone from Linkwarden (or the token cannot see it),
  nothing is changed and the connection reports it.
- A change that fails (network, a refusal from Linkwarden) stays pending and
  is tried again; the status line says what failed.
- Removing a connection leaves everything as it is on both sides.
- At most 10 connections; two cannot use the same collection of the same
  Linkwarden.

## Not covered

- Tags are neither read nor written, new links get none.
- Shared collections owned by another user may refuse edits.
- No push from Linkwarden (it has no webhook for this), so changes there wait
  for the interval.
