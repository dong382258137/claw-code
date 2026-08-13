// 状态栏管理：单一来源更新状态，避免多处直接操作 statusBarItem。
//
// 显示：模型 · #turn · ⏳流式 · cwd（对齐 TUI status_bar 的信息密度）。
// token/cost 的实时统计待后端 Usage 推送链路补全后接入（见 P1）。

import type * as vscode from 'vscode';

export type ClawStatus =
    | 'stopped' // 未启动
    | 'starting' // 正在 spawn + initialize
    | 'running' // 已启动
    | 'error' // 启动失败或崩溃
    | 'busy'; // 正在处理 prompt

export class StatusBarManager {
    private item: vscode.StatusBarItem;
    private status: ClawStatus = 'stopped';
    private model = '';
    private cwd = '';
    private streaming = false;
    private turnCount = 0;

    constructor(vscodeApi: typeof vscode) {
        this.item = vscodeApi.window.createStatusBarItem(
            vscodeApi.StatusBarAlignment.Right,
            100,
        );
        this.item.command = 'claw.showStatus';
        this.render();
        this.item.show();
    }

    setStatus(status: ClawStatus): void {
        this.status = status;
        if (status === 'running') this.streaming = false;
        this.render();
    }

    setModel(model: string): void {
        this.model = model;
        this.render();
    }

    setCwd(cwd: string): void {
        this.cwd = cwd;
        this.render();
    }

    /** 流式状态：收到 agent 文本 chunk 时 true，turn 结束 false */
    setStreaming(streaming: boolean): void {
        this.streaming = streaming;
        if (streaming) this.status = 'busy';
        else if (this.status === 'busy') this.status = 'running';
        this.render();
    }

    /** turn 计数（后端 Usage 推送后接入，当前预留） */
    setTurnCount(n: number): void {
        this.turnCount = n;
        this.render();
    }

    dispose(): void {
        this.item.dispose();
    }

    private render(): void {
        const parts: string[] = [];
        switch (this.status) {
            case 'stopped':
                parts.push('$(comment-discussion) Claw');
                break;
            case 'starting':
                parts.push('$(loading~spin) Claw');
                break;
            case 'running':
                parts.push('$(check) Claw');
                break;
            case 'error':
                parts.push('$(error) Claw');
                break;
            case 'busy':
                parts.push('$(sync~spin) Claw');
                break;
        }
        if (this.model) parts.push(this.shortenModel(this.model));
        if (this.turnCount > 0) parts.push(`#${this.turnCount}`);
        if (this.streaming) parts.push('$(pulse) streaming');
        if (this.cwd) parts.push(this.shortenCwd(this.cwd));
        this.item.text = parts.join(' ');
        this.item.tooltip = `Claw Plus: ${this.status}`;
    }

    private shortenModel(model: string): string {
        const m = model.toLowerCase();
        if (m.includes('opus')) return 'opus';
        if (m.includes('sonnet')) return 'sonnet';
        if (m.includes('haiku')) return 'haiku';
        if (m.includes('gpt-5') || m.includes('gpt5')) return 'gpt-5';
        if (m.includes('deepseek')) return model.replace('deepseek-', 'ds-');
        if (m.includes('qwen')) return 'qwen';
        if (m.includes('grok')) return 'grok';
        return model.length > 16 ? model.slice(0, 16) : model;
    }

    private shortenCwd(cwd: string): string {
        const parts = cwd.replace(/\\/g, '/').split('/').filter(Boolean);
        if (parts.length <= 2) return cwd;
        return `…/${parts.slice(-2).join('/')}`;
    }
}
