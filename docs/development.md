# Development and releases

Back to the [README](../README.md).

## Build and test

Tools and build commands run through [mise](https://mise.jdx.dev):

```bash
mise install          # Rust and Node
mise run build        # server (target/release), extension/dist (Chrome), extension/dist-firefox
mise run check        # what CI checks
mise run server       # start the server, state in ./state.json
mise tasks            # all tasks
```

Without mise: `cargo test`, and `cd extension && npm ci && npm run build`.
Chrome: load `extension/dist` via `chrome://extensions` > Developer mode > Load
unpacked. Firefox: `about:debugging` > This Firefox > Load Temporary Add-on,
then pick `extension/dist-firefox/manifest.json` (gone after a restart).

### Download a built extension

Every change to the extension on `main` runs the "Extension package" workflow
(Actions tab > latest run > Artifacts): a Chrome zip (unzip it and load the
folder unpacked), an unsigned Firefox zip and, once the repository secrets
`WEB_EXT_API_KEY` and `WEB_EXT_API_SECRET` are set, a signed Firefox `.xpi`
that stays installed across restarts. The version is `0.<run>.<attempt>`.

### Firefox: keep the extension after a restart

A temporarily loaded add-on is gone after a restart. For a permanent install
have Mozilla sign it (free, "unlisted", nothing is published):

1. Create API credentials at <https://addons.mozilla.org/developers/addon/api/key/>
   (JWT issuer and JWT secret). Treat them like a password.
2. Put them into the environment of your shell, not into the repo:
   `WEB_EXT_API_KEY` (issuer) and `WEB_EXT_API_SECRET` (secret).
3. `mise run sign:firefox`. The signed `.xpi` lands in
   `extension/web-ext-artifacts/`. Install it via `about:addons` > gear icon >
   Install Add-on From File.

Each signing needs a new `version` in `extension/manifest.json`. Without
signing, only Firefox Developer Edition, Nightly and ESR can install an
unsigned file (set `xpinstall.signatures.required` to `false`):
`mise run package:firefox` builds the unsigned `.zip`, rename it to `.xpi`.

### Releases and versions

Server, core and extension share one release number `X.Y.Z`. Its single source
is `[workspace.package] version` in `Cargo.toml`; `extension/package.json`,
`package-lock.json` and `manifest.json` are copies that
`node scripts/sync-version.mjs` writes (CI fails if they drift). To release:

1. `node scripts/sync-version.mjs --set 0.11.0 && cargo update --workspace`, open a PR, merge it.
2. Push the tag (`git tag v0.11.0 && git push origin v0.11.0`) or run Actions > **Release** > Run workflow on `main`, which creates the tag. The *Release* workflow checks that CI is green on the commit, that the tag matches `Cargo.toml` and creates the GitHub release page (versions 0.x as *pre-release*, i.e. beta; a pre-release does not move the Docker `latest` tag). The Extension package and Docker workflows build from the tag; they refuse a tag that differs from `Cargo.toml`. A release made in the GitHub UI creates the same tag, so it works too.
3. The release builds the Chrome zip and the signed Firefox `.xpi` with exactly `0.11.0`, submits Chrome to the Web Store (if the secrets are set) and pushes the Docker image as `0.11.0`, `0.11` and, unless it is a 0.x pre-release, `latest`. The tag must be on `main`. If a build fails, re-run it from the Actions page; running Release again does not start the builds a second time (that would try to sign the same version twice).

The script refuses any version not above `0.10.0`: CI signed Firefox builds up to `0.9.1`, and the dev build `0.10.0.13` already sits in the Chrome Web Store, which compares versions numerically (`0.10.0` < `0.10.0.13`) and never accepts a lower one. The first release is therefore `0.11.0` (already set in `Cargo.toml`). Do not upload dev builds of an unreleased version to the store by hand; if you do, raise the floor in `scripts/sync-version.mjs`.

Builds from `main` and manual runs are dev builds numbered `X.Y.Z.<run>` (browsers allow at most four parts); they sort after `X.Y.Z`, so they are signed only while the tag `vX.Y.Z` exists. Between a version bump and its release tag they are built but not signed. Only plain `X.Y.Z` tags are supported (no `-rc` suffixes, browsers reject them).

### Chrome Web Store (unlisted, uploaded by CI)

Chrome only installs extensions from the Web Store (or by policy), so the Chrome equivalent of a signed `.xpi` is an **unlisted** store item: nobody finds it by search, you install it with its link and it updates itself. The `Extension package` workflow uploads every build and submits it for review once these four repository secrets are set (Settings > Secrets and variables > Actions):

| Secret | Where it comes from |
| --- | --- |
| `CWS_EXTENSION_ID` | the item's ID from the Web Store developer dashboard (step 1) |
| `CWS_CLIENT_ID`, `CWS_CLIENT_SECRET` | an OAuth client (step 2) |
| `CWS_REFRESH_TOKEN` | a refresh token for your Google account (step 3) |

1. **Create the item once, by hand.** In the [developer dashboard](https://chrome.google.com/webstore/devconsole) choose *New item* and upload `bookmoor-chrome-<version>.zip` from a workflow run. Fill in the listing (description, a screenshot, category), the privacy tab (it reads bookmarks and talks to your own server; no data goes to the developer) and under *Distribution* set visibility to **Unlisted**. Submit it. The ID (32 letters) is in the item's URL.
2. **OAuth client.** In the [Google Cloud console](https://console.cloud.google.com/) create a project, enable the *Chrome Web Store API*, set up the OAuth consent screen (external, add yourself as test user, then *publish app* so the refresh token does not expire after 7 days) and create an OAuth client ID of type *Desktop app*.
3. **Refresh token.** Open `https://accounts.google.com/o/oauth2/auth?response_type=code&scope=https://www.googleapis.com/auth/chromewebstore&redirect_uri=http://localhost&client_id=<CLIENT_ID>&access_type=offline&prompt=consent`, allow access and copy the `code` parameter from the (failing) `localhost` redirect URL. Then run `curl -s https://oauth2.googleapis.com/token -d client_id=<CLIENT_ID> -d client_secret=<CLIENT_SECRET> -d code=<CODE> -d grant_type=authorization_code -d redirect_uri=http://localhost` and use the `refresh_token` from the answer.

The store upload is not part of every build: it runs for a release (see *Releases and versions*), or when you start the `Extension package` workflow by hand with *Also upload the Chrome build to the Web Store* ticked. Either way it uploads the build's version (the release number `X.Y.Z` for a release) and submits it. A manual run only uploads once the tag `vX.Y.Z` of the current version exists; before that, dev builds are not signed or uploaded. Ordinary pushes to `main` only produce the downloadable artifacts. Review usually takes a few hours to days; the previous version stays live meanwhile. The store keeps one pending submission at a time, so a second upload while one is in review fails the upload step (the zip is still an artifact).
