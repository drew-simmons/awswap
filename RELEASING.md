# Releasing

Release Please manages versions, `Cargo.lock`, `CHANGELOG.md`, tags, and GitHub
release notes from Conventional Commits. Cargo-dist 0.32.0 builds the macOS and
Linux archives, checksums, and shell installer for each release tag.

## One-time GitHub setup

1. Create a fine-grained personal access token scoped only to
   `drew-simmons/awswap` with these repository permissions:
   - Contents: read and write
   - Issues: read and write
   - Pull requests: read and write
2. Add it as the repository Actions secret `RELEASE_PLEASE_TOKEN` for the full
   release flow. Until then, Release Please falls back to `GITHUB_TOKEN`.
3. In **Settings → Actions → General**, enable **Allow GitHub Actions to create
   and approve pull requests**. The `GITHUB_TOKEN` fallback requires it.
4. Create a crates.io API token and add it as the repository Actions secret
   `CARGO_REGISTRY_TOKEN`.

> [!IMPORTANT]
> Use a dedicated token for Release Please. GitHub does not start new workflow
> runs for events created with the default `GITHUB_TOKEN`. The dedicated token
> lets CI run on Release PRs and lets a Release Please tag start cargo-dist.
> The fallback can create or update the Release PR, but it cannot start those
> later workflows.

## Conventional Commits

Use Conventional Commit subjects on commits merged to `main`:

- `fix: ...` creates a patch release.
- `feat: ...` creates a minor release.
- `feat!: ...` or a `BREAKING CHANGE:` footer creates a breaking release.
- `docs:`, `test:`, `ci:`, and `chore:` do not create releases by themselves.

Before version 1.0, breaking changes bump the minor version. Release Please
keeps implementation-only commit types out of the public changelog.

## Automated release flow

1. Push or merge Conventional Commits to `main`.
2. Release Please creates or updates a Release PR containing the calculated
   version, `Cargo.toml`, `Cargo.lock`, and `CHANGELOG.md` changes.
3. Review its generated notes and merge the Release PR after CI passes.
4. Release Please creates the version tag and a draft GitHub Release.
5. The tag starts cargo-dist. After every target builds successfully,
   cargo-dist attaches the archives, checksums, and `awswap-installer.sh`, then
   publishes the draft release.
6. The release workflow publishes the same version to crates.io.

The repository is bootstrapped at version `0.0.0`. Use `feat: initial release`
for the initial repository commit so the first Release PR proposes `v0.1.0`.

> [!IMPORTANT]
> Do not bump the package version, edit generated changelog entries, or create
> release tags by hand. If cargo-dist fails, fix the cause and rerun the failed
> workflow in GitHub Actions. The draft release stays unpublished.

> [!WARNING]
> `.github/workflows/release.yml` has custom steps that upload to the draft from
> Release Please and publish the crate to crates.io. If `dist generate` rewrites
> the workflow, restore the `Publish GitHub Release` and
> `Publish crate to crates.io` steps before you merge the change.

Do not move or recreate a published tag. Fix released defects with a new patch
release.
