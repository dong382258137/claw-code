# CLAUDE.md

> 本项目早期代码源自 [dong382258137/claw-code](https://github.com/dong382258137/claw-code)（MIT License），现已独立演进。

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Detected stack
- Languages: Rust.
- Frameworks: none detected from the supported starter markers.

## Verification
- Run Rust verification from repo root: `scripts/fmt.sh --check`; for formatting use `scripts/fmt.sh`. Run Rust clippy/tests from `rust/`: `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`
- `src/` and `tests/` are both present; update both surfaces together when behavior changes.

## Repository shape
- `rust/` contains the Rust workspace and the Web server/runtime implementation. The product surface is the local Web UI; the only entry point is `claw serve` (CLI/TUI/REPL were removed in Phase 3). The server crate is `rust/crates/claw-server` (renamed from `rusty-claude-cli` in Phase 9); the single binary is `claw` (the legacy `claw-plus` bin has been removed).
- `rust/crates/claw-server/web` is the single-page frontend; run `npm run build` there after frontend changes (the bundle is embedded into the binary).
- `src/` contains source files that should stay consistent with generated guidance and tests.
- `tests/` contains validation surfaces that should be reviewed alongside code changes.
- Semantic search requires `onnxruntime.dll` (>=1.24) next to the binary; without it the service falls back to keyword matching. See `USAGE.md` §7.

## Working agreement
- Prefer small, reviewable changes and keep generated bootstrap files aligned with actual repo workflows.
- Keep shared defaults in `.claw.json`; reserve `.claw/settings.local.json` for machine-local overrides.
- Do not overwrite existing `CLAUDE.md` content automatically; update it intentionally when repo workflows change.
