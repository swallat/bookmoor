# Multi-user and web UI

Status: accepted (PR 1 implements the model, PR 2 the web UI).

## Decisions

- **One bookmark tree per user.** Each user owns an independent `Engine`
  (revisions, tombstones, endpoints). Nothing is shared between users; sharing
  is a non-goal for now.
- **Tokens and devices belong to a user.** A `read` or `sync` token carries a
  `user_id` and can only reach that user's tree. A sync token still maps to
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

## Migration

A state file of the single-user layout is read as: one user `owner` (admin,
no password) who owns the old tree and all non-admin tokens. The extension
keeps working unchanged. Set a password with
`bookmoor-server user passwd --name owner --state state.json` (server stopped).
The file is written in the new layout on the next save; going back to an older
server version is not supported afterwards (keep a copy).

## API changes

- `POST /v1/tokens` takes `user` (id or name). It may be left out while there is
  exactly one user, so existing callers keep working.
- `GET|POST /v1/users`, `DELETE /v1/users/{id}` (admin token). Deleting a user
  removes their tree and revokes all their tokens.
- CLI: `user create|passwd`, and `token create --user NAME`.

## Web UI (PR 2)

Served by the server at `/`, no build step, no external resources (strict
CSP). API under `/ui/api/*` with the session cookie, plus a custom request
header on every write as CSRF protection. First setup: while no user has a
password, the page offers a setup form that needs the admin token.
