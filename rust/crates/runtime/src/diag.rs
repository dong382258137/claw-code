//! 统一诊断基础设施 — Multi-Agent Hardening Plan §4.1。
//!
//! 设计文档:`docs/multi-agent-hardening-plan.md` §4.1 / §0.1 / §0.2
//!
//! 架构:
//! - [`install_panic_hook`][]:提取自 `rusty-claude-cli/src/lib.rs` main_entry 内联闭包,
//!   落盘到 `~/.claw/claw-crash.log`。供 `main_entry()`、`headless` binary、测试入口复用。
//! - [`DiagLog`][]:统一诊断日志入口,支持结构化 KV 记录,落盘到 `~/.claw/diag.log`。
//! - [`DiagEntry`][]:诊断记录条目,含 timestamp/level/category/message/fields。
//!
//! v2 修正(§0.1):
//! - 不是"新增 panic hook",而是**提取**现有内联闭包到本模块,
//!   `main_entry` 改为调用 `diag::install_panic_hook()` 并删除内联代码。
//! - `std::panic::set_hook` 是**替换**不是追加,所以只能有一个 hook 函数。
//!
//! v2 修正(§0.2):
//! - `headless.rs` 是真实缺口,需补 `install_panic_hook()` 调用。

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// 诊断日志级别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiagLevel {
    /// 信息:正常运行信号(如 subagent 启动/完成)。
    Info,
    /// 警告:可恢复异常(如 retry 触发、cost limit 接近)。
    Warn,
    /// 错误:不可恢复异常(如 subagent 失败、validation gate 拒绝)。
    Error,
    /// 致命:panic 或 process abort。
    Fatal,
}

/// 诊断日志条目。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagEntry {
    /// Unix 毫秒时间戳。
    pub timestamp_ms: u128,
    /// 日志级别。
    pub level: DiagLevel,
    /// 分类(如 `subagent`/`validation`/`panic`/`recovery`)。
    pub category: String,
    /// 主消息。
    pub message: String,
    /// 结构化 KV 字段(JSON 对象)。
    pub fields: serde_json::Value,
}

impl DiagEntry {
    /// 创建一条新诊断条目,timestamp 自动填充为当前时间。
    pub fn new(level: DiagLevel, category: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            timestamp_ms: now_ms(),
            level,
            category: category.into(),
            message: message.into(),
            fields: serde_json::Value::Object(serde_json::Map::new()),
        }
    }

    /// 追加一个 KV 字段。
    pub fn with_field(mut self, key: impl Into<String>, value: impl Into<serde_json::Value>) -> Self {
        if let Some(obj) = self.fields.as_object_mut() {
            obj.insert(key.into(), value.into());
        }
        self
    }

    /// 格式化为单行日志字符串(JSON)。
    pub fn to_json_line(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "<serde error>".to_string())
    }
}

/// 诊断日志sink,append-only 文件 + 内存 ring buffer(最近 N 条)。
pub struct DiagLog {
    /// 落盘文件路径(若 None 则不落盘,仅内存)。
    path: Option<PathBuf>,
    /// 内存 ring buffer(Mutex 保护以支持多线程 append)。
    buffer: Mutex<Vec<DiagEntry>>,
    /// ring buffer 容量。
    buffer_cap: usize,
}

impl DiagLog {
    /// 创建一个不落盘的诊断日志(仅内存,主要用于测试)。
    pub fn in_memory(cap: usize) -> Self {
        Self {
            path: None,
            buffer: Mutex::new(Vec::with_capacity(cap.min(1024))),
            buffer_cap: cap,
        }
    }

    /// 创建一个落盘到 `path` 的诊断日志,内存 buffer 默认 256 条。
    pub fn with_file(path: PathBuf) -> std::io::Result<Self> {
        // 确保父目录存在
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Ok(Self {
            path: Some(path),
            buffer: Mutex::new(Vec::with_capacity(256)),
            buffer_cap: 256,
        })
    }

    /// 追加一条诊断条目。落盘失败不影响内存 buffer。
    pub fn append(&self, entry: DiagEntry) {
        // 落盘
        if let Some(path) = &self.path {
            if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
                let _ = writeln!(file, "{}", entry.to_json_line());
            }
        }
        // 内存 buffer(ring)
        if let Ok(mut buf) = self.buffer.lock() {
            if buf.len() >= self.buffer_cap {
                buf.remove(0);
            }
            buf.push(entry);
        }
    }

    /// 便捷方法:Info 级别。
    pub fn info(&self, category: &str, message: &str) {
        self.append(DiagEntry::new(DiagLevel::Info, category, message));
    }

    /// 便捷方法:Warn 级别。
    pub fn warn(&self, category: &str, message: &str) {
        self.append(DiagEntry::new(DiagLevel::Warn, category, message));
    }

    /// 便捷方法:Error 级别。
    pub fn error(&self, category: &str, message: &str) {
        self.append(DiagEntry::new(DiagLevel::Error, category, message));
    }

    /// 快照当前内存 buffer 内容(复制)。
    pub fn snapshot(&self) -> Vec<DiagEntry> {
        self.buffer.lock().map(|b| b.clone()).unwrap_or_default()
    }
}

/// 默认全局 DiagLog 单例(懒加载)。
static GLOBAL_DIAG: std::sync::OnceLock<DiagLog> = std::sync::OnceLock::new();

/// 获取全局 DiagLog 单例。
/// 首次调用时初始化为落盘到 `~/.claw/diag.log` + 内存 256 条。
/// 若环境变量 `CLAW_DISABLE_DIAG_LOG` 设置为 `1` 则退化为纯内存(测试场景)。
pub fn global() -> &'static DiagLog {
    GLOBAL_DIAG.get_or_init(|| {
        if std::env::var("CLAW_DISABLE_DIAG_LOG").as_deref() == Ok("1") {
            return DiagLog::in_memory(64);
        }
        let path = claw_home().join("diag.log");
        DiagLog::with_file(path).unwrap_or_else(|_| DiagLog::in_memory(256))
    })
}

/// 返回 `~/.claw` 目录路径(Windows 优先 `%USERPROFILE%`,其他平台 `$HOME`)。
pub fn claw_home() -> PathBuf {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".claw")
}

/// 安装 panic hook,落盘到 `~/.claw/claw-crash.log`。
///
/// # 设计
/// 提取自 `rusty-claude-cli/src/lib.rs::main_entry()` 内联闭包(§0.1 v2 修正)。
/// `std::panic::set_hook` 是**替换**不是追加,本函数应作为唯一的 panic hook 注册点。
///
/// # 行为
/// 1. 落盘 panic 信息到 `~/.claw/claw-crash.log`(覆盖模式,只保留最后一次)
/// 2. 同步追加一条 Fatal 级别 DiagEntry 到 `global()` 诊断日志
/// 3. eprintln 提示用户 crash log 路径
///
/// # 调用点
/// - `rusty_claude_cli::main_entry()`(替换原内联闭包)
/// - `rusty_claude_cli::src::bin::headless::main()`(§0.2 修复缺口)
/// - 测试入口(可选)
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let payload = info.payload();
        let msg = if let Some(s) = payload.downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = payload.downcast_ref::<String>() {
            s.clone()
        } else {
            "<non-string panic>".to_string()
        };
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown>".to_string());

        let claw_dir = claw_home();
        let _ = std::fs::create_dir_all(&claw_dir);
        let crash_path = claw_dir.join("claw-crash.log");
        let _ = std::fs::write(
            &crash_path,
            format!(
                "PANIC at {location}\nMessage: {msg}\nTimestamp: {}\n",
                now_ms()
            ),
        );
        eprintln!("thread panicked at {location}: {msg}");
        eprintln!("Crash log: {}", crash_path.display());

        // 同步到统一诊断日志
        global().append(
            DiagEntry::new(DiagLevel::Fatal, "panic", &msg)
                .with_field("location", serde_json::Value::String(location))
                .with_field(
                    "crash_log",
                    serde_json::Value::String(crash_path.to_string_lossy().into_owned()),
                ),
        );
    }));
}

/// 返回当前 Unix 毫秒时间戳。
fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diag_entry_json_round_trip() {
        let entry = DiagEntry::new(DiagLevel::Warn, "test", "hello")
            .with_field("key", serde_json::Value::String("value".to_string()));
        let json = entry.to_json_line();
        let parsed: DiagEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.level, DiagLevel::Warn);
        assert_eq!(parsed.category, "test");
        assert_eq!(parsed.message, "hello");
        assert_eq!(parsed.fields["key"], "value");
    }

    #[test]
    fn diag_log_in_memory_respects_cap() {
        let log = DiagLog::in_memory(3);
        log.info("t", "a");
        log.info("t", "b");
        log.info("t", "c");
        log.info("t", "d"); // 应淘汰 a
        let snap = log.snapshot();
        assert_eq!(snap.len(), 3);
        assert_eq!(snap[0].message, "b");
        assert_eq!(snap[2].message, "d");
    }

    #[test]
    fn diag_log_with_file_appends_jsonl() {
        let tmp = std::env::temp_dir().join(format!(
            "claw-diag-test-{}-{}.log",
            std::process::id(),
            now_ms()
        ));
        let log = DiagLog::with_file(tmp.clone()).unwrap();
        log.info("t", "line1");
        log.warn("t", "line2");
        drop(log);

        let content = std::fs::read_to_string(&tmp).unwrap();
        let lines: Vec<&str> = content.trim().lines().collect();
        assert_eq!(lines.len(), 2);
        let first: DiagEntry = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first.message, "line1");
        let second: DiagEntry = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(second.level, DiagLevel::Warn);

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn install_panic_hook_does_not_panic() {
        // 安装 hook 不应本身 panic
        install_panic_hook();
        // 触发一次 catch_unwind 验证 hook 已安装(不强制 crash log 文件存在,
        // 因为 std::panic::set_hook 是 process-global,测试串行执行即可)
        let result = std::panic::catch_unwind(|| {
            panic!("test panic for diag hook");
        });
        assert!(result.is_err());
    }
}
