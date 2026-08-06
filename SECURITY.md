# Security Policy

## Supported versions

Security fixes are provided for the latest released version of `awswap`.

## Reporting a vulnerability

Please report vulnerabilities privately through GitHub's **Report a
vulnerability** security-advisory form for this repository. Do not open a public
issue for suspected credential exposure, command injection, unsafe installer
behavior, or authentication bypasses.

Include the affected version, operating system, reproduction steps, expected
behavior, and potential impact. Remove AWS account IDs, profile names, tokens,
registry credentials, and other secrets from reports and logs.

You should receive an acknowledgment within seven days. Confirmed issues will be
coordinated privately until a fix and disclosure plan are available.

## Credential handling

`awswap` does not persist AWS or registry credentials. It stores only current
and previous profile names. AWS CLI, Docker, Helm, and configured credential
helpers remain responsible for credential storage and refresh.
