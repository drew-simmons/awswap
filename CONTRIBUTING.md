# Contributing

Thank you for helping improve `awswap`. Use a GitHub issue to report a bug or
suggest a change. Open a pull request when you have a tested fix.

## Development setup

1. Install Rust 1.88.0 using `rustup` or `mise`.
2. Install AWS CLI v2.
3. Install prek 0.4.12:

   ```sh
   uv tool install prek==0.4.12
   ```

4. Clone the repository:

   ```sh
   git clone https://github.com/drew-simmons/awswap.git
   cd awswap
   ```

5. Install the Git hook:

   ```sh
   prek install
   ```

6. Run the hooks and build the crate package:

   ```sh
   prek run --all-files
   cargo package --locked --allow-dirty
   ```

The hook runs repository hygiene checks, `cargo fmt`, Clippy with warnings
denied, and the test suite. The equivalent Rust commands are:

```sh
cargo fmt --all --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets
```

> [!IMPORTANT]
> Tests must not require live AWS credentials or change a developer's AWS,
> Docker, or Helm setup. Test parsing and command construction where possible.

## Commit messages

Use Conventional Commit subjects because release automation derives versions
and release notes from commits merged to `main`:

```text
fix: handle expired SSO sessions
feat: add profile aliases
feat!: change the shell hook protocol
```

Use `fix:` for bug fixes, `feat:` for features, and `!` or a
`BREAKING CHANGE:` footer for incompatible changes. Types such as `docs:`,
`test:`, `ci:`, and `chore:` do not trigger a release by themselves.

> [!TIP]
> The pull request title should follow this format. The project uses squash
> merges, so that title becomes the commit subject on `main`.

## Pull requests

- Keep changes focused and explain their user-visible impact.
- Add tests for new behavior and regressions.
- Update the docs when commands, environment variables, or requirements
  change.
- Do not commit credentials, AWS configuration, generated build output, or local
  tool state.

Before you open a pull request, run both checks from the setup steps and
describe any check you could not run.

By contributing, you agree that your contributions are licensed under the MIT
License.
