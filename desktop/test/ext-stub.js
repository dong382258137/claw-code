/**
 * 联调扩展桩（不依赖浏览器/DeepSeek 登录）
 *
 * 模拟壳（claw-desktop）的 WS 协议行为，连接真实的 claw-web-bridge 守护进程：
 *   1. 发送 user_run："读取 CLAUDE.md 并总结"
 *   2. 收到 inject_prompt 第 1 轮 → 以 ```claw 块回复 await read(...)
 *      （claw 守护在本机真实执行 read，工具结果会进入下一轮 conversation）
 *   3. 收到 inject_prompt 第 2 轮（含 <|tool_result|> 文件内容）→ 纯文字总结回复
 *   4. 守护进程发 Final → 断言包含桩的总结标记，打印全程注入轮次
 *
 * 用法：node test/ext-stub.js   （守护进程默认 127.0.0.1:18793）
 */
const WebSocket = require('ws');

const URL = 'ws://127.0.0.1:18793/ws/ext?token=dev-claw-token';
const CLAUDE_MD = 'D:/claw-code-src/rust/CLAUDE.md';
const MARK = 'STUB_SUMMARY: CLAUDE.md 阅读完毕';

const ws = new WebSocket(URL);
let rounds = 0;

function send(obj) {
  ws.send(JSON.stringify(obj));
  console.log('[stub]->', JSON.stringify(obj).slice(0, 120));
}

ws.on('open', () => {
  console.log('[stub] connected');
  send({ type: 'user_run', text: `读取本地文件 ${CLAUDE_MD} 的内容，并总结它。` });
});

ws.on('message', (data) => {
  const msg = JSON.parse(String(data));
  console.log('[stub]<-.', JSON.stringify(msg).slice(0, 160));
  if (msg.type === 'inject_prompt') {
    rounds += 1;
    if (rounds === 1) {
      // 第 1 轮：让守护进程真实执行 read（await read(...) 协议）
      send({
        type: 'reply',
        turn_id: msg.turn_id,
        text: '```claw\nconst t = await read("' + CLAUDE_MD + '");\nt;\n```',
      });
    } else {
      // 第 2 轮起：纯文字总结（无代码块 → 守护进程结束回合）
      send({ type: 'reply', turn_id: msg.turn_id, text: MARK });
    }
  } else if (msg.type === 'final') {
    console.log('\n===== Final =====');
    console.log(msg.text);
    console.log('=================');
    if (msg.text.includes(MARK)) {
      console.log('\nPASS: 真实守护进程 + WS 协议 + await read(...) 本地执行闭环跑通（注入轮次=' + rounds + '）');
      process.exit(0);
    } else {
      console.log('\nFAIL: Final 不含桩总结标记');
      process.exit(1);
    }
  }
});

ws.on('error', (err) => {
  console.error('[stub] ws 错误(守护进程未启动？):', err.message);
  process.exit(1);
});

setTimeout(() => {
  console.error('[stub] 90s 超时');
  process.exit(1);
}, 90000).unref();