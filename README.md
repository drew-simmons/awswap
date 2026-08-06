# awswap

`awswap` is a fast AWS profile switcher with a searchable, metadata-rich picker.
It validates credentials when you switch, opens AWS sign-in when SSO or login
credentials have expired, authenticates Docker and Helm to ECR, and includes
built-in status and diagnostic views.

## Install

### Installer (macOS and Linux)

```sh
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/drew-simmons/awswap/releases/latest/download/awswap-installer.sh \
  | sh
```

The installer downloads a prebuilt release and verifies its checksum. Direct
archives and checksums are available from
[GitHub Releases](https://github.com/drew-simmons/awswap/releases).

### Build from source

Building from source requires Rust 1.88 or newer:

```sh
git clone https://github.com/drew-simmons/awswap.git
cd awswap
cargo install --path . --locked
```

### Supported release targets

| Platform | Architectures |
| --- | --- |
| macOS | Apple Silicon (`aarch64`), Intel (`x86_64`) |
| Linux with glibc | `aarch64`, `x86_64` |
| Linux with musl/Alpine | `x86_64` |

## Requirements

- AWS CLI v2
- Docker and/or Helm when ECR authentication is needed
- Bash, Zsh, or Fish for updating the active shell

Configure profiles with the AWS CLI first, for example:

```sh
aws configure sso
```

Profiles using an AWS CLI `login_session` are also supported.

## Shell setup

Add the shell hook so a switch updates `AWS_PROFILE` in the current shell.

**Zsh** (`~/.zshrc`):

```sh
eval "$(awswap init zsh)"
```

**Bash** (`~/.bashrc`):

```sh
eval "$(awswap init bash)"
```

**Fish** (`~/.config/fish/conf.d/awswap.fish`):

```fish
awswap init fish | source
```

Restart the shell or source its configuration after adding the hook.

Optional generated completions use configured profile names as candidates:

```sh
# Zsh or Bash
source <(awswap completions zsh) # replace zsh with bash as needed
```

```fish
# Fish
awswap completions fish | source
```

## Usage

```text
awswap [options]                  Search and select a profile
awswap [options] <profile>        Switch directly to a profile
awswap [options] -                Switch back to the previous profile
awswap current                    Print the active profile
awswap list                       List configured profiles
awswap status [profile]           Show identity and integration status
awswap doctor [profile]           Diagnose configuration and credentials
awswap login [options] [profile]  Refresh AWS and ECR authentication
awswap completions <shell>        Generate Bash, Zsh, or Fish completions
awswap help                       Show all commands and options
```

Common options:

```text
--no-ecr                  Skip Docker and Helm ECR authentication
-r, --registry <value>    Use a registry hostname or AWS account ID; repeatable
-q, --quiet               Suppress progress and success output
--json                    Emit JSON from list, current, status, doctor, and login
-v, --verbose             Show commands and detailed AWS failures
```

Examples:

```sh
awswap dev
aws sts get-caller-identity
awswap status
awswap doctor

awswap --no-ecr staging
awswap --registry 123456789012 dev
awswap status --json

docker pull 123456789012.dkr.ecr.us-east-1.amazonaws.com/app:latest
helm pull oci://123456789012.dkr.ecr.us-east-1.amazonaws.com/charts/app

awswap -
```

## Authentication and ECR

On each switch, `awswap` shows progress for operations that may require the
network and prints an identity receipt containing the profile, account, region,
and role or user. The interactive picker shows local region, authentication,
and account metadata, with current, previous, and recently used profiles first.

It then:

1. Checks the profile with `aws sts get-caller-identity`.
2. If credentials are invalid, runs `aws sso login` for SSO profiles or
   `aws login` for profiles with `login_session`, opening the AWS sign-in flow.
3. Authenticates registry clients: Helm receives an ECR login token; Docker
   receives one unless its effective credential helper is `ecr-login`, in which
   case `awswap` validates that helper under the selected profile instead.

The ECR credential helper obtains short-lived Docker credentials on demand and
does not support Docker's credential `store` operation, so `awswap` deliberately
avoids `docker login` when that helper is configured. The shell hook keeps
`AWS_PROFILE` available to the helper for later pulls.

The shell hook exports `AWS_PROFILE` and `AWS_DEFAULT_PROFILE` and removes stale
static AWS credential variables that would otherwise override the profile.
`awswap` stores only the current, previous, and eight most recently used
profile names under `$XDG_STATE_HOME/awswap` (or `~/.local/state/awswap`); AWS,
Docker, and Helm manage their own credentials.

Flags are preferred for one-off behavior. Equivalent environment controls are
available for persistent configuration:

```sh
AWSWAP_NO_ECR=1 awswap dev
AWSWAP_ECR_REGISTRIES=123456789012,registry.example.com awswap dev
AWSWAP_HOME=/custom/state/directory awswap dev
```

`AWSWAP_ECR_REGISTRIES` accepts comma-separated AWS account IDs or registry
hostnames. Without it, `awswap` uses the selected profile's account ID and
region. Account IDs are expanded with the correct ECR DNS suffix for the `aws`,
`aws-cn`, `aws-us-gov`, `aws-iso`, `aws-iso-b`, `aws-iso-e`, `aws-iso-f`, and
`aws-eusc` partitions.

## Upgrade and uninstall

Rerun the installer command from [Install](#install) to upgrade.

Uninstall the shell-installed binary with:

```sh
rm "${CARGO_HOME:-$HOME/.cargo}/bin/awswap"
```

Also remove the shell hook from your shell configuration if it is no longer
needed. State contains only profile names and can be deleted from
`${XDG_STATE_HOME:-$HOME/.local/state}/awswap`.

## Troubleshooting

Run `awswap doctor` first. It checks the AWS CLI, profile discovery, active
identity, region, shell hook, state path, Docker and Helm, the Docker credential
helper, and static environment variables that may override profiles.

- If the profile does not persist, install the shell hook and start a new shell.
  Without the hook, awswap reports a profile as **selected**, not **active**.
- Authentication errors distinguish expired or unavailable credentials, access
  denial, network failures, missing regions, and missing profiles, with a
  specific recovery command when possible. Add `--verbose` for the original AWS
  CLI detail.
- If ECR is not needed or Docker/Helm is unavailable, use `--no-ecr`.
- Use `NO_COLOR=1` to disable colored output.

## Development

The project uses Rust 1.88.0. Before submitting a change, run:

```sh
cargo fmt --all --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for contribution guidance,
[SECURITY.md](SECURITY.md) for vulnerability reporting, and
[RELEASING.md](RELEASING.md) for maintainer release steps.

## License

MIT
