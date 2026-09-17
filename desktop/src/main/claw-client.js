/**
 * claw 守护进程 WS 客户端（主进程）
 *
 * 连接 ws://127.0.0.1:<port>/ws/ext?token=<token>，把网页壳变成 claw 的模型后端。
 * 协议与服务端（rust/crates/web-bridge/src/server.rs）一一对应：
 *   服务器→壳: inject_prompt{turn_id,text} / final{text} / hello_ok / error{message}
 *   壳→服务器: user_run{text} / reply{turn_id,text} / hello / diag{payload}
 *
 * 断线自动重连（指数退避 1s→10s），单扩展连接，后连覆盖先连。
 */
const WebSocket = require('ws');

const DEFAULT_CFG = { host: '127.0.0.1', port: 18793, token: 'dev-claw-token' };

function loadConfig() {
  return {
    host: process.env.CLAW_BRIDGE_HOST || DEFAULT_CFG.host,
    port: parseInt(process.env.CLAW_BRIDGE_PORT, 10) || DEFAULT_CFG.port,
    token: process.env.CLAW_BRIDGE_TOKEN || DEFAULT_CFG.token,
  };
}

module.exports = {
  ws: null,
  reconnectTimer: null,
  backoff: 1000,
  stopped: true,
  status: { connected: false, detail: '未连接' },

  /** 回调注入：index.js 在 app ready 后调用 */
  onStatus: () => {},
  onInject: () => {},
  onFinal: () => {},

  start() {
    this.stopped = false;
    this.connect();
  },

  stop() {
    this.stopped = true;
    clearTimeout(this.reconnectTimer);
    if (this.ws) {
      try { this.ws.close(); } catch (_) { /* ignore */ }
      this.ws = null;
    }
    this.setStatus(false, '已停止');
  },

  setStatus(connected, detail) {
    this.status = { connected, detail };
    try { this.onStatus(this.status); } catch (_) { /* ignore */ }
  },

  connect() {
    if (this.stopped) return;
    clearTimeout(this.reconnectTimer);
    const { host, port, token } = loadConfig();
    const url = `ws://${host}:${port}/ws/ext?token=${encodeURIComponent(token)}`;
    let ws;
    try {
      ws = new WebSocket(url);
    } catch (err) {
      console.error('[Claw] WS 创建失败:', err.message);
      this.scheduleReconnect();
      return;
    }
    this.ws = ws;
    ws.on('open', () => {
      this.backoff = 1000;
      console.log(`[Claw] 已连接 claw 守护 (${host}:${port})`);
      this.setStatus(true, `已连接 claw 守护 (${host}:${port})`);
      this.send({ type: 'hello' });
    });
    ws.on('message', (data) => {
      let msg;
      try { msg = JSON.parse(String(data)); } catch (_) { return; }
      if (msg.type === 'inject_prompt' && msg.turn_id) {
        this.onInject({ turnId: msg.turn_id, text: msg.text || '' });
      } else if (msg.type === 'final') {
        this.onFinal({ text: msg.text || '' });
      } else if (msg.type === 'error') {
        this.setStatus(false, 'claw: ' + msg.message);
      } else if (msg.type === 'hello_ok') {
        this.setStatus(true, '已连接 claw 守护');
      }
    });
    ws.on('close', () => {
      if (this.ws === ws) this.ws = null;
      this.setStatus(false, '连接断开，重连中…');
      this.scheduleReconnect();
    });
    ws.on('error', (err) => {
      console.warn('[Claw] WS 错误:', err.message);
      try { ws.close(); } catch (_) { /* ignore */ }
    });
  },

  scheduleReconnect() {
    clearTimeout(this.reconnectTimer);
    this.reconnectTimer = setTimeout(() => this.connect(), this.backoff);
    this.backoff = Math.min(this.backoff * 2, 10000);
  },

  send(msg) {
    if (this.ws && this.ws.readyState === WebSocket.OPEN) {
      this.ws.send(JSON.stringify(msg));
      return true;
    }
    return false;
  },

  sendUserRun(text) {
    return this.send({ type: 'user_run', text });
  },

  sendReply(turnId, text) {
    return this.send({ type: 'reply', turn_id: turnId, text });
  },

  sendDiag(payload) {
    return this.send({ type: 'diag', payload });
  },
};