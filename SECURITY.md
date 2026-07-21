# Security Policy

## Supported versions

Security fixes target the current `main` branch and the latest published
release artifacts when available. Older experimental branches are not supported
unless a maintainer explicitly marks them as supported.

## Reporting a vulnerability

Please do **not** open a public issue for a suspected vulnerability. Use GitHub
private vulnerability reporting for
[dong382258137/claw-code](https://github.com/dong382258137/claw-code)
when available, or contact a maintainer through the repository's published
support channel with a minimal, non-destructive reproduction.

Include:

- affected command, crate, or workflow;
- operating system and shell, especially for Windows/PowerShell path issues;
- whether live credentials, MCP servers, plugins, or workspace filesystem
  state might be involved;
- steps to reproduce without exposing secrets.

## Upstream

This repository is a fork of [ultraworkers/claw-code](https://github.com/ultraworkers/claw-code) (MIT License).
Security issues that also affect the upstream project should be reported to
both repositories.
