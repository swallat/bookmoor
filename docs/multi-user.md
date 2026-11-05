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
- **Login: local password, plus OIDC (single sign-on).** Passwords are Argon2id hashes in
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

## Single sign-on

`src/oidc.rs`, endpoints under `/ui/api/oidc`. Configured by
`--public-url/--oidc-issuer/--oidc-client-id` (env `BOOKMOOR_*`; the client
secret only from `BOOKMOOR_OIDC_CLIENT_SECRET`). A user may carry
`oidc: {issuer, subject}`; sign-in finds the user by that pair only. Flow:
`GET /ui/api/oidc/start[?link=1][&return=?query]` stores state, nonce and PKCE
verifier in memory (10 minutes, one use, bound to the browser by an `HttpOnly`
`SameSite=Lax` cookie scoped to `/ui/api/oidc`) and redirects to the provider;
`GET /ui/api/oidc/callback` checks all of that and the ID token (signature,
issuer, audience, expiry, nonce), then sets the normal session cookie and
redirects to `../../<return>` (relative, so a proxy path prefix works). `link=1`
requires a session and attaches the identity to that user. Errors come back as
`?sso_error=<code>`. `return` may only be a bare query string, which carries the
extension's `/?connect=...` request across the round trip. `POST
/ui/api/oidc/unlink` needs a password on the account. Unknown identities are
refused unless `--oidc-create-users` is set.

Admins can also set the provider up in the UI: `GET/PUT/DELETE /ui/api/sso` and
`POST /ui/api/sso/test` (discovery only, nothing saved) take `issuer`,
`client_id`, `client_secret`, `public_url`, `label`, `create_users`. The
config is saved as `oidc` in the state file (the secret in clear text, file
mode 0600; the API never returns it, an empty one keeps it, and it carries
over only to the same issuer and client ID) and applied live; flows in
progress end. Environment/flags win: `oidc_env` makes PUT/DELETE answer 422
and the saved config stays unused.

## Editing users and two-factor authentication

- **Admins edit users** in the Users tab: rename, set a password, make admin,
  disable or enable (`POST /ui/api/users/{id}/edit`), remove. A disabled user
  cannot sign in, their web sessions end and their device tokens answer 401;
  nothing is deleted, so enabling brings everything back. Admins cannot
  disable themselves.
- **TOTP for the password login** (RFC 6238: SHA-1, 6 digits, 30 s, one step
  of clock drift). Users set it up in the Account tab (secret to type in or an
  `otpauth://` link; no QR code, because the strict CSP allows no library) and
  confirm with a code. They get 10 single-use recovery codes, shown once and
  stored as SHA-256 hashes. A code works once (replay guard). The secret is
  kept in the state file in clear, because it must be readable to verify a
  code: protect the file like the tokens.
- **Login:** a correct password with an authenticator on answers
  `{"state":"mfa","ticket":...}` and no cookie; `POST /ui/api/login/mfa` with
  ticket and code finishes it. A ticket lives 5 minutes and takes 5 tries.
- **Admins can require it** per user. Until the user has set it up, their
  session may only call `/ui/api/mfa/*` and log out, and they cannot turn it
  off again. An admin can **reset** it for another user (lost phone): the
  authenticator and recovery codes go and their sessions end.
- **Single sign-on** logins skip all this; the identity provider is the second
  factor. Device tokens are unaffected (they are long random secrets).

## Passkeys (WebAuthn)

- **Two uses.** A passkey signs in without a password ("Sign in with a
  passkey" on the login page, discoverable credentials, user verification
  required), and it works as the second step after a password, next to or
  instead of an authenticator app. A passkey already proves presence and
  identity (PIN, fingerprint, face), so the passwordless login asks for no
  further factor, even when an admin requires two-factor authentication. A
  passkey satisfies that requirement.
- **Account tab:** add (needs the password; users without one, such as
  single sign-on users, need a session younger than 15 minutes), rename,
  delete (the last one cannot go when it is the user's only way in, or when
  an admin requires a second factor and there is no authenticator app). Up to 10 per user. Admins can **reset** all passkeys of
  another user in the Users tab; their sessions end.
- **No configuration.** The relying party id is the host name of the `Origin`
  header the browser sends, so it works behind any reverse proxy. Passkeys
  need https (or `localhost`) and a host name, not an IP address, and only
  work on the host they were made for: moving the server to another host name
  means adding them again. Each passkey stores that host and is checked against it.
- **Verification** is built in (no extra service): ES256 only (what platform
  authenticators and security keys use), attestation `none`, the challenge is
  random, single-use and lives 5 minutes, origin and relying party hash are
  checked. The signature counter is stored but not enforced (synced passkeys report 0).
  The state file keeps the credential id, public key, name and counter; there
  is no secret in it.
- **API** (all under `/ui/api`): `GET passkeys`, `POST passkeys/begin` and
  `passkeys/finish`, `POST passkeys/{id}/rename`, `DELETE passkeys/{id}`,
  `POST login/passkey/begin` and `finish`, `POST login/mfa/passkey/begin` and
  `finish` (with the login ticket), `POST users/{id}/passkeys/reset` (admin).
