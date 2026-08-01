//! Ports from grok-build (Apache-2.0, xai-org/grok-build).
//!
//! 本模块隔离从 grok-build 移植的 TUI 子模块,与 claw 原生 `tui/` 模块分开,
//! 便于跟踪上游变更与审计。每个子模块文件头注释记录源 SHA 与适配点。
//!
//! See `tui-ports/PORTING.md` for the per-module porting ledger.

pub(crate) mod diff_view;
pub(crate) mod project_picker;
