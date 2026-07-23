//! Full-screen TUI mode for the claw REPL.
//!
//! Gated on the `full-tui` Cargo feature. When enabled, `claw --tui`
//! launches an alternate-screen ratatui interface with:
//! - A scrollable output area capturing streamed responses
//! - A bottom input line with slash-command popup menu (fuzzy filter)
//! - A persistent status bar showing model, cwd, branch, tokens, cost
//!
//! All modules here are `#[cfg(feature = "full-tui")]`. When the feature
//! is off, this entire module compiles to nothing.

#![cfg(feature = "full-tui")]
#![allow(dead_code, unused_imports, unused_variables)]

pub(crate) mod app;
pub(crate) mod input_line;
pub(crate) mod output_view;
pub(crate) mod sidebar;
pub(crate) mod slash_menu;
pub(crate) mod status_bar;
pub(crate) mod stderr_guard;
pub(crate) mod tool_card;

pub(crate) mod wizard;

#[cfg(test)]
mod tests;
