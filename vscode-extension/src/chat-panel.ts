// Webview 对话面板：每个 panel 对应一个 ACP session。
//
// 职责：
// 1. 创建 Webview，注入基础 HTML
// 2. 接收 webview 的 prompt 消息，转发给 AcpTransport
// 3. 接收 AcpTransport 的 session/update 通知，路由给对应 panel
// 4. 管理多 session 生命周期（panel 关闭时通知 agent）

import type * as vscode from 'vscode';
import type { AcpTransport } from './acp-transport';
import type { SessionUpdateParams, SessionUpdate } from './types';

interface PanelSession {
    panel: vscode.WebviewPanel;
    sessionId: string;
    disposable: vscode.Disposable;
}

export class ChatPanelManager {
    private panels = new Map<string, PanelSession>();
    private nextPanelId = 1;

    constructor(
        private vscodeApi: typeof vscode,
        private transport: AcpTransport,
        /** 流式/忙碌状态回调（供状态栏更新） */
        private onBusyChange?: (busy: boolean) => void,
    ) {}

    /** 打开新的对话面板，自动创建 session */
    async openChat(cwd?: string): Promise<void> {
        const panelId = `panel-${this.nextPanelId++}`;
        const panel = this.vscodeApi.window.createWebviewPanel(
            'clawChat',
            'Claw Plus',
            this.vscodeApi.ViewColumn.Beside,
            {
                enableScripts: true,
                retainContextWhenHidden: true,
                localResourceRoots: [],
            },
        );
        panel.iconPath = new this.vscodeApi.ThemeIcon('comment-discussion');
        panel.webview.html = this.getChatHtml();

        // 创建 ACP session
        // mcpServers 是 NewSessionRequest 的必填字段（schema 无 default），
        // 缺省时 agent 返回 -32602 Invalid params。
        const result = (await this.transport.request('session/new', {
            cwd,
            mcpServers: [],
        })) as {
            sessionId: string;
        };
        const sessionId = result.sessionId;

        const disposable = panel.webview.onDidReceiveMessage((msg) => {
            void this.handleWebviewMessage(panelId, sessionId, msg);
        });

        panel.onDidDispose(() => {
            this.panels.delete(panelId);
            disposable.dispose();
            // 通知 agent 取消该 session（若协议支持 session/close，此处用 cancel 替代）
            try {
                this.transport.notify('session/cancel', { sessionId });
            } catch {
                // 忽略 transport 已停止的情况
            }
        });

        this.panels.set(panelId, { panel, sessionId, disposable });
    }

    /** 取消所有活跃 session 的当前 prompt */
    cancelAll(): void {
        for (const { sessionId } of this.panels.values()) {
            try {
                this.transport.notify('session/cancel', { sessionId });
            } catch {
                // 忽略
            }
        }
    }

    /** 路由 session/update 通知给对应 panel */
    routeSessionUpdate(params: SessionUpdateParams): void {
        for (const session of this.panels.values()) {
            if (session.sessionId === params.sessionId) {
                session.panel.webview.postMessage({
                    type: 'update',
                    update: params.update,
                });
                return;
            }
        }
        // 没有匹配的 panel（可能已被用户关闭），忽略
    }

    /** 关闭所有 panel */
    dispose(): void {
        for (const { panel, disposable } of this.panels.values()) {
            disposable.dispose();
            panel.dispose();
        }
        this.panels.clear();
    }

    private async handleWebviewMessage(
        _panelId: string,
        sessionId: string,
        msg: unknown,
    ): Promise<void> {
        const data = msg as { type: string; text?: string };
        if (data.type === 'prompt' && data.text) {
            try {
                this.onBusyChange?.(true);
                await this.transport.request('session/prompt', {
                    sessionId,
                    prompt: [{ type: 'text', text: data.text }],
                });
                // turn 结束：通知 webview 折叠 thinking、闭合流式文本块
                this.postToPanel(sessionId, { type: 'turn_end' });
            } catch (err) {
                // 错误通过 session/update 不一定能传达，直接 postMessage 给 webview
                this.postToPanel(sessionId, {
                    type: 'error',
                    message: `Prompt failed: ${(err as Error).message}`,
                });
            } finally {
                this.onBusyChange?.(false);
            }
        } else if (data.type === 'cancel') {
            this.transport.notify('session/cancel', { sessionId });
        }
    }

    /** 向指定 session 的 panel 发送 webview 消息 */
    private postToPanel(sessionId: string, message: unknown): void {
        for (const { panel, sessionId: sid } of this.panels.values()) {
            if (sid === sessionId) {
                panel.webview.postMessage(message);
                return;
            }
        }
    }

    private getChatHtml(): string {
        return `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>Claw Chat</title>
<style>
  :root {
    --fg: var(--vscode-foreground, #333);
    --muted: var(--vscode-descriptionForeground, #888);
    --border: var(--vscode-input-border, #ccc);
    --accent: var(--vscode-textLink-foreground, #007acc);
    --error: var(--vscode-errorForeground, #d73a49);
    --bg: var(--vscode-editor-background, #fff);
    --panel-bg: var(--vscode-sideBar-background, #f5f5f5);
  }
  * { box-sizing: border-box; }
  body {
    font-family: var(--vscode-font-family, sans-serif);
    font-size: var(--vscode-font-size, 13px);
    color: var(--fg);
    background: var(--bg);
    display: flex; flex-direction: column; height: 100vh;
    margin: 0; padding: 8px;
  }
  #messages {
    flex: 1; overflow-y: auto;
    border: 1px solid var(--border);
    padding: 8px; margin-bottom: 8px; border-radius: 4px;
    background: var(--panel-bg);
  }
  .entry { margin: 6px 0; line-height: 1.5; }
  .entry-user { color: var(--accent); }
  .entry-user .who { font-weight: 600; }
  .entry-agent .md { color: var(--fg); }
  .entry-error { color: var(--error); }
  .entry-tool { border: 1px solid var(--border); border-radius: 4px; padding: 4px 8px; }
  .entry-tool .head { cursor: pointer; display: flex; align-items: center; gap: 6px; }
  .entry-tool .head .status { font-weight: 600; }
  .entry-tool .body { margin-top: 4px; padding: 6px; background: var(--bg); border-radius: 3px; font-family: var(--vscode-editor-font-family, monospace); font-size: 12px; white-space: pre-wrap; }
  .entry-tool.collapsed .body { display: none; }
  .entry-thinking { border-left: 3px solid var(--muted); padding-left: 8px; color: var(--muted); }
  .entry-thinking .head { cursor: pointer; }
  .entry-thinking .body { margin-top: 4px; white-space: pre-wrap; }
  .entry-thinking.collapsed .body { display: none; }
  /* markdown 基础样式 */
  .md pre { background: var(--bg); border: 1px solid var(--border); border-radius: 3px; padding: 6px; overflow-x: auto; }
  .md code { font-family: var(--vscode-editor-font-family, monospace); font-size: 12px; }
  .md pre code { background: none; padding: 0; }
  .md p { margin: 4px 0; }
  .md table { border-collapse: collapse; margin: 4px 0; }
  .md th, .md td { border: 1px solid var(--border); padding: 3px 6px; }
  .md h1, .md h2, .md h3, .md h4 { margin: 8px 0 4px; }
  .md ul { margin: 4px 0; padding-left: 20px; }
  .md li { margin: 2px 0; }
  .md blockquote { border-left: 3px solid var(--muted); margin: 4px 0; padding-left: 8px; color: var(--muted); }
  #input-row { display: flex; gap: 8px; }
  #input {
    flex: 1;
    background: var(--vscode-input-background, #fff);
    color: var(--vscode-input-foreground, #333);
    border: 1px solid var(--border); padding: 6px; border-radius: 4px;
    font-family: inherit; font-size: inherit;
    resize: vertical; min-height: 60px; max-height: 200px;
  }
  button {
    background: var(--vscode-button-background, #007acc);
    color: var(--vscode-button-foreground, #fff);
    border: none; padding: 6px 12px; border-radius: 4px; cursor: pointer;
  }
  button.secondary { background: var(--vscode-button-secondaryBackground, #5f6b7c); color: var(--vscode-button-secondaryForeground, #fff); }
  button:hover { filter: brightness(1.1); }
  button:disabled { opacity: 0.5; cursor: not-allowed; }
</style>
</head>
<body>
<div id="messages"></div>
<div id="input-row">
  <textarea id="input" placeholder="Send a prompt to Claw... (Shift+Enter for newline)"></textarea>
  <button id="send" onclick="sendPrompt()">Send</button>
  <button class="secondary" onclick="cancelPrompt()">Cancel</button>
</div>
<script>
  const vscode = acquireVsCodeApi();
  const messages = document.getElementById('messages');
  const input = document.getElementById('input');

  // ===== 流式状态 =====
  let currentTextEl = null;   // 当前 AI 文本条目（markdown）
  let currentTextBuf = '';
  let currentThinkingEl = null; // 当前 thinking 条目
  let currentThinkingBuf = '';
  const toolCards = new Map(); // id -> { el, statusEl, bodyEl, raw }

  input.addEventListener('keydown', (e) => {
    if (e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); sendPrompt(); }
  });

  function sendPrompt() {
    const text = input.value.trim();
    if (!text) return;
    appendUser(text);
    vscode.postMessage({ type: 'prompt', text });
    input.value = '';
  }
  function cancelPrompt() { vscode.postMessage({ type: 'cancel' }); }

  function scrollToBottom() { messages.scrollTop = messages.scrollHeight; }

  function appendUser(text) {
    const div = document.createElement('div');
    div.className = 'entry entry-user';
    const who = document.createElement('span'); who.className = 'who'; who.textContent = '> ';
    div.appendChild(who);
    div.appendChild(document.createTextNode(text));
    messages.appendChild(div);
    scrollToBottom();
  }

  // ===== markdown 渲染（轻量）=====
  function escapeHtml(s) {
    return s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
  }
  function inline(s) {
    let out = escapeHtml(s);
    // 行内代码（先处理，避免与加粗冲突）
    out = out.replace(/\`([^\`]+)\`/g, '<code>$1</code>');
    // 加粗 / 斜体
    out = out.replace(/\*\*([^*]+)\*\*/g, '<strong>$1</strong>');
    out = out.replace(/\*([^*]+)\*/g, '<em>$1</em>');
    // 链接
    out = out.replace(/\[([^\]]+)\]\(([^)]+)\)/g, '<a href="$2">$1</a>');
    return out;
  }
  function renderTable(lines) {
    const parse = (l) => l.trim().replace(/^\|/, '').replace(/\|$/, '').split('|').map((c) => c.trim());
    let html = '<table>';
    lines.forEach((l, i) => {
      if (i === 1 && parse(l).every((c) => /^:?-+:?$/.test(c))) return; // 分隔行
      const tag = i === 0 ? 'th' : 'td';
      html += '<tr>' + parse(l).map((c) => '<' + tag + '>' + inline(c) + '</' + tag + '>').join('') + '</tr>';
    });
    return html + '</table>';
  }
  function renderMarkdown(src) {
    if (!src) return '';
    const lines = src.split('\n');
    let html = '';
    let inCode = false, codeBuf = [], inTable = false, tableBuf = [];
    for (const line of lines) {
      const codeMatch = line.match(/^\`\`\`/);
      if (codeMatch) {
        if (inCode) { html += '<pre><code>' + escapeHtml(codeBuf.join('\n')) + '</code></pre>'; inCode = false; codeBuf = []; }
        else { inCode = true; codeBuf = []; }
        continue;
      }
      if (inCode) { codeBuf.push(line); continue; }
      const isTableRow = line.trim().startsWith('|') && line.trim().endsWith('|');
      if (isTableRow) { if (!inTable) { inTable = true; tableBuf = []; } tableBuf.push(line); continue; }
      else if (inTable) { html += renderTable(tableBuf); inTable = false; tableBuf = []; }
      const h = line.match(/^(#{1,6})\\s+(.*)/);
      if (h) { html += '<h' + h[1].length + '>' + inline(h[2]) + '</h' + h[1].length + '>'; continue; }
      const li = line.match(/^\\s*[-*]\\s+(.*)/);
      if (li) { html += '<li>' + inline(li[1]) + '</li>'; continue; }
      const quote = line.match(/^>\\s+(.*)/);
      if (quote) { html += '<blockquote>' + inline(quote[1]) + '</blockquote>'; continue; }
      if (line.trim() === '') { html += '<p></p>'; continue; }
      html += '<p>' + inline(line) + '</p>';
    }
    if (inCode) html += '<pre><code>' + escapeHtml(codeBuf.join('\n')) + '</code></pre>';
    if (inTable) html += renderTable(tableBuf);
    return html;
  }

  // ===== AI 文本（流式累积）=====
  function ensureTextEntry() {
    if (currentTextEl) return currentTextEl;
    const div = document.createElement('div');
    div.className = 'entry entry-agent';
    const md = document.createElement('div'); md.className = 'md';
    div.appendChild(md);
    messages.appendChild(div);
    currentTextEl = md;
    currentTextBuf = '';
    return md;
  }
  function appendAgentText(delta) {
    const el = ensureTextEntry();
    currentTextBuf += delta;
    el.innerHTML = renderMarkdown(currentTextBuf);
    scrollToBottom();
  }
  function closeTextEntry() {
    if (currentTextEl) {
      currentTextEl.innerHTML = renderMarkdown(currentTextBuf);
      currentTextEl = null; currentTextBuf = '';
    }
  }

  // ===== Thinking（折叠卡片）=====
  function ensureThinkingEntry() {
    if (currentThinkingEl) return currentThinkingEl;
    const div = document.createElement('div');
    div.className = 'entry entry-thinking';
    const head = document.createElement('div'); head.className = 'head';
    head.textContent = '\u25B6 Thinking';
    head.addEventListener('click', () => div.classList.toggle('collapsed'));
    const body = document.createElement('div'); body.className = 'body';
    div.appendChild(head); div.appendChild(body);
    messages.appendChild(div);
    currentThinkingEl = { div, head, body };
    return currentThinkingEl;
  }
  function appendThinking(delta) {
    const t = ensureThinkingEntry();
    t.div.classList.remove('collapsed');
    t.body.textContent += delta;
    t.head.textContent = '\u25BC Thinking (' + t.body.textContent.length + ' chars)';
    scrollToBottom();
  }
  function closeThinkingEntry() {
    if (currentThinkingEl) {
      const t = currentThinkingEl;
      t.div.classList.add('collapsed');
      t.head.textContent = '\u25B6 Thinking (' + t.body.textContent.length + ' chars hidden)';
      currentThinkingEl = null; currentThinkingBuf = '';
    }
  }

  // ===== ToolCard =====
  function upsertToolCard(id, name, status) {
    let card = toolCards.get(id);
    if (!card) {
      const div = document.createElement('div');
      div.className = 'entry entry-tool collapsed';
      const head = document.createElement('div'); head.className = 'head';
      const statusEl = document.createElement('span'); statusEl.className = 'status';
      const nameEl = document.createElement('span'); nameEl.textContent = name;
      head.appendChild(statusEl); head.appendChild(nameEl);
      head.addEventListener('click', () => div.classList.toggle('collapsed'));
      const body = document.createElement('div'); body.className = 'body';
      div.appendChild(head); div.appendChild(body);
      messages.appendChild(div);
      card = { div, statusEl, bodyEl: body };
      toolCards.set(id, card);
    }
    setToolStatus(card, status);
    scrollToBottom();
  }
  function setToolStatus(card, status) {
    const s = status === 'completed' ? '\u2713' : status === 'failed' ? '\u2717' : '\u23F3';
    card.statusEl.textContent = s;
  }
  function updateToolCard(id, status) {
    const card = toolCards.get(id);
    if (card) { setToolStatus(card, status); scrollToBottom(); }
  }

  function appendError(msg) {
    closeTextEntry(); closeThinkingEntry();
    const div = document.createElement('div');
    div.className = 'entry entry-error';
    div.textContent = '[error] ' + msg;
    messages.appendChild(div);
    scrollToBottom();
  }
  function appendRaw(text) {
    closeTextEntry(); closeThinkingEntry();
    const div = document.createElement('div');
    div.className = 'entry entry-agent';
    div.textContent = text;
    messages.appendChild(div);
    scrollToBottom();
  }

  function finishTurn() {
    closeTextEntry(); closeThinkingEntry();
  }

  // ===== 后端 update 分发 =====
  function handleUpdate(update) {
    const kind = update.sessionUpdate;
    if (kind === 'agent_message_chunk') {
      const text = update.content && update.content.text;
      if (!text) return;
      if (text.startsWith('[thinking] ')) { closeTextEntry(); appendThinking(text.slice(11)); }
      else { closeThinkingEntry(); appendAgentText(text); }
    } else if (kind === 'tool_call') {
      const id = update.id || update.toolCallId || update.tool_call_id || 'tool';
      const name = update.toolName || update.title || update.name || 'tool';
      const status = update.status || 'pending';
      upsertToolCard(id, name, status);
    } else if (kind === 'tool_call_update') {
      const id = update.id || update.toolCallId || update.tool_call_id || 'tool';
      const status = update.status || 'completed';
      updateToolCard(id, status);
    } else {
      appendRaw(JSON.stringify(update));
    }
  }

  window.addEventListener('message', (e) => {
    const msg = e.data;
    if (msg.type === 'update') { handleUpdate(msg.update); }
    else if (msg.type === 'error') { appendError(msg.message); }
    else if (msg.type === 'turn_end') { finishTurn(); }
  });
</script>
</body>
</html>`;
    }
}

/** 类型辅助：让 TS 知道 update 字段使用 */
export type _SessionUpdate = SessionUpdate;
