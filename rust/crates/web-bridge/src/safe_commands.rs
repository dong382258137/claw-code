//! 危险命令预检:解析出的 shell 命令在执行前过黑名单,命中即拒绝。
//!
//! 网页模型输出是"不可信"输入(可能被网页内容诱导),因此任何想执行的
//! 命令都要先经过这里,拒绝破坏性/系统性命令,避免"模型照着网页内容
//! 让删库就删库"。参考 cuckoo-code `dangerous-commands.js` 的规则思路,
//! 按本项目实际风险自清单实现。

/// 检查命令是否命中危险黑名单。命中返回拒绝原因描述,安全返回 `None`。
#[must_use]
pub fn check(command: &str) -> Option<&'static str> {
    let c = command.trim().to_ascii_lowercase();
    const RULES: &[(&str, &str)] = &[
        // 破坏性命令
        ("format c:", "格式化 C 盘属于不可恢复的破坏性操作"),
        ("format ", "格式化磁盘属于不可恢复的破坏性操作"),
        ("del /s /q", "递归删除文件属于破坏性操作"),
        ("rd /s /q", "递归删除目录属于破坏性操作"),
        ("deltree", "递归删除目录属于破坏性操作"),
        ("rm -rf /", "删除根目录属于破坏性操作"),
        ("rm -rf ~", "删除用户主目录属于破坏性操作"),
        ("rm -r /", "递归删除根目录属于破坏性操作"),
        ("mkfs.", "创建文件系统会抹掉数据"),
        ("dd if=", "dd 写入磁盘属于破坏性操作"),
        (":(){", "fork 炸弹会导致系统崩溃"),
        // 系统级操作
        ("shutdown", "关机/重启命令需要用户明确确认"),
        ("reboot", "重启命令需要用户明确确认"),
        ("poweroff", "关机命令需要用户明确确认"),
        ("taskkill /f /im", "强制终止进程属于系统级操作"),
        ("kill -9", "强制终止进程属于系统级操作"),
        ("reg delete", "删除注册表项属于系统级操作"),
        ("net user", "用户账户管理属于系统级操作"),
        ("chkdsk /f", "磁盘检查修复属于系统级操作"),
        // 高影响组合
        ("diskpart", "磁盘分区操作属于系统级操作"),
        (";rm", "删除命令的组合注入"),
        ("&& rm", "删除命令的组合注入"),
    ];
    for (pat, desc) in RULES {
        if c.contains(pat) {
            return Some(desc);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_destructive_commands() {
        assert!(check("format C:").is_some());
        assert!(check("rm -rf /").is_some());
        assert!(check("del /s /q C:\\temp").is_some());
        assert!(check("rd /s /q C:\\temp").is_some());
        assert!(check("shutdown /s").is_some());
    }

    #[test]
    fn allows_normal_commands() {
        assert!(check("echo hello").is_none());
        assert!(check("type Cargo.toml").is_none());
        assert!(check("cat file.txt").is_none());
        assert!(check("dir /b src").is_none());
    }
}
