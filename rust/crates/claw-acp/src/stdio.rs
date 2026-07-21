//! 专用线程读取 ACP stdio 传输的标准输入。
//!
//! 参考 grok-build `xai-acp-lib/src/stdin_reader.rs`。
//!
//! # 为什么不用 `tokio::io::stdin()`
//!
//! `tokio::io::stdin()` 并非真正异步。Tokio 在内部线程池上用阻塞 `std::io`
//! 读取来服务它,且该读取**无法取消**。对于持久化的 stdio 传输,reader
//! 几乎总是停在 read 上等待下一行;如果同时有其它代码调用 `std::io::stdin()`
//! (Windows 上会争用全局 StdinLock),会死锁到 EOF 才解锁。
//!
//! 解决方案:专用的 OS 线程做阻塞读取,通过 mpsc channel 把每行(含尾部 `\n`)
//! 异步投递给 runtime。
//!
//! 当前实现是简化版:暂不做 Windows 的 stdin 句柄隔离(将真实 stdin 重定向
//! 到 NUL),仅做基本的专用线程读取。Windows 隔离留给后续迭代。

use std::io::BufRead;

use tokio::sync::mpsc;

/// 缓冲 stdin 行的 channel 深度。较小:reader 线程在 channel 满时阻塞,
/// 对快速发送方施加自然背压而不是无限增长内存。
const STDIN_LINE_CHANNEL_DEPTH: usize = 64;

/// 启动专用 OS 线程,以**同步阻塞**的 `std::io` 从进程标准输入读取
/// 换行分隔的行,并通过返回的 channel 投递每行(含尾部 `\n`,类似
/// `read_line`/`read_until`)。最后一行即使没有尾换行也会在 channel 关闭前
/// 投递。
///
/// channel 在 stdin EOF、读取失败或 [`Receiver`] 被 drop 时关闭(即
/// [`recv`](mpsc::Receiver::recv) 返回 `None`)。
///
/// 该 reader 是 agent-stdio 路径中**唯一的** stdin 消费者。
pub fn spawn_stdin_line_reader() -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel::<Vec<u8>>(STDIN_LINE_CHANNEL_DEPTH);

    std::thread::Builder::new()
        .name("claw-acp-stdin".to_string())
        .spawn(move || {
            let stdin = std::io::stdin();
            forward_lines(stdin.lock(), &tx);
        })
        .expect("failed to spawn claw-acp-stdin reader thread");
    rx
}

/// 从 `reader` 读取 `\n` 分隔的行,通过 `tx` 转发 —— 直到 EOF、读取错误
/// 或 receiver 被 drop。
fn forward_lines<R: BufRead>(mut reader: R, tx: &mpsc::Sender<Vec<u8>>) {
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            // EOF 或致命读取错误:返回,drop `tx` 关闭 channel。
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        // `blocking_send` 在 channel 满时挂起当前线程(非 runtime worker),
        // 仅在 receiver drop 后报错 —— 此时也没有东西可投递了。
        if tx.blocking_send(std::mem::take(&mut line)).is_err() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn forward_lines_yields_each_line_with_newline() {
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(8);
        let input = b"line1\nline2\nline3\n";
        forward_lines(Cursor::new(input), &tx);
        drop(tx);

        let lines: Vec<Vec<u8>> = futures::executor::block_on(async {
            let mut out = Vec::new();
            while let Some(line) = rx.recv().await {
                out.push(line);
            }
            out
        });
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], b"line1\n");
        assert_eq!(lines[1], b"line2\n");
        assert_eq!(lines[2], b"line3\n");
    }

    #[test]
    fn forward_lines_delivers_final_line_without_newline() {
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(8);
        let input = b"no_trailing_newline";
        forward_lines(Cursor::new(input), &tx);
        drop(tx);

        let lines: Vec<Vec<u8>> = futures::executor::block_on(async {
            let mut out = Vec::new();
            while let Some(line) = rx.recv().await {
                out.push(line);
            }
            out
        });
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0], b"no_trailing_newline");
    }

    #[test]
    fn forward_lines_empty_input_yields_nothing() {
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(8);
        forward_lines(Cursor::new(b""), &tx);
        drop(tx);

        let count = futures::executor::block_on(async {
            let mut n = 0;
            while rx.recv().await.is_some() {
                n += 1;
            }
            n
        });
        assert_eq!(count, 0);
    }
}
