# Multi-user and web UI

Status: accepted and implemented (model in #35, web UI after it).

## Decisions

- **Separate bookmark trees.** Each user owns one or more profiles (below), and
  each profile has an independent `Engine` (revisions, tombstones, endpoints).
  Nothing is shared between users or between profiles; sharing is a non-goal
  for now.
- **Profiles.** A user has one or more profiles (name unique per user, at most
  20, a user always keeps one). A profile is a separate bookmark tree with its
  own endpoints; profiles share nothing (a shared or filtered tree was weighed
  and rejected: the sync would need to understand subtrees). The `Engine` is
  now per profile, so neither the sync protocol nor the extension changes.
- **Tokens and devices belong to a user and a profile.** A `read` or `sync` token carries a
  `user_id` and a `profile_id` and can only reach that profile's tree. A sync token still maps to
  exactly one endpoint. `admin` tokens belong to no user: they manage users and
  tokens, and have no tree (the server answers 403 on `/v1/tree` and
  `/v1/sync`).
- **Persistence stays one JSON file**, now `{ users, engines: {user_id: ...},
  tokens }`. SQLite was weighed and rejected again: the whole state is rewritten
  atomically, and up to 100 users with personal bookmark sets stay small.
- **Login: local password first, OIDC later.** Passwords are Argon2id hashes in
  PHC format (`users.password_hash`). The web UI uses a session cookie
  (`HttpOnly`, `SameSite=Strict`, `Secure` unless `--insecure-http`), sessions
  live in memory (a restart signs everyone out). Everything behind login goes
  through one function that resolves "who is this request" to a user id, so
  an OIDC or trusted-proxy login can be added later without touching handlers.
- **Roles.** `admin` users manage all users; others manage their own devices
  and tokens. Admin API tokens stay as the break-glass and automation path.

## Profiles: state and API

State: `{ users, profiles, engines: {profile_id: ...}, tokens }`. A file from
the first multi-user version (no `profiles`) becomes one profile `Default`
(id `p_<user id suffix>`) per user, holding the old tree; its tokens move
there. The single-user file migrates the same way via `owner`.

- `POST /v1/tokens` takes `profile` (id or name, within the token's user); it
  may be left out while the user has only one profile.
- Web UI: a Profiles tab (add, rename, remove; removing deletes the profile's
  bookmarks and revokes its devices), a profile choice when adding a device
  (shown once there are two), a profile tag on each device.
- CLI: `token create --profile`; Dynacat: `--dynacat-profile` next to
  `--dynacat-user`. Without it, a user with several profiles shows their
  `Default` one, so adding a profile does not break a running setup. The file
  is only rewritten when that profile changes.

## Migration

A state file of the single-user layout is read as: one user `owner` (admin,
no password) who owns the old tree (as profile `Default`) and all non-admin
tokens. The extension
keeps working unchanged. Set a password with
`bookmoor-server user passwd --name owner --state state.json` (server stopped).
The file is written in the new layout on the next save; going back to an older
server version is not supported afterwards (keep a copy).

Admin tokens never had a device, but they could read `/v1/tree`; they now get
403 there, because no tree is theirs. Use a read token instead.

## API changes

- `POST /v1/tokens` takes `user` (id or name). It may be left out while there is
  exactly one user, so existing callers keep working.
- `GET|POST /v1/users`, `DELETE /v1/users/{id}` (admin token). Deleting a user
  removes their tree and revokes all their tokens.
- Names starting with `u_` are reserved (generated ids look like that).
  `user` is rejected for admin tokens.
- CLI: `user create|passwd`, and `token create --user NAME`.

## Web UI

Served by the server at `/`, no build step, no external resources (strict
CSP). API under `/ui/api/*` with the session cookie, plus a custom request
header on every write as CSRF protection. First setup: while no user has a
password, the page offers a setup form that needs the admin token (it gives
the first admin a password, or the legacy `owner`). Signing in, setup and
password changes share the per-address failed-auth bucket, one token per
attempt. Password hashing runs off the async runtime and outside the state
lock. Admins cannot remove or demote themselves.

## Extension sign-in

`POST /ui/api/connect` (session cookie and `X-Bookmoor-UI`) takes
`{redirect, state, name, profile?}`, issues a new `sync` token for the session's
user (same limit as devices) and returns `{url, device}` where `url` is
`redirect#token=...&state=...`. `redirect` must be `https://<label>.chromiumapp.org/...`
or `https://<label>.extensions.allizom.org/...` (one lowercase label, no port,
userinfo or fragment), so a foreign page cannot obtain a token. The web UI shows
a confirmation page for `/?connect=...&state=...&name=...` after sign-in; the
extension runs this with `identity.launchWebAuthFlow` and checks `state`.
