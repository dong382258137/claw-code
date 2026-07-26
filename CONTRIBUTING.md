# Contributing to Claw Plus

Thanks for helping improve Claw Plus. This repository is a fork of
[dong382258137/claw-code](https://github.com/dong382258137/claw-code) (MIT License),
maintained at [dong382258137/claw-code](https://github.com/dong382258137/claw-code).

## Ground rules

- Keep changes small, reviewable, and tied to a concrete issue or behavior.
- Do not commit secrets, API keys, session transcripts with credentials, or
  generated build output.
- Prefer existing crate boundaries and utilities before adding dependencies.
- Update documentation when a user-facing command, config key, or provider
  behavior changes.
- Keep examples copy/paste safe. Use placeholder keys such as `sk-ant-...` and
  avoid commands that require live credentials unless the text explicitly says
  so.

## Local setup

```bash
git clone https://github.com/dong382258137/claw-code
cd claw-code/rust
cargo build --workspace
cargo test --workspace
```

On Windows PowerShell, build from the same `rust` workspace and run the binary
with the `.exe` suffix:

```powershell
git clone https://github.com/dong382258137/claw-code
cd claw-code/rust
cargo build --workspace
cargo test --workspace
.\target\debug\claw-plus.exe doctor
```

## Contribution workflow

1. Open an issue describing what you want to change and why.
2. Fork this repository and create a feature branch.
3. Make your changes, adding or updating tests as needed.
4. Run `cargo test --workspace` and `cargo clippy --workspace -- -D warnings`
   before pushing.
5. Open a pull request against the `main` branch with a clear description.

## Code style

- Follow the conventions already in use in each crate.
- Use `cargo fmt` for Rust formatting.
- Keep comments concise and useful — explain *why*, not *what*.
- Prefer `expect` with a clear message over `unwrap` in production paths.

## Testing

- New tool or provider behavior should include at least one test.
- Use the mock service harness (`mock-anthropic-service`) for provider-related
  tests that need deterministic responses.
- Tests that shell out to external commands should be gated behind
  `#[cfg(unix)]` or `#[cfg(target_os = "windows")]` as appropriate.

## Questions

For questions that aren't covered here, open a GitHub Discussion or issue on
the repository.
