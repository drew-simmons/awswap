# Contributing

Contributions are welcome through GitHub issues and pull requests.

## Development setup

1. Install Rust 1.88.0 using `rustup` or `mise`.
2. Install AWS CLI v2.
3. Install prek 0.4.12:

   ```sh
   uv tool install prek==0.4.12
   ```

4. Clone the repository and install its Git hook:

   ```sh
   prek install
   ```

5. Run all checks:

   ```sh
   prek run --all-files
   ```

The hook runs repository hygiene checks, `cargo fmt`, Clippy with warnings
denied, and the test suite. The equivalent Rust commands are:

```sh
cargo fmt --all --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets
```

Tests must not require live AWS credentials. Keep subprocess behavior isolated
behind parsing or command-construction tests where practical.

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

## Pull requests

- Keep changes focused and explain their user-visible impact.
- Add tests for new behavior and regressions.
- Update the README when commands, environment variables, or requirements
  change.
- Do not commit credentials, AWS configuration, generated build output, or local
  tool state.

By contributing, you agree that your contributions are licensed under the MIT
License.
