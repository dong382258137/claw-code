//! verify-lsp:对比 regex 版 repomap 与 LSP references 增强版。
//!
//! 用法(在 demo-monorepo 目录):
//! ```bash
//! cargo run -p verify-lsp
//! ```
//!
//! 前置条件:
//! - `rust-analyzer` 在 PATH 中(已通过 `rustup component add rust-analyzer` 安装)
//! - 首次运行 rust-analyzer 需要索引整个 workspace,可能耗时 30-60 秒
//!
//! 输出:
//! - **Regex 版**:旧的 substring 匹配(regex 提取 + use 语句 contains 匹配)。
//!   对 `use demo_core::{a,b,c}` 组导入,regex 只捕获 `demo_core`(不含函数名),
//!   因此跨模块引用基本数不到 → 排序按字母序,无区分度。
//! - **LSP 版**:`textDocument/references` 语义引用,精确区分跨模块引用。
//!   demo 预期排名:core > utils > app > api(被引用越多越靠前)。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use runtime::lsp_client::{LspRegistry, LspServerStatus};
use runtime::RepoMap;

fn main() {
    // demo-monorepo 根目录(与 verify-lsp 同级的 workspace 根)
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("verify-lsp should be a direct child of demo-monorepo")
        .to_path_buf();

    println!("=== 验证 demo-monorepo 的 LSP 跨模块引用 ===");
    println!("root: {}", root.display());
    println!();

    // ---- 1. Regex 版 ----
    let t0 = Instant::now();
    let mut regex_map = RepoMap::new(&root).with_max_tokens(1024);
    let regex_out = regex_map.render();
    println!("--- [Regex 版] render() 耗时 {:?} ---", t0.elapsed());
    println!("{regex_out}");
    println!();

    // ---- 2. Spawn rust-analyzer ----
    let registry = LspRegistry::new();
    registry.register_with_command(
        "rust",
        LspServerStatus::Disconnected,
        Some(root.to_str().unwrap()),
        vec![],
        "rust-analyzer",
    );
    println!("--- 启动 rust-analyzer(首次索引可能 30-60s)---");
    let t_spawn = Instant::now();
    match registry.spawn_server("rust", "rust-analyzer", &[], root.to_str().unwrap()) {
        Ok(()) => println!("rust-analyzer connected ({:?})", t_spawn.elapsed()),
        Err(e) => {
            println!("rust-analyzer spawn failed: {e}");
            println!("请确认已安装:rustup component add rust-analyzer");
            return;
        }
    }

    // 给 rust-analyzer 一点时间完成索引,再渲染 LSP 版。
    println!("--- 等待索引稳定(30s)---");
    std::thread::sleep(Duration::from_secs(30));

    // ---- 3. LSP 版 ----
    let t1 = Instant::now();
    let mut lsp_map = RepoMap::new(&root).with_max_tokens(1024);
    let lsp_out = lsp_map.render_with_lsp(&registry);
    println!("--- [LSP 版] render_with_lsp() 耗时 {:?} ---", t1.elapsed());
    println!("{lsp_out}");
    println!();

    // ---- 4. 清理 ----
    let _ = registry.shutdown_server("rust");
    println!("=== 完成。对比上面两段的排名差异即可看到 LSP 语义引用的效果 ===");
}
