# Claw Plus

<p align="center">
  <a href="https://github.com/dong382258137/claw-code">dong382258137/claw-code</a>
  ·
  <a href="./USAGE.md">Usage</a>
  ·
  <a href="./rust/README.md">Rust workspace</a>
  ·
  <a href="./PARITY.md">Parity</a>
  ·
  <a href="./ROADMAP.md">Roadmap</a>
  ·
  <a href="./CONTRIBUTING.md">Contributing</a>
  ·
  <a href="./SECURITY.md">Security</a>
  ·
  <a href="https://github.com/dong382258137/claw-code">GitHub</a>
</p>

<p align="center">
  <img src="assets/claw-hero.jpeg" alt="Claw Plus" width="300" />
</p>

## 📜 项目渊源 (Project Lineage)

本项目有清晰的来源链路，每一层都基于上一层进行改进和扩展：

```
Anthropic Claude Code（概念来源 / 架构参考）
  └─ ultraworkers/claw-code（MIT License 开源实现）
       └─ dong382258137/claw-code → Claw Plus（本仓库）
            ├─ 企业审计模块 (Enterprise Audit Module)
            ├─ IM 桥接集成 (IM Bridge)
            └─ 其他功能增强与定制
```

| 层级 | 项目 | 角色 |
|------|------|------|
| 概念层 | [Anthropic Claude Code](https://claude.ai/code) | Anthropic 公司的闭源产品，CLI AI 编程助手的原始概念和架构参考 |
| 实现层 | [ultraworkers/claw-code](https://github.com/ultraworkers/claw-code) | MIT License 开源实现，本项目直接 fork 的基座仓库 |
| 扩展层 | **Claw Plus**（本仓库） | 在上游基础上增加企业审计模块、IM 桥接等功能的二次开发 |

> [!IMPORTANT]
> **本仓库是 [ultraworkers/claw-code](https://github.com/ultraworkers/claw-code) 的 fork（MIT License）。**
> - 上游版权 © 2026 UltraWorkers and Claw Plus contributors
> - 下游修改版权 © 2026 dong382258137（企业审计模块及下游改动）
> - "Claude" 和 "Claude Code" 是 Anthropic 的商标，本项目与 Anthropic 无关联，亦非其官方产品

## 参考与借鉴的项目 (Referenced Projects)

以下项目为本项目的设计和实现提供了重要的参考和灵感，但它们是**独立维护的第三方项目**，各有自己的作者和许可证：

| 项目 | 用途 | 作者/维护者 |
|------|------|-------------|
| [clawhip](https://github.com/Yeachan-Heo/clawhip) | 事件与通知路由系统 | [Yeachan-Heo](https://github.com/Yeachan-Heo) |
| [oh-my-openagent](https://github.com/code-yeongyu/oh-my-openagent) | 多智能体协调框架 | [code-yeongyu](https://github.com/code-yeongyu) |
| [oh-my-codex](https://github.com/Yeachan-Heo/oh-my-codex) | 工作流执行层 | [Yeachan-Heo](https://github.com/Yeachan-Heo) |
| [oh-my-claudecode](https://github.com/Yeachan-Heo/oh-my-claudecode) | Claude Code 工作流 | [Yeachan-Heo](https://github.com/Yeachan-Heo) |

> 以上项目均为独立开源项目。本仓库的 PHILOSOPHY.md 等文档中提及它们是为了说明多智能体协作生态的完整图景，并非声称对其拥有所有权。

## 本仓库的独立贡献 (Original Contributions)

在继承上游代码的基础上，本仓库增加了以下原创功能：

- **企业审计模块 (Enterprise Audit Module)** — 参见 [`docs/enterprise-audit-module-design.md`](./docs/enterprise-audit-module-design.md)
- **IM 桥接集成 (IM Bridge)** — 位于 `rust/crates/im-bridge/`
- **其他功能增强与定制**

---

Claw Plus is a Rust implementation of the `claw-plus` CLI agent harness.
The canonical implementation lives in [`rust/`](./rust). This fork builds on
the upstream [ultraworkers/claw-code](https://github.com/ultraworkers/claw-code)
project with additional features and modifications.

> [!IMPORTANT]
> Start with [`USAGE.md`](./USAGE.md) for build, auth, CLI, session, and parity-harness workflows. For file submission/navigation questions, see [Navigation and file context](./docs/navigation-file-context.md). For local OpenAI-compatible models and offline skill installs, see [Local OpenAI-compatible providers and skills setup](./docs/local-openai-compatible-providers.md). Windows users can jump to the PowerShell-first [Windows install and release quickstart](./docs/windows-install-release.md). Make `claw doctor` your first health check after building, use [`rust/README.md`](./rust/README.md) for crate-level details, read [`PARITY.md`](./PARITY.md) for the current Rust-port checkpoint, and see [`docs/container.md`](./docs/container.md) for the container-first workflow.
>
> **ACP / Zed status:** `claw-code` does not ship an ACP/Zed daemon or JSON-RPC entrypoint yet. Run `claw acp` (or `claw --acp`) for the current status instead of guessing from source layout; `claw acp serve` is currently a discoverability alias only, returns status with exit code 0, and real ACP support remains tracked separately in `ROADMAP.md`. For the public JSON contract, see [`docs/g011-acp-json-rpc-status-contract.md`](./docs/g011-acp-json-rpc-status-contract.md).

## Current repository shape

- **`rust/`** — canonical Rust workspace and the `claw-plus` CLI binary
- **`USAGE.md`** — task-oriented usage guide for the current product surface
- **`PARITY.md`** — Rust-port parity status and migration notes
- **`ROADMAP.md`** — active roadmap and cleanup backlog
- **`PHILOSOPHY.md`** — project intent and system-design framing
- **`src/` + `tests/`** — companion Python/reference workspace and audit helpers; not the primary runtime surface

## Quick start

> [!WARNING]
> **`cargo install claw-code` installs the wrong thing.** The `claw-code` crate on crates.io is a deprecated stub that places `claw-code-deprecated.exe` — not `claw-plus`. Running it only prints `"claw-code has been renamed to agent-code"`. **Do not use `cargo install claw-code`.** Either build from source (this repo) or install the upstream binary:
> ```bash
> cargo install agent-code   # upstream binary — installs 'agent.exe' (Windows) / 'agent' (Unix), NOT 'agent-code'
> ```
> This repo is the actively maintained fork — follow the steps below to build from source.

```bash
# 1. Clone and build
git clone https://github.com/dong382258137/claw-code
cd claw-code/rust
cargo build --workspace

# 2. Set your API key (Anthropic API key — not a Claude subscription)
export ANTHROPIC_API_KEY="sk-ant-..."

# 3. Verify everything is wired correctly
./target/debug/claw-plus doctor

# 4. Run a prompt
./target/debug/claw-plus prompt "say hello"
```

> [!NOTE]
> **Windows (PowerShell):** the binary is `claw.exe`, not `claw-plus`. Use `.\target\debug\claw-plus.exe` or run `cargo run -- prompt "say hello"` to skip the path lookup.

### Windows setup

**PowerShell is a supported Windows path.** Use whichever shell works for you. The common onboarding issues on Windows are:

1. **Install Rust first** — download from <https://rustup.rs/> and run the installer. Close and reopen your terminal when it finishes.
2. **Verify Rust is on PATH:**
   ```powershell
   cargo --version
   ```
   If this fails, reopen your terminal or run the PATH setup from the Rust installer output, then retry.
3. **Clone and build** (works in PowerShell, Git Bash, or WSL):
   ```powershell
   git clone https://github.com/dong382258137/claw-code
   cd claw-code/rust
   cargo build --workspace
   ```
4. **Run** (PowerShell — note `.exe` and backslash):
   ```powershell
   $env:ANTHROPIC_API_KEY = "sk-ant-..."
   .\target\debug\claw-plus.exe prompt "say hello"
   ```

For release ZIPs, PATH setup, provider switching, and notification smoke checks, see [`docs/windows-install-release.md`](./docs/windows-install-release.md).

**Git Bash / WSL** are optional alternatives, not requirements. If you prefer bash-style paths (`/c/Users/you/...` instead of `C:\Users\you\...`), Git Bash (ships with Git for Windows) works well. In Git Bash, the `MINGW64` prompt is expected and normal — not a broken install.

## Post-build: locate the binary and verify

After running `cargo build --workspace`, the `claw-plus` binary is built but **not** automatically installed to your system. Here's where to find it and how to verify the build succeeded.

### Binary location

After `cargo build --workspace` in `claw-code/rust/`:

**Debug build (default, faster compile):**
- **macOS/Linux:** `rust/target/debug/claw-plus`
- **Windows:** `rust/target/debug/claw-plus.exe`

**Release build (optimized, slower compile):**
- **macOS/Linux:** `rust/target/release/claw-plus`
- **Windows:** `rust/target/release/claw-plus.exe`

If you ran `cargo build` without `--release`, the binary is in the `debug/` folder.

### Verify the build succeeded

Test the binary directly using its path:

```bash
# macOS/Linux (debug build)
./rust/target/debug/claw-plus --help
./rust/target/debug/claw-plus doctor

# Windows PowerShell (debug build)
.\rust\target\debug\claw.exe --help
.\rust\target\debug\claw.exe doctor
```

PowerShell smoke commands that do not require live credentials:

```powershell
$env:CLAW_CONFIG_HOME = Join-Path $env:TEMP "claw config home"
New-Item -ItemType Directory -Force -Path $env:CLAW_CONFIG_HOME | Out-Null
Remove-Item Env:\ANTHROPIC_API_KEY, Env:\ANTHROPIC_AUTH_TOKEN, Env:\OPENAI_API_KEY -ErrorAction SilentlyContinue
.\rust\target\debug\claw.exe help
.\rust\target\debug\claw.exe status
.\rust\target\debug\claw.exe config env
.\rust\target\debug\claw.exe doctor
```

If these commands succeed, the build is working. `claw doctor` is your first health check — it validates your API key, model access, and tool configuration.

### Optional: Add to PATH

If you want to run `claw-plus` from any directory without the full path, choose one of these approaches:

**Option 1: Symlink (macOS/Linux)**
```bash
ln -s $(pwd)/rust/target/debug/claw-plus /usr/local/bin/claw-plus
```
Then reload your shell and test:
```bash
claw --help
```

**Option 2: Use `cargo install` (all platforms)**

Build and install to Cargo's default location (`~/.cargo/bin/`, which is usually on PATH):
```bash
# From the claw-code/rust/ directory
cargo install --path . --force

# Then from anywhere
claw --help
```

**Option 3: Update shell profile (bash/zsh)**

Add this line to `~/.bashrc` or `~/.zshrc`:
```bash
export PATH="$(pwd)/rust/target/debug:$PATH"
```

Reload your shell:
```bash
source ~/.bashrc  # or source ~/.zshrc
claw --help
```

### Troubleshooting

- **"command not found: claw"** — The binary is in `rust/target/debug/claw-plus`, but it's not on your PATH. Use the full path `./rust/target/debug/claw-plus` or symlink/install as above.
- **"permission denied"** — On macOS/Linux, you may need `chmod +x rust/target/debug/claw-plus` if the executable bit isn't set (rare).
- **Debug vs. release** — If the build is slow, you're in debug mode (default). Add `--release` to `cargo build` for faster runtime, but the build itself will take 5–10 minutes.

> [!NOTE]
> **Auth:** claw requires an **API key** (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, etc.) — Claude subscription login is not a supported auth path.

Run the workspace test suite after verifying the binary works:

```bash
cd rust
cargo test --workspace
```

## Documentation map

- [`USAGE.md`](./USAGE.md) — quick commands, auth, sessions, config, parity harness
- [`docs/navigation-file-context.md`](./docs/navigation-file-context.md) — terminal navigation, scrollback, `@path` file context, attachments, and secret-safety guidance
- [`docs/local-openai-compatible-providers.md`](./docs/local-openai-compatible-providers.md) — Ollama/llama.cpp/vLLM setup, Claw multi-provider positioning, and local skills install checks
- [`docs/windows-install-release.md`](./docs/windows-install-release.md) — PowerShell-first install, release artifact, provider switching, and Windows/WSL notification smoke paths
- [`rust/README.md`](./rust/README.md) — crate map, CLI surface, features, workspace layout
- [`PARITY.md`](./PARITY.md) — parity status for the Rust port
- [`rust/MOCK_PARITY_HARNESS.md`](./rust/MOCK_PARITY_HARNESS.md) — deterministic mock-service harness details
- [`ROADMAP.md`](./ROADMAP.md) — active roadmap and open cleanup work
- [`docs/g004-events-reports-contract.md`](./docs/g004-events-reports-contract.md) — Stream 2 lane event/report contract guidance for consumers
- [`docs/enterprise-audit-module-design.md`](./docs/enterprise-audit-module-design.md) — enterprise audit module design (v1.0, fork-specific)
- [`PHILOSOPHY.md`](./PHILOSOPHY.md) — why the project exists and how it is operated
- [`CONTRIBUTING.md`](./CONTRIBUTING.md), [`SECURITY.md`](./SECURITY.md), [`SUPPORT.md`](./SUPPORT.md), and [`CODE_OF_CONDUCT.md`](./CODE_OF_CONDUCT.md) — contribution, vulnerability-reporting, support, and community policies
- [`LICENSE`](./LICENSE) — MIT license for this repository

## Ecosystem

This fork is part of a broader ecosystem of projects. The upstream base is
[ultraworkers/claw-code](https://github.com/ultraworkers/claw-code) (MIT License).
Related projects in the ecosystem (independently maintained by their respective authors):

- [clawhip](https://github.com/Yeachan-Heo/clawhip) — event and notification routing (by [Yeachan-Heo](https://github.com/Yeachan-Heo))
- [oh-my-openagent](https://github.com/code-yeongyu/oh-my-openagent) — multi-agent coordination (by [code-yeongyu](https://github.com/code-yeongyu))
- [oh-my-claudecode](https://github.com/Yeachan-Heo/oh-my-claudecode) — Claude Code workflow (by [Yeachan-Heo](https://github.com/Yeachan-Heo))
- [oh-my-codex](https://github.com/Yeachan-Heo/oh-my-codex) — workflow execution (by [Yeachan-Heo](https://github.com/Yeachan-Heo))

## Ownership / affiliation disclaimer

- 本项目基于 MIT License 的 [ultraworkers/claw-code](https://github.com/ultraworkers/claw-code) 进行二次开发，尊重并保留上游版权声明。
- This repository does **not** claim ownership of the original Claude Code source material.
- This repository is **not affiliated with, endorsed by, or maintained by Anthropic**.
- "Claude" and "Claude Code" are trademarks of Anthropic.
- This is an independent fork of the MIT-licensed [ultraworkers/claw-code](https://github.com/ultraworkers/claw-code) project.
- 参考项目（clawhip、oh-my-openagent 等）均为独立开源项目，各有自己的作者和许可证，本仓库引用它们不代表对其拥有所有权。
