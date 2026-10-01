# Security Policy

Warden is a process supervisor: a compromise of the binary or its control
sockets can affect every app it runs. Please report security issues privately.

## Reporting a vulnerability

While this repository is **private**, do **not** open a public GitHub Issue
or Discussion for a security finding.

1. Email the maintainers using a private channel tied to the GitHub org/repo
   owners (preferred: GitHub Security Advisories for this repository when
   enabled, or a direct message to the org owners).
2. Include: affected version or commit, what you can do, and a minimal
   reproduction if you have one. Do not attach production secrets.
3. Allow a reasonable time for acknowledgement and a fix before any public
   disclosure (coordinated disclosure). We aim to acknowledge within a few
   business days.

If GitHub Private Vulnerability Reporting is enabled for
[`oceanwap/warden`](https://github.com/oceanwap/warden), use that form first:
**Security → Report a vulnerability**.

## Scope

In scope: remote or local privilege escalation via Warden, unauthenticated
control-plane access, path/command injection through config or IPC that
Warden should have rejected, and crypto/checksum failures in install or
release verification.

Out of scope (unless they amplify a Warden bug): vulnerabilities only in
apps Warden supervises, the host OS, or third-party runtimes (Bun/Node)
when Warden is configured correctly.

## Supported versions

There is no published release yet (`0.1.0` is unreleased). Fixes land on
`main` until the first tagged release; after that, the latest release and
`main` are the supported lines unless a security advisory says otherwise.
