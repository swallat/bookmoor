# Deployment and operations

Everything you need to run bookmoor behind a reverse proxy: sign-in methods, single sign-on, proxy setups, users and tokens. Back to the [README](../README.md).

## Docker

A multi-stage image is published to `ghcr.io/swallat/bookmoor` (`edge` follows
`main`; release tags `vX.Y.Z` give `latest`, `X.Y.Z` and `X.Y`; pre-releases do not move `latest`). It runs as a
non-root user, keeps its state in the volume `/data` (`/data/state.json`) and
listens on `0.0.0.0:8787` with `--insecure-http`, because the server refuses a
non-loopback address without TLS in front. **Always put a reverse proxy with
TLS in front of it** and do not publish the port directly.

```bash
cp docker-compose.example.yml docker-compose.yml
docker compose up -d
docker compose logs bookmoor | grep 'admin token'   # shown once on first start; or set BOOKMOOR_ADMIN_TOKEN
```

Commands run in the same image, with the server stopped:

```bash
docker compose stop bookmoor
docker compose run --rm -T bookmoor token create --name laptop --scope sync --user anna --yes
echo 'a long password' | docker compose run --rm -T bookmoor user create --name anna --admin
docker compose start bookmoor
```

For the Dynacat adapter, mount a writable directory (uid 10001) and point
`BOOKMOOR_DYNACAT_OUTPUTS` at files in it (one `file=PATH[;profile=NAME][;folder=A/B/C]`, plus optional look settings,
per line, see [adapters/dynacat.md](adapters/dynacat.md)); mount the same
directory into Dynacat. The example compose file shows both. Build locally with
`docker build -t bookmoor .`. If a command overrides `--bind` with another port, the
image's healthcheck (port 8787) reports unhealthy.

## Signing the extension in

Instead of pasting a token, press "Sign in with bookmoor" in the extension
settings (server URL first). The browser opens the server's page in an auth
window: sign in to the web UI (and to a proxy login in front of it, if any),
pick the device name and profile, and confirm. The server creates a **new**
`sync` token for that device on every sign-in (revoke old ones on the Devices
tab) and returns it in the URL fragment of the extension's redirect address,
so it reaches no log. The server only redirects to extension addresses
(`*.chromiumapp.org`, `*.extensions.allizom.org`). The extension needs the
`identity` permission for this; pasting a token still works.

## Single sign-on (OIDC)

The web UI can sign users in through any OpenID Connect provider (Authentik,
Keycloak, Authelia, Pocket ID, Google, ...), with the authorization code flow
and PKCE. Register the redirect URL `<public URL>/ui/api/oidc/callback` with
the provider, then start the server with:

```bash
BOOKMOOR_PUBLIC_URL=https://bookmoor.example.com   # where users reach the web UI
BOOKMOOR_OIDC_ISSUER=https://id.example.com/application/o/bookmoor/
BOOKMOOR_OIDC_CLIENT_ID=bookmoor
BOOKMOOR_OIDC_CLIENT_SECRET=...                    # optional for public clients
BOOKMOOR_OIDC_LABEL="Authentik"                    # button text (default "single sign-on")
BOOKMOOR_OIDC_CREATE_USERS=1                       # optional, see below
```

(or `--public-url`, `--oidc-issuer`, `--oidc-client-id`, `--oidc-label`,
`--oidc-create-users`; the secret is only read from the environment.) The
provider is contacted on first use, so the server starts while it is down.
Provider certificates are checked against the built-in public roots.

**Or in the web UI:** admins can set the same things up on the "Single sign-on"
tab (issuer, client ID and secret, public address, button text, whether to
create users), test the connection, and copy the redirect URL for the
provider. Changes apply at once, no restart. The client secret is saved in the
state file (readable by the server's user only) and never shown again; leave
the field empty to keep it. If the environment variables or flags above are
set, they win: the tab then shows them read-only, and settings saved earlier
are kept but unused until the variables are removed.

- **Linking:** a user signs in with the password once and presses "Link ..."
  under Account. From then on the provider's stable user id (`sub`) signs them in,
  never the name. Unlinking needs a password, so nobody locks themselves out.
- **Creating users:** with `--oidc-create-users` an unknown identity gets a new
  non-admin user on first sign-in (name from `preferred_username` or the email
  prefix, made unique). Without it, an unlinked identity is refused. Anyone who
  can sign in at the provider can create a user, so restrict the application
  there.
- **First admin:** unchanged, the setup form with the admin token; link single
  sign-on to that admin afterwards.
- With the same provider in front of the web UI (Pangolin, NetBird and
  Cloudflare Access can use OIDC providers) the second sign-in runs without
  typing anything. The extension's "Sign in with bookmoor" works with it too.

## Client certificates (mTLS)

Optionally the server can require a client certificate in addition to the
token. TLS ends at your reverse proxy, so the proxy checks the certificate
against your client CA and tells the server the result in a header. With
`--client-cert-header NAME[=VALUE]` (or `BOOKMOOR_CLIENT_CERT_HEADER`) every request
except `/health` must carry that header with the expected value, otherwise it
gets 403 before the token is looked at. `NAME` alone expects `SUCCESS`;
`NAME=*` accepts any value except empty, `NONE`, `0` and `FAILED...` (for proxies that set the header only for a
verified certificate).

nginx:

```nginx
ssl_client_certificate /etc/nginx/client-ca.pem;
ssl_verify_client optional;
location / {
    proxy_set_header X-SSL-Client-Verify $ssl_client_verify;  # always overwritten
    proxy_pass http://bookmoor:8787;
}
```

```bash
bookmoor-server --client-cert-header X-SSL-Client-Verify
```

The proxy must set the header on every request (so a client cannot send it
itself) and the server port must not be reachable around the proxy. Browsers
and the extension present the certificate from the browser or OS certificate
store, so install it there; Chrome asks once which certificate to use.

### Pangolin

Pangolin can use a client certificate as the only login of a resource: enable
"Client Certificate (mTLS)" on the Bookmoor resource, upload your CA, and turn
the other methods (SSO, PIN) off. The extension then reaches Bookmoor without a
Pangolin login; the Bookmoor token stays required. Pangolin checks the
certificate in the TLS handshake and forwards its details in
`X-Forwarded-Tls-Client-Cert-Info`, so this also works as the extra check above:

```bash
BOOKMOOR_CLIENT_CERT_HEADER='X-Forwarded-Tls-Client-Cert-Info=*'
```

Install the client certificate (PKCS#12) in the browser or OS store. If the
extension's requests fail with a TLS error because no certificate prompt
appears for background requests, select the certificate automatically (Chrome:
the `AutoSelectCertificateForUrls` policy; Firefox: "Select one automatically"
under Settings > Privacy & Security > Certificates). Not tested in a real browser.

### Pangolin SSO (browser login)

Instead of a certificate you can keep Pangolin's normal login (Platform SSO or
your OIDC provider) in front of Bookmoor, without shareable links. In the
extension settings, press "Sign in with Bookmoor": the sign-in window passes
Pangolin's login and Bookmoor's, and Pangolin's resource-session cookie (30 days
by default) is sent with every request afterwards. There is no setting for it;
the extension notices a login in front of the server by itself.

When the session ends (expiry, logout, revocation by the identity provider),
the sync stops with "Sign in again", the toolbar icon shows a `!`, and the
popup and settings offer a **Sign in** button. It opens the server URL in a new
tab; closing that tab after the login makes the extension sync again. Changes
made meanwhile stay queued locally. Not tested in a real browser yet (cookie
handling for extension requests differs between Chrome and Firefox).

### NetBird

NetBird's reverse proxy has no client-certificate authentication. Options, best first:

- **NetBird-Only access:** the extension's device is a NetBird peer, so there is
  no login at all. Needs a self-run proxy cluster (see NetBird's docs).
- **Header authentication with a custom header:** create a header entry such as
  `X-Proxy-Key` on the service and add the same name and value under "Extra
  headers" in the extension. Do not use the `Authorization` presets: the proxy
  strips the matching header, so the Bookmoor token would never arrive.
- **SSO:** just sign in with Bookmoor (see Pangolin SSO above).
  NetBird sessions last 24 hours, so expect a sign-in about once a day.

### Cloudflare Tunnel with Cloudflare Access

Put a Cloudflare Access application in front of the tunnel hostname.

- **Service token:** create a service token with a policy of action "Service
  Auth" and add `CF-Access-Client-Id` and `CF-Access-Client-Secret` under
  "Extra headers". No browser login, and it works with the extension as it is.
- **Browser login:** with an Allow policy and your identity provider, sign in
  with Bookmoor in the extension. Cloudflare answers with a redirect to its
  login, which the extension reports as "Sign in again"; the session length is
  set on the Access application. Not tested in a real browser; Cloudflare's
  cookie is sent to the app domain, so browsers that block third-party cookies
  may need an exception.

In both cases the Bookmoor token stays required.
### Log

The server writes one line per request to stderr: time (UTC), client address,
token name (`-` if unauthenticated), method, path, status and duration. Query
strings are left out because proxy setups may carry secrets there; `/health`
is not logged. Behind a reverse proxy the address is the proxy's.

### Web UI

The server serves a small admin page at `/` (behind your reverse proxy at the
same path prefix; links are relative). Sign in with a user name and password to
create and revoke device tokens (the token is shown once), change your password
and, as an admin, add, promote and remove users. On a fresh install the page
asks for the admin token once to create the first admin; after an upgrade use
the name `owner` there to keep your bookmarks. Sessions last 7 days and live in
memory, so a restart signs everyone out. The session cookie is `HttpOnly`,
`SameSite=Strict` and `Secure` unless `--insecure-http` is set; pages are
served with a strict Content-Security-Policy and no external resources.

### Users

Every user has their own bookmark tree; devices and tokens belong to a user
(see [multi-user.md](multi-user.md)). Older single-user state files
become the user `owner`. A fresh install has no user: start the server once (it creates the state file
and prints the admin token), stop it, and run `user create` before the first
`token create`; or create the user with the admin token while it runs. Users are managed with an admin token
(`POST /v1/users`, `GET /v1/users`, `DELETE /v1/users/{id}`) or, with the
server stopped:

```bash
echo 'a long password' | bookmoor-server user create --name anna --admin --state state.json
echo 'a long password' | bookmoor-server user passwd --name anna --state state.json
```

### Tokens

The extension needs a **device token** (scope `sync`), one per browser. The
admin token only manages tokens and has no device, so the server answers 403
when it is used for syncing. The extension options show the last sync status.

The first start with a new state file prints an admin token once. Only a hash
is stored, so a lost token cannot be shown again. Device tokens come from
`POST /v1/tokens` (admin token required) or, with the server stopped, from the
command line:

```bash
mise run token:create -- --name laptop --scope sync --user anna --profile Work
# or: bookmoor-server token create --name laptop --scope sync --state state.json
```

The command refuses to run while a server holds the state file (an OS lock on
`state.json.lock`, released by the system even after a crash; the empty file
itself may stay), asks you to retype the name unless you pass `--yes`, and
needs `--allow-admin` for an admin token. The token is printed once. To
replace a lost admin token, start the server with `BOOKMOOR_ADMIN_TOKEN` set to
a new `bkm_<id>_<64 hex characters>`, or use the command above with
`--scope admin --allow-admin`.

