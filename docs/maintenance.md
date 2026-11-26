# Automated maintenance

Dependencies and security fixes are looked after without anyone watching.
This page says what runs, what is merged on its own and what always waits
for a person.

## Renovate

Configuration: [`renovate.json`](../renovate.json).

- Updates are raised on weekday nights (00:00 to 06:00, Europe/Berlin), at
  most 5 PRs open at once. A release must be 3 days old first. Security fixes
  are raised at once and skip that wait.
- Rust crates, npm dev tools and GitHub Actions are grouped. Lock file
  maintenance runs on Monday night.
- Renovate merges a PR itself once every check on it is green.
- **Merged on their own:** patch and minor updates, pins and digests, lock
  file maintenance, and security fixes of packages that are not listed below.
- **Never merged on their own** (label `needs-review`):
  - every major update (label `major`);
  - minor updates of `0.x` packages;
  - language runtimes, toolchains and base images (`node`, `rust`, `debian`),
    and everything in the `Dockerfile`, the compose files and `mise.toml`;
  - minor updates of the core libraries: the auth and crypto crates
    (`argon2`, `openidconnect`, `p256`, `sha2`, `sha1`, `hmac`, `subtle`,
    `getrandom`, `ciborium`), `axum`, `tokio`, `schemars`, and the extension
    build chain (`typescript`, `esbuild`, `web-ext`). Their patch and
    security updates do merge;
  - the actions that handle registry credentials: `docker/login-action`,
    `docker/build-push-action`.

## Vulnerability scan

[`vulnerability-scan.yml`](../.github/workflows/vulnerability-scan.yml) runs
`cargo audit` and `npm audit` (high and above) every Sunday night. One open
issue labelled `vulnerability` lists the findings; a clean run closes it. It
is not a required check.

## Weekly Claude task

Every Monday morning a scheduled Claude task in the cloud
(`Softwarepflege bookmoor`):

1. Looks at open PRs by `renovate[bot]`, at open issues labelled
   `vulnerability` or `security-release`, and at security fixes that are
   merged but not released. If there is nothing, it ends at once.
2. Repairs red checks on those PRs.
3. Merges non-major updates after an independent review and green CI.
4. Prepares major updates, but does not merge them, and lists them in its
   report.
5. Decides on a security release, see below.

It touches no other PRs, never force-pushes and changes no settings. CI can
be started by hand with the `workflow_dispatch` trigger of `ci.yml`.

## Security releases

When pull requests labelled `security` were merged after the latest release,
the weekly task decides whether to publish them:

- It releases only when the fix touches shipped code (the Cargo files, the
  `Dockerfile`, the extension sources; not build, test or CI tools), and when
  everything on `main` since the latest release is dependency updates,
  maintenance and bug fixes.
- It opens a pull request `Security release vX.Y.Z` that raises the patch
  version (`node scripts/sync-version.mjs --set`, `cargo update --workspace`),
  and merges it after an independent review and green CI.
- It then starts [`release.yml`](../.github/workflows/release.yml) on `main`.
  That workflow creates the tag, the release page and the builds, so stores
  and Docker only ever see a real release tag.
- It checks afterwards that the release is published.
- In any other case, or when in doubt, it publishes nothing and opens an
  issue `Security release needs a decision` (label `security-release`).

At most one security release per run. Every other release stays a decision
of the maintainer.

## Repository settings

Dependabot alerts on (Renovate reads them), Dependabot security updates off,
and the required checks in the ruleset for `main` (Rust, Extension and the
E2E checks). GitHub's own auto-merge
is not used. The repository secret `RELEASE_BOT_TOKEN` is no longer needed.

## Deviations from the standard

None.
