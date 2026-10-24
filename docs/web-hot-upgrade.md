# Web UI 热升级（web-aware hot upgrade）

## 行为

Agent 在 Web 会话里调用 `HotUpgrade` 工具后：

1. 当前 turn 正常收尾、会话落盘；
2. `claw serve` 写 `.claw/upgrade-request.json`（含 `web_port` 与 `web_token`）；
3. spawn `--upgrade-helper`（独立控制台）：等旧进程退出 → 备份 exe → `cargo build` → 回拷部署 → 启动新进程；
4. 旧 serve 写 `.claw/upgrade-exited.json` 后 `exit(0)`；
5. helper 以 **相同端口、相同 token** 启动 `claw serve`，并清掉 upgrade 标记；
6. 页面 EventSource 自动重连；前端轮询 `/api/meta` 成功后 `location.reload()`，加载新前端。

## 关键实现点

- **token 复用**：helper 通过环境变量 `CLAW_WEB_TOKEN` 把旧 token 传给新进程；`serve` 只接受 32 位 hex token 并复用它，因此旧 URL 继续有效。手动启动仍生成新 token。
- **端口接管**：旧进程退出后 Windows 可能短暂保留监听条目；升级模式下 `serve` 会最多重试 bind 10 秒再退回随机端口。
- **句柄继承**：监听 socket 在启动时调用 `SetHandleInformation(..., HANDLE_FLAG_INHERIT, 0)`，否则 helper/PTY/git 子进程会继承 LISTEN 句柄，导致新进程永远 bind 不上。
- **入口同步**：部署目录同时存在 `claw.exe` 与 `claw-web.exe` 时，helper 会把构建产物同步到两个入口，避免快捷方式仍启动旧版本。
- **前端提示**：SSE 收到 `{"kind":"upgrading"}` 后显示「正在热升级…」，最多等 4 分钟；服务恢复后自动 reload，超时提示查看 `.claw/upgrade-helper.log`。

## 限制

- 编译失败时旧进程已退出：新服务不会起来，需查看 `.claw/upgrade-helper.log`，并用 `claw.exe.old` 恢复。
- 升级窗口（helper 控制台）会驻留到编译完成；Web 服务本身以 detached 方式启动，关闭该窗口不影响服务。
- 页面 reload 会丢失输入框里未发送的草稿。
- 源码根依赖 `~/.claw/source-root` 注册表（首次在源码树内升级会自动登记）。

## 手动验证

```powershell
# 1. 启动 serve（debug 或 release）
claw serve --port 4399 --cwd D:\claw-code-src --no-open
# 2. 在 Web 里让 agent 调用 HotUpgrade
# 3. 观察端口与 token 不变：
curl.exe -s "http://127.0.0.1:4399/api/meta?t=<旧token>"
Get-Content D:\claw-code-src\.claw\upgrade-helper.log -Tail 5
```
