# Releasing Epazote

Epazote uses `just release`: one manually dispatched
[release.yml](workflows/release.yml), shared [local actions](actions),
and the operator's [.justfile](../.justfile) recipe. The released commit is exactly the
commit that passed `CI OK`; all uploaded files are built in that release run.
The handwritten [CHANGELOG.md](../CHANGELOG.md) remains maintained; generated commit-subject
GitHub release notes supplement it.

## Release contract

- Release only `develop`, with an untagged plain `X.Y.Z` version greater than every
  existing release-version tag. `origin/main` must be an ancestor of that commit.
- A completed successful check named exactly `CI OK` must exist on that exact SHA.
  It aggregates tests, security audit, all four builds/Linux packaging, and the crate
  package/dry run. Coverage is deliberately non-gating.
- The guard resolves Rust stable once to an exact version. Every Rust release job
  uses it, including failed-job reruns; notes end with `Built with Rust X.Y.Z`.
- No release job restores caches, including registry sources or packager executables.
  Packaging tools are pinned and installed with `--locked`. Binaries use default
  features: telemetry is not compiled in. Optional telemetry remains opt-in.
- Upload all assets to a draft before publishing it. Publishing creates the version
  tag at the tested SHA, after every build and crate check passes. Never move tags.
- Verify the published tag, exact asset list, and downloaded bytes before crates.io
  publication. GitHub and crates.io publication are **not atomic**.
- Commits must be signed. The lightweight tag GitHub creates is **unsigned**.

## Daily release

Prerequisites: `bash`, `git`, `just`, authenticated `gh`, and a registered commit-signing
key. Use **one release operator at a time**: workflow serialization does not uniquely
attribute competing local dispatch commands.

1. Contributions and dependency updates land on `sandbox`. Update the changelog.
2. Make a signed version bump the **last** release-preparation commit, covering
   `Cargo.toml` and `Cargo.lock` (for example, `cargo set-version --bump patch`).
   Review the version; this workflow never bumps, commits, or pushes preparation.
   The existing 5.2.0 is already tagged: the first release using this flow needs a
   higher version, not another 5.2.0.
3. Wait for `CI OK` on `sandbox`, then fast-forward `develop` to that exact green SHA
   and push. Squashing or rebasing changes the SHA and requires fresh CI.
4. Run `just release`. It fetches `develop`, captures its SHA and the previous release
   run ID, dispatches `release.yml` on `develop`, selects the **lowest new run ID**
   matching that SHA, and requires that release run to succeed.
5. The recipe finds `develop`'s push-triggered `build.yml` run for the captured SHA,
   waits for completion, and requires its **CI OK job**, not its whole-workflow
   conclusion, to succeed. Missing, skipped, cancelled, or failed gates stop it;
   a coverage-only failure does not. It then pushes that SHA to `main` without force.

Keep `develop` and `main` fast-forward only. The workflow never pushes `main`;
protected-branch updates use the operator's credentials after the final gate.

## Job graph and files

```text
guard ──► build[4 targets] ──┐
      └─► crate ────────────┴─► release ──► publish
```

- **guard:** branch, monotonic version, tag absence, main ancestry, exact-SHA CI gate;
  resolve the release compiler once.
- **build:** Linux x86_64 on `ubuntu-latest`, Linux ARM64 on `ubuntu-24.04-arm`, and
  both Darwin architectures on `macos-latest`. Linux binaries run `--version` and
  must report the manifest version (Epazote also reports a git hash). macOS x86_64
  may cross-compile on ARM; both macOS binaries are not necessarily executable on
  that runner. Both Linux legs strip and validate RPM/DEB metadata, payload paths,
  permissions, and Debian scripts without installing packages.
- **crate:** `cargo package --locked` and `cargo publish --dry-run --locked`;
  upload the exact `.crate` as a release asset.
- **release:** enforce the nine-file set, generate `SHA256SUMS`, attest all ten files,
  generate notes, create/upload the draft, publish, and verify tag/assets/bytes.
- **publish:** skip an existing crates.io version, otherwise use Trusted Publishing
  and `cargo publish --locked --no-verify`. Cargo packages from the same checkout;
  it does **not** upload the GitHub `.crate` asset directly to crates.io.

For `${VERSION}`, exactly these nine files exist before checksums:

```text
epazote-${VERSION}-x86_64-unknown-linux-musl.tar.gz
epazote-${VERSION}-aarch64-unknown-linux-musl.tar.gz
epazote-${VERSION}-x86_64-apple-darwin.tar.gz
epazote-${VERSION}-aarch64-apple-darwin.tar.gz
epazote-${VERSION}-1.x86_64.rpm
epazote-${VERSION}-1.aarch64.rpm
epazote_${VERSION}-1_amd64.deb
epazote_${VERSION}-1_arm64.deb
epazote-${VERSION}.crate
```

`SHA256SUMS` covers those nine files; the uploaded asset set is exactly **ten**.
Provenance covers `dist/*`, including `SHA256SUMS`; attestations are not extra named
assets in that set. GitHub's automatic source archives are not uploaded assets.
Each tarball contains `epazote-${VERSION}-${TARGET}/epazote`.
RPM and DEB preserve `/usr/bin/epazote` (0755),
`/etc/systemd/system/epazote.service` (0644), and
`/etc/epazote/epazote.env` (0640). Only Debian has service lifecycle scripts;
no active `epazote.yml` is packaged.

## Recovery

Read the refusal before retrying. Wrong branch/version, existing or lower tag,
main ancestry, and absent successful exact-SHA `CI OK` all block the guard.

Use **Re-run failed jobs** for transient external failures. Successful jobs keep
outputs, including the guard's resolved compiler. A leftover draft is replaced;
an already published release is never overwritten, only re-verified. A version
already on crates.io skips authentication/publication. Re-running the entire
workflow or the guard resolves Rust again and rejects an already tagged version.

Before publication, fix source on `sandbox`, run CI, promote the exact green SHA,
and dispatch again. After a tag is public, any source fix needs a **new version**;
never delete/move the tag or replace public assets. GitHub can be public even if
verification or crates.io subsequently fails: rerun the failed job, not the guard.
If recovery produces different bytes, asset verification must fail rather than
silently replacing the published release.

If `just release` stops after a successful release but before advancing `main`,
wait for the captured SHA's develop push CI run to complete and its `CI OK` job
to succeed, then `git push origin <released-sha>:refs/heads/main`. Do not gate on
coverage or bypass a missing/pending/failed CI OK. Do not dispatch a second release
of an already tagged version merely to advance `main`.

## Maintainer settings and first rollout

These are required maintainer actions, not settings applied by this change:

- Ensure `release.yml` exists on the **actual default branch** before first dispatch.
  Bootstrap by advancing it to the green integration commit through the approved
  repository process. GitHub cannot dispatch a workflow absent from that branch.
- Protect `main`: require `CI OK`, signed commits and linear history; disallow force
  pushes and deletion. Allow the designated operator to fast-forward it. A mandatory
  PR-only rule without an appropriate exception conflicts with this recipe.
- Protect `develop` against force pushes/deletion and use signed, linear promotions.
- Protect version tags against update/deletion while permitting creation.
- Keep default workflow permissions read-only; jobs request additional permissions
  explicitly. Checkouts do not persist credentials. Remote actions are full-SHA
  pinned with version comments; weekly Dependabot updates target `sandbox`.
- Configure the existing crates.io **epazote** crate's Trusted Publisher for owner
  **epazote**, repository **epazote**, workflow **release.yml**. There is no stored
  token fallback. Retire the old `CRATES_TOKEN` after this configuration is confirmed.
- No environment is required by this workflow. If repository policy adds one, allow
  `develop` and match its identity in the Trusted Publisher configuration.
- Confirm repository support/policy for provenance attestations and immutable
  releases. Do not silently drop the provenance guarantee if unavailable.

## Verification and limits

Download the desired assets and `SHA256SUMS` from
[epazote/epazote releases](https://github.com/epazote/epazote/releases), then:

```sh
sha256sum --check --ignore-missing SHA256SUMS
gh attestation verify <file> \
  --repo epazote/epazote \
  --signer-workflow epazote/epazote/.github/workflows/release.yml
```

The checksum command checks downloaded files; provenance binds a file to the
workflow identity. Release notes identify the compiler. Stable CI and release can
resolve different compilers; the exact version is fixed within the release run,
not necessarily shared with the preceding CI run. Locked dependencies, a fixed
compiler, and no restored release caches do **not** promise byte-for-byte
reproducibility across independent rebuilds.

Before merging workflow/dependency changes, run formatting, locked all-target /
all-feature check, strict Clippy and tests, a locked default release build, YAML
validation (including composite actions), `just --list`, `just --show release`,
and locked package list/package/publish dry-run checks. On an uncommitted checkout,
local `--allow-dirty` packaging checks are weaker evidence than clean CI; do not
add that flag to CI. Inspect packaged `contrib` assets and build metadata inputs.
Native Linux packaging and both Darwin builds must also pass CI. Never install
validation packages: Debian scripts can enable/start services.

Local linting and decision fixtures cannot prove live OIDC exchange, GitHub draft
publication/tag behavior, or published-asset redownload verification. Validate those
at rollout; no Epazote failure-injection exercise is claimed here.
