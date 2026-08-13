# Security Policy

## Supported versions

Security fixes cover the latest released version of `awswap`. Upgrade before
you report a flaw that may already have a fix.

## Reporting a vulnerability

Report vulnerabilities through GitHub's private
[security advisory form](https://github.com/drew-simmons/awswap/security/advisories/new).

> [!WARNING]
> Do not open a public issue for suspected credential exposure, command
> injection, unsafe installer behavior, or an authentication bypass.

Include the affected version, operating system, steps to reproduce the issue,
expected behavior, and possible impact. Remove AWS account IDs, profile names,
tokens, registry credentials, and all other secrets from reports and logs.

You should receive an acknowledgment within seven days. Confirmed issues will be
coordinated privately until a fix and disclosure plan are available.

## Credential handling

`awswap` does not store AWS or registry credentials. It stores only the current,
previous, and eight most recent profile names. AWS CLI, Docker, Helm, and any
credential helpers store and refresh their own credentials.
