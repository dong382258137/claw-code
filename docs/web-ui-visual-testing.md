# Web UI 视觉验证（无头浏览器）安全规程

## 背景

CLAW 的 Web UI 默认端口是 **4317**（`claw serve --port`，只绑定 `127.0.0.1`）；端口被占时会自动退到系统分配端口，不会抢占其他程序。

2026-10-24 做 UI 截图验证时，脚本用 Edge 无头模式 + `--remote-debugging-port=9222` 启动浏览器。但 **Trae CN 自己占用了 9222**（Electron 的 DevTools 端口），Edge 无法绑定，脚本随后访问 `http://127.0.0.1:9222/json/list` 拿到的是 **Trae 的调试目标**，并把其中一个 Trae 工作台窗口导航到了 CLAW 页面（target 标题变成 `CLAW`，URL 变成 `http://127.0.0.1:4399/?t=...`）。

这不是 CLAW 端口冲突，也不是 CLAW 修改了 Trae；是自动化脚本误连了别的程序的调试端口。

## 必须遵守的规则

1. **不要使用 9222**（Trae CN 在用），也不要使用 9876 / 51000 等 Trae 监听端口。
   截图脚本固定使用 19300–19400 区间的高位端口。
2. 启动浏览器后，先 `GET /json/version`，断言 `Browser` 以 `Edg/` 开头；不是就立刻退出。
3. 只操作 URL 以 `http://127.0.0.1:4317`（或本次测试端口）开头的 target；
   **绝不 navigate/close 其他 target**（尤其是 `vscode-file://`、`file://`）。
4. 浏览器进程必须在 `finally` 里杀掉，避免残留占用端口。

## 参考脚本（Node，内置 fetch/WebSocket）

```js
const PORT = 19377 // 不要用 9222
const ORIGIN = 'http://127.0.0.1:4317'
const version = await (await fetch(`http://127.0.0.1:${PORT}/json/version`)).json()
if (!String(version.Browser || '').startsWith('Edg/')) throw new Error('not our Edge: ' + version.Browser)
const targets = await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json()
const page = targets.find((t) => t.type === 'page' && String(t.url).startsWith(ORIGIN))
if (!page) throw new Error('CLAW page not found; open it in the launched browser first')
// 后续只连 page.webSocketDebuggerUrl，且只用 Page/Runtime 域
```

启动命令示例：

```powershell
msedge.exe --headless=new --disable-gpu --remote-allow-origins=* `
  --user-data-dir="$env:TEMP\claw-edge-safe" --remote-debugging-port=19377 `
  "http://127.0.0.1:4317/?t=<token>"
```

## 事故恢复记录

- 误导航的 Trae target：`F33AF89A…`，已用 CDP `Page.navigate` 恢复到
  `vscode-file://vscode-app/d:/Trae%20CN/resources/app/out/vs/code/electron-browser/workbench/workbench.html`。
- 恢复后 `/json/list` 中不再有 `127.0.0.1:4399` 的 target。
