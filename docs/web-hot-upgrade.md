# Web UI 热升级（web-aware hot upgrade）

## 行为（P1：构建成功才切换）

Agent 在 Web 会话里调用 `HotUpgrade` 工具后：

1. 工具循环在调用边界挂起：已执行的工具结果照常落盘，`HotUpgrade` 本身不写结果；
2. `claw serve` 写 `.claw/upgrade-request.json`（含 `web_port` / `web_token` / 挂起现场 / 排队消息），会话标记 `suspended`；
3. spawn `--upgrade-helper`（独立控制台）：**旧进程保持运行**，helper 先改名释放 exe 路径，再 `cargo build`；
4. 构建失败：helper 写 `.claw/upgrade-result.json`（`ok=false` + 编译错误末尾），旧进程轮询到后补一条 `HotUpgrade` 错误结果，**原地继续任务**（模型当场修代码重试），不重启、不丢任务；
5. 构建成功：helper 写结果（`ok=true`）+ 调 `POST /api/upgrade/commit`；旧 serve 写 `.claw/upgrade-exited.json` 后 `exit(0)`；
6. helper 以 **相同端口、相同 token** 启动 `claw serve`，并做 `/api/meta` 健康检查；起不来时自动回滚 `exe.old` 并启动旧版本；
7. 新进程启动恢复：补 `HotUpgrade` 的“升级完成”结果、同批未执行调用补“未执行”，恢复计划，自动入队续跑；页面 EventSource 自动重连，前端检测 `build_id` 变化后 `location.reload()`。

## 关键实现点

- **token 复用**：helper 通过环境变量 `CLAW_WEB_TOKEN` 把旧 token 传给新进程；`serve` 只接受 32 位 hex token 并复用它，因此旧 URL 继续有效。手动启动仍生成新 token。
- **端口接管**：旧进程退出后 Windows 可能短暂保留监听条目；升级模式下 `serve` 会最多重试 bind 10 秒再退回随机端口。
- **句柄继承**：监听 socket 在启动时调用 `SetHandleInformation(..., HANDLE_FLAG_INHERIT, 0)`，否则 helper/PTY/git 子进程会继承 LISTEN 句柄，导致新进程永远 bind 不上。
- **入口同步**：部署目录只保留 `claw.exe`，桌面快捷方式直接指向 `claw.exe serve`，热升级替换 `claw.exe` 后快捷方式自然生效；历史入口 `claw-plus.exe` / `claw-web.exe` 由部署脚本清理。
- **前端提示**：SSE 收到 `{"kind":"upgrading"}` 后显示「正在热升级…」；模型调用 `HotUpgrade` 时立即提示；续跑成功后收到 `{"kind":"upgrade_resumed"}` 清除横幅。
- **构建结果回传**：`.claw/upgrade-result.json` 是新旧进程之间的构建结果通道（`ok` / `elapsed_ms` / `stderr_tail`）；旧进程只在 `Building` 阶段接受 `POST /api/upgrade/commit`，已取消的升级返回 409，helper 不会启动新进程。
- **exe 释放顺序**：Web 模式下 helper 先复制备份、再把运行中的 exe 改名为 `.old`（Windows 允许重命名运行中的镜像），然后构建；构建失败改回原名。
- **前端自动构建**：`web/src` 比 `web/dist/index.html` 新时，helper 先执行 `npm run build`（没有 npm 时跳过并沿用旧 dist；构建失败则中止升级并回传错误）。
- **升级循环保护**：同一会话连续 3 次触发 `HotUpgrade` 都未成功时暂停自动升级，把原因作为错误结果回传模型并继续任务；用户再发消息后计数清零，可再次尝试。

## 限制

- 构建期间该会话保持 `suspended`，新消息会被拒绝（排在队列里的消息已随请求持久化，重启后重新入队）。
- 构建失败时不会重启，任务在旧版本中继续；模型需要按回传的编译错误修复后再次调用 `HotUpgrade`。
- 新版本启动健康检查失败会自动回滚旧版本（会话仍会恢复并继续任务，提示中会说明升级未完成）。
- 升级窗口（helper 控制台）会驻留到编译完成；Web 服务本身以 detached 方式启动，关闭该窗口不影响服务。
- 页面 reload 仍可能丢失输入框里未发送的草稿（仅限浏览器内未提交内容）。
- 源码根依赖 `~/.claw/source-root` 注册表（首次在源码树内升级会自动登记）。
- 首次由旧二进制发起升级时仍是旧行为；本次构建之后的下一次升级才具备“构建成功才切换 + 失败原地续跑”。

## 手动验证

```powershell
# 1. 启动 serve（debug 或 release）
claw serve --port 4399 --cwd D:\claw-code-src --no-open
# 2. 在 Web 里让 agent 调用 HotUpgrade
# 3. 观察端口与 token 不变：
curl.exe -s "http://127.0.0.1:4399/api/meta?t=<旧token>"
Get-Content D:\claw-code-src\.claw\upgrade-helper.log -Tail 5
```
