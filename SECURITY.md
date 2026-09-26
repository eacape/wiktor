# Security Policy

## Supported versions

Wiktor is pre-1.0. Security fixes land on `main` and are released in the next
milestone/release. No separate LTS maintenance branches are maintained yet.

## Reporting a vulnerability

Please report security vulnerabilities privately — do **not** file a public issue.

Preferred channel: **GitHub Security Advisory**
(Releases → "Report a vulnerability", or directly at
<https://github.com/eacape/wiktor/security/advisories/new>).

You can also write to `eacape@users.noreply.github.com`. Please include:

- Affected version(s) / commit and how to reproduce.
- The impact (what is at risk, severity estimate).
- Any proposed patch, if available.

We aim to acknowledge reports within 3 business days and to give a first triage
within a week. We ask that you allow a reasonable coordination window before public
disclosure so a fix can be prepared.

## Scope

This policy covers the `wiktor` crates and the production `deploy/` scripts. The
optional external services we integrate with (qdrant, Meilisearch, litestream) have
their own security processes — a vulnerability in those backends should be reported
to their maintainers.

## Release process

Security fixes are released promptly after they are confirmed, either as a hotfix
patch or bundled with the next scheduled release. Backports are considered for
deployments still on the prior stable version.