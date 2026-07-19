#![cfg(feature = "full-tui")]

//! Scrollable output view that captures streamed text via `io::Write`.
//!
//! `OutputView` is a ring buffer holding the last N characters of output
//! written by `consume_stream`. It implements `io::Write` so it can be
//! passed as the `out` sink in place of `io::stdout()` during a TUI turn.
//! The TUI render loop reads `snapshot()` to display the current buffer
//! content as a ratatui `Paragraph`.

use std::io::{self, Write};
use std::sync::{Arc, Mutex};

/// Maximum bytes retained in the scrollback buffer.
const MAX_BUFFER_BYTES: usize = 64 * 1024;

/// Thread-safe scrollback buffer for streamed output.
#[derive(Debug)]
pub(crate) struct OutputView {
    inner: Arc<Mutex<OutputBuffer>>,
}

#[derive(Debug, Default)]
struct OutputBuffer {
    buffer: String,
    /// Total bytes ever written (for diagnostics; not capped).
    total_written: u64,
    /// True if any output was truncated (buffer overflowed).
    truncated: bool,
}

impl OutputView {
    /// Create a new empty buffer.
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(OutputBuffer::default())),
        }
    }

    /// Share the underlying buffer with another consumer (e.g., the render loop).
    pub(crate) fn shared_handle(&self) -> Arc<Mutex<OutputBuffer>> {
        Arc::clone(&self.inner)
    }

    /// Snapshot of the current buffer content (cloned).
    pub(crate) fn snapshot(&self) -> String {
        self.inner
            .lock()
            .expect("OutputBuffer mutex poisoned")
            .buffer
            .clone()
    }

    /// Clear the buffer.
    pub(crate) fn clear(&mut self) {
        let mut guard = self.inner.lock().expect("OutputBuffer mutex poisoned");
        guard.buffer.clear();
        guard.truncated = false;
    }

    /// Total bytes ever written.
    pub(crate) fn total_written(&self) -> u64 {
        self.inner
            .lock()
            .expect("OutputBuffer mutex poisoned")
            .total_written
    }
}

impl Default for OutputView {
    fn default() -> Self {
        Self::new()
    }
}

impl Write for OutputView {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let text = String::from_utf8_lossy(bytes);
        let mut guard = self.inner.lock().expect("OutputBuffer mutex poisoned");
        guard.buffer.push_str(&text);
        guard.total_written += bytes.len() as u64;
        if guard.buffer.len() > MAX_BUFFER_BYTES {
            let overflow = guard.buffer.len() - MAX_BUFFER_BYTES;
            guard.buffer = guard.buffer.split_off(overflow);
            guard.truncated = true;
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_appends_to_buffer() {
        let mut view = OutputView::new();
        view.write_all(b"hello ").unwrap();
        view.write_all(b"world").unwrap();
        assert_eq!(view.snapshot(), "hello world");
    }

    #[test]
    fn total_written_counts_all_bytes() {
        let mut view = OutputView::new();
        view.write_all(b"abc").unwrap();
        view.write_all(b"de").unwrap();
        assert_eq!(view.total_written(), 5);
    }

    #[test]
    fn invalid_utf8_is_lossy_converted() {
        let mut view = OutputView::new();
        view.write_all(&[0xff, 0xfe, 0xfd]).unwrap();
        assert!(!view.snapshot().is_empty());
    }

    #[test]
    fn buffer_trims_when_exceeding_max() {
        let mut view = OutputView::new();
        let big_chunk = "x".repeat(MAX_BUFFER_BYTES + 100);
        view.write_all(big_chunk.as_bytes()).unwrap();
        let snap = view.snapshot();
        assert_eq!(snap.len(), MAX_BUFFER_BYTES);
        assert!(snap.ends_with(&"x".repeat(100)));
    }

    #[test]
    fn clear_empties_buffer() {
        let mut view = OutputView::new();
        view.write_all(b"data").unwrap();
        view.clear();
        assert_eq!(view.snapshot(), "");
    }

    #[test]
    fn shared_handle_shares_state() {
        let mut view = OutputView::new();
        let handle = view.shared_handle();
        view.write_all(b"shared").unwrap();
        let snap = handle.lock().unwrap().buffer.clone();
        assert_eq!(snap, "shared");
    }

    #[test]
    fn flush_is_noop() {
        let mut view = OutputView::new();
        assert!(view.flush().is_ok());
    }
}
