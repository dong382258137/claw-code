//! 用户级本地设置（单人自用，无云端同步）。
//!
//! 存储位置：`~/.claw/settings.json`（Windows 为 `%USERPROFILE%\.claw\settings.json`）。
//! 目录不可写或缺省 HOME 时静默降级为内存默认值 —— 设置读取失败绝不阻断启动。

use serde::{Deserialize, Serialize};
use std::io;
use std::path::PathBuf;

/// 持久化的用户设置。新增字段必须带 `#[serde(default)]`，保证旧文件可读。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Settings {
    /// vim 编辑模式（`/vim` 切换）。
    #[serde(default)]
    pub vim: bool,
}

/// 设置文件路径；无法确定主目录时返回 `None`。
#[must_use]
pub fn path() -> Option<PathBuf> {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .filter(|value| !value.is_empty())?;
    Some(PathBuf::from(home).join(".claw").join("settings.json"))
}

/// 读取设置；文件缺失、损坏或路径不可用都返回默认值。
#[must_use]
pub fn load() -> Settings {
    let Some(path) = path() else {
        return Settings::default();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Settings::default();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

/// 写回设置；自动创建目录。
pub fn save(settings: &Settings) -> io::Result<()> {
    let Some(path) = path() else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no home directory for settings",
        ));
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(settings)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    std::fs::write(&path, text)
}

/// 只更新 vim 开关并落盘；失败静默（会话内状态已经生效）。
pub fn set_vim(enabled: bool) {
    let mut settings = load();
    settings.vim = enabled;
    let _ = save(&settings);
}

#[cfg(test)]
mod tests {
    use super::Settings;

    #[test]
    fn defaults_to_vim_disabled() {
        assert!(!Settings::default().vim);
    }

    #[test]
    fn settings_round_trip_through_json() {
        let settings = Settings { vim: true };
        let text = serde_json::to_string(&settings).expect("serialize");
        let restored: Settings = serde_json::from_str(&text).expect("deserialize");
        assert!(restored.vim);
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let restored: Settings =
            serde_json::from_str(r#"{"vim": true, "future_key": 1}"#).expect("deserialize");
        assert!(restored.vim);
    }

    #[test]
    fn missing_vim_key_falls_back_to_default() {
        let restored: Settings = serde_json::from_str("{}").expect("deserialize");
        assert!(!restored.vim);
    }
}
