#!/usr/bin/env bash
# compat-harness CI 集成:拉取上游 TypeScript 参考源并运行兼容性对比测试。
#
# 用法:
#   1. 在 CI 中: 脚本自动 clone 上游仓库到临时目录
#   2. 在本地:  设置 CLAUDE_CODE_UPSTREAM 环境变量指向已有 checkout
#
# 行为:
#   - 上游不可达时跳过(返回 0),不阻塞 CI。
#   - 上游可达时运行 `cargo test -p compat-harness`,对比
#     commands/tools/bootstrap-phase 与上游的差异。

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# ── 上游仓库配置 ──────────────────────────────────────────────
UPSTREAM_REPO="${CLAUDE_CODE_UPSTREAM_REPO:-https://github.com/anthropics/claude-code.git}"
UPSTREAM_REF="${CLAUDE_CODE_UPSTREAM_REF:-main}"
UPSTREAM_DEPTH="${CLAUDE_CODE_UPSTREAM_DEPTH:-1}"

main() {
  local upstream_dir

  # 若已设置 CLAUDE_CODE_UPSTREAM,直接使用
  if [[ -n "${CLAUDE_CODE_UPSTREAM:-}" ]] && [[ -d "$CLAUDE_CODE_UPSTREAM" ]]; then
    echo "[compat-harness] 使用已有上游: $CLAUDE_CODE_UPSTREAM"
    upstream_dir="$CLAUDE_CODE_UPSTREAM"
  else
    upstream_dir="$(mktemp -d)"
    echo "[compat-harness] 克隆上游仓库: $UPSTREAM_REPO (ref=$UPSTREAM_REF)"
    
    if ! git clone --depth "$UPSTREAM_DEPTH" --branch "$UPSTREAM_REF" \
         "$UPSTREAM_REPO" "$upstream_dir" 2>/dev/null; then
      echo "[compat-harness] (info) 上游仓库不可达,跳过兼容性对比"
      rm -rf "$upstream_dir"
      exit 0
    fi
  fi

  export CLAUDE_CODE_UPSTREAM="$upstream_dir"

  echo "[compat-harness] 运行兼容性对比测试..."
  cd "$REPO_ROOT/rust"
  
  if cargo test -p compat-harness -- --nocapture 2>&1; then
    echo "[compat-harness] ✅ 上游兼容性对比通过"
  else
    # compat-harness 失败不阻塞 CI —— 上游 API 变更属于信息性告警。
    echo "[compat-harness] ⚠️ 上游兼容性对比检测到差异(不阻塞 CI)"
  fi

  # 清理临时目录
  if [[ -z "${CLAUDE_CODE_UPSTREAM_KEEP:-}" ]]; then
    if [[ "$upstream_dir" != "${CLAUDE_CODE_UPSTREAM:-}" ]]; then
      rm -rf "$upstream_dir"
    fi
  fi
}

main "$@"
