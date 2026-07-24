// 状态栏管理：单一来源更新状态，避免多处直接操作 statusBarItem。

import type * as vscode from 'vscode';

export type ClawStatus =
    | 'stopped' // 未启动
    | 'starting' // 正在 spawn + initialize
    | 'running' // 已启动
    | 'error' // 启动失败或崩溃
    | 'busy'; // 正在处理 prompt

const STATUS_TEXT: Record<ClawStatus, string> = {
    stopped: '$(comment-discussion) Claw',
    starting: '$(loading~spin) Claw (starting)',
    running: '$(check) Claw',
    error: '$(error) Claw (errored)',
    busy: '$(sync~spin) Claw (busy)',
};

const STATUS_TOOLTIP: Record<ClawStatus, string> = {
    stopped: 'Claw Plus: stopped. Click to start.',
    starting: 'Claw Plus: starting...',
    running: 'Claw Plus: running. Click to manage.',
    error: 'Claw Plus: errored. Check logs.',
    busy: 'Claw Plus: processing prompt...',
};

export class StatusBarManager {
    private item: vscode.StatusBarItem;

    constructor(vscodeApi: typeof vscode) {
        this.item = vscodeApi.window.createStatusBarItem(
            vscodeApi.StatusBarAlignment.Right,
            100,
        );
        this.item.command = 'claw.showStatus';
        this.setStatus('stopped');
        this.item.show();
    }

    setStatus(status: ClawStatus): void {
        this.item.text = STATUS_TEXT[status];
        this.item.tooltip = STATUS_TOOLTIP[status];
        this.item.backgroundColor = undefined;
    }

    dispose(): void {
        this.item.dispose();
    }
}
