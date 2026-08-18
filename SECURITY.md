# Security Policy

Oxid orchestrates Docker containers, holds encrypted secrets, and accepts
webhook input from the network — security issues here have real
consequences. Reports are taken seriously and triaged as a priority over
regular feature work.

## Supported versions

Oxid is pre-1.0 and under active development. There is currently a single
supported line: the `main` branch. Security fixes land there and are called
out explicitly in the commit message and, once tagged releases exist, in the
release notes.

| Version | Supported |
|---|---|
| `main` (latest commit) | ✅ |
| Anything else / forks | ❌ |

## Reporting a vulnerability

**Please do not open a public GitHub issue for security vulnerabilities.**

Use GitHub's private vulnerability reporting for this repository instead:

1. Go to the [**Security** tab](https://github.com/sazardev/oxid/security) of this repository.
2. Click **"Report a vulnerability"**.
3. Describe the issue, its impact, and steps to reproduce it (a minimal
   repro or PoC helps a lot).

This opens a private conversation with the maintainer(s) only — nothing is
visible publicly until a fix is ready and you both agree it's safe to
disclose.

If for some reason you can't use that flow, opening a regular issue with as
few technical specifics as possible (just "I found a potential security
issue, please contact me") is a reasonable fallback — a maintainer will move
the conversation to a private channel.

## What to expect

- **Acknowledgment:** within a few days of the report.
- **Triage:** you'll get an initial assessment of severity and whether it's
  accepted as a valid vulnerability.
- **Fix & disclosure:** once a fix is ready, we'll coordinate with you on
  timing and credit before any public disclosure (including a GitHub
  Security Advisory, if applicable).

There is no bug bounty program — this is a personal open-source project —
but responsible disclosure is credited in the advisory/release notes unless
you ask to stay anonymous.

## Scope — what's most interesting to report

- Webhook signature verification bypass (`X-Hub-Signature-256` / HMAC checks
  in `oxid-daemon`).
- Secret handling: AES-GCM encryption at rest, master key handling, secret
  leakage through logs/API responses/audit trail.
- Environment variable inheritance (`Global → Project → Branch → Runtime`)
  leaking secrets across projects/branches it shouldn't.
- Container/OCI adapter issues that could let a deployed environment escape
  its intended isolation, or that let an attacker control build/run
  arguments passed to Docker.
- Anything letting an unauthenticated caller trigger a deploy, read another
  project's secrets, or access the Docker socket indirectly.

Out of scope: vulnerabilities that only manifest with an already-compromised
Docker socket/host, or that require an attacker to already have write access
to the git repository being deployed (that's the trust boundary Oxid
assumes, per `SPEC.md`).
