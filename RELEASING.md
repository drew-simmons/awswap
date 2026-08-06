# Releasing

Release Please manages versions, `Cargo.lock`, `CHANGELOG.md`, tags, and GitHub
release notes from Conventional Commits. Cargo-dist 0.32.0 builds the macOS and
Linux archives, checksums, and shell installer for each generated tag.

## One-time GitHub setup

1. Create a fine-grained personal access token scoped only to
   `drew-simmons/awswap` with these repository permissions:
   - Contents: read and write
   - Issues: read and write
   - Pull requests: read and write
2. Add it as the repository Actions secret `RELEASE_PLEASE_TOKEN`.
3. In **Settings → Actions → General**, allow GitHub Actions to create pull
   requests if repository or organization policy otherwise blocks them.

A dedicated token is intentional: GitHub suppresses workflow events created by
the default `GITHUB_TOKEN`. The token lets CI run on Release PRs and lets a
release-please tag trigger the cargo-dist workflow.

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

The repository is bootstrapped at version `0.0.0`. Use `feat: initial release`
for the initial repository commit so the first Release PR proposes `v0.1.0`.

Do not manually bump the package version, edit generated changelog entries, or
create release tags. If a cargo-dist run fails, fix the cause and rerun the
failed workflow from GitHub Actions; the existing draft remains unpublished.

The generated `.github/workflows/release.yml` contains a deliberate integration
that uploads to release-please's existing draft. If `dist generate` rewrites the
workflow, preserve or restore its `Publish GitHub Release` step.

Do not move or recreate a published tag. Fix released defects with a new patch
release.
