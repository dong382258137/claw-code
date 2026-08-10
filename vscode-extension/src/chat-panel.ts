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
                await this.transport.request('session/prompt', {
                    sessionId,
                    prompt: [{ type: 'text', text: data.text }],
                });
            } catch (err) {
                // 错误通过 session/update 不一定能传达，直接 postMessage 给 webview
                for (const { panel, sessionId: sid } of this.panels.values()) {
                    if (sid === sessionId) {
                        panel.webview.postMessage({
                            type: 'error',
                            message: `Prompt failed: ${(err as Error).message}`,
                        });
                    }
                }
            }
        } else if (data.type === 'cancel') {
            this.transport.notify('session/cancel', { sessionId });
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
  body {
    font-family: var(--vscode-font-family, sans-serif);
    color: var(--vscode-foreground, #333);
    background: var(--vscode-editor-background, #fff);
    display: flex;
    flex-direction: column;
    height: 100vh;
    margin: 0;
    padding: 8px;
    box-sizing: border-box;
  }
  #messages {
    flex: 1;
    overflow-y: auto;
    border: 1px solid var(--vscode-input-border, #ccc);
    padding: 8px;
    margin-bottom: 8px;
    border-radius: 4px;
    background: var(--vscode-sideBar-background, transparent);
  }
  .msg-user { color: var(--vscode-textLink-foreground, #007acc); margin: 4px 0; }
  .msg-agent { color: var(--vscode-foreground); margin: 4px 0; }
  .msg-tool { color: var(--vscode-descriptionForeground, #888); font-style: italic; margin: 4px 0; }
  .msg-error { color: var(--vscode-errorForeground, #d73a49); margin: 4px 0; }
  #input-row { display: flex; gap: 8px; }
  #input {
    flex: 1;
    background: var(--vscode-input-background, #fff);
    color: var(--vscode-input-foreground, #333);
    border: 1px solid var(--vscode-input-border, #ccc);
    padding: 6px;
    border-radius: 4px;
    font-family: inherit;
    font-size: var(--vscode-font-size, 13px);
    resize: vertical;
    min-height: 60px;
    max-height: 200px;
  }
  button {
    background: var(--vscode-button-background, #007acc);
    color: var(--vscode-button-foreground, #fff);
    border: none;
    padding: 6px 12px;
    border-radius: 4px;
    cursor: pointer;
    font-size: var(--vscode-font-size, 13px);
  }
  button.secondary {
    background: var(--vscode-button-secondaryBackground, #5f6b7c);
    color: var(--vscode-button-secondaryForeground, #fff);
  }
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
  const sendBtn = document.getElementById('send');

  // Enter 发送，Shift+Enter 换行
  input.addEventListener('keydown', (e) => {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      sendPrompt();
    }
  });

  function sendPrompt() {
    const text = input.value.trim();
    if (!text) return;
    appendMsg('user', '> ' + text);
    vscode.postMessage({ type: 'prompt', text });
    input.value = '';
  }

  function cancelPrompt() {
    vscode.postMessage({ type: 'cancel' });
  }

  function appendMsg(cls, text) {
    const div = document.createElement('div');
    div.className = 'msg-' + cls;
    div.textContent = text;
    messages.appendChild(div);
    messages.scrollTop = messages.scrollHeight;
  }

  window.addEventListener('message', (e) => {
    const msg = e.data;
    if (msg.type === 'update') {
      handleUpdate(msg.update);
    } else if (msg.type === 'error') {
      appendMsg('error', '[error] ' + msg.message);
    }
  });

  function handleUpdate(update) {
    // ACP 0.10.4:SessionUpdate 内部 tag 为 sessionUpdate, variant 为 snake_case
    if (update.sessionUpdate === 'agent_message_chunk' && update.content && update.content.text) {
      appendMsg('agent', update.content.text);
    } else if (update.sessionUpdate === 'tool_call') {
      const status = update.status || 'pending';
      appendMsg('tool', '[tool] ' + (update.toolName || 'unknown') + ' (' + status + ')');
    } else if (update.sessionUpdate === 'tool_call_update') {
      appendMsg('tool', '[tool] ' + (update.id || 'unknown') + ' -> ' + update.status);
    } else {
      // 未识别的 update 类型，原样显示
      appendMsg('agent', JSON.stringify(update));
    }
  }
</script>
</body>
</html>`;
    }
}

/** 类型辅助：让 TS 知道 update 字段使用 */
export type _SessionUpdate = SessionUpdate;
