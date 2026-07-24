// 子进程崩溃后的自动重启策略
//
// 策略：
// - 正常退出（code=0）：不重启
// - 非正常退出：最多重启 MAX_RESTART_ATTEMPTS 次，每次间隔 RESTART_INTERVAL_MS
// - 重启后稳定运行 STABLE_RESET_MS（5min），尝试次数重置为 0
// - 超过最大尝试次数：提示用户手动检查日志

import type * as vscode from 'vscode';
import type { AcpTransport, ProcessExitInfo } from './acp-transport';

const MAX_RESTART_ATTEMPTS = 3;
const RESTART_INTERVAL_MS = 5000;
const STABLE_RESET_MS = 5 * 60 * 1000; // 5 分钟稳定运行后重置计数器

export interface ErrorRecoveryDeps {
    showWarningMessage(message: string): Thenable<string | undefined>;
    showErrorMessage(message: string, ...items: string[]): Thenable<string | undefined>;
    showOutputChannel(): void;
    setTimeout(callback: () => void, ms: number): NodeJS.Timeout;
}

/** 重启回调，由调用方实现（重新 start + initialize） */
export type OnRestarted = () => Promise<void>;

export class ErrorRecovery {
    private restartAttempts = 0;
    private lastRestartTime = 0;
    private stableTimer?: NodeJS.Timeout;
    private deps: ErrorRecoveryDeps;

    constructor(deps: ErrorRecoveryDeps) {
        this.deps = deps;
    }

    /**
     * 处理子进程退出事件。
     *
     * @param transport 已停止的 transport（调用方负责重新 start）
     * @param exitInfo 退出信息
     * @param onRestarted 重启成功后回调（重新 initialize 等）
     */
    async handleProcessExit(
        _transport: AcpTransport,
        exitInfo: ProcessExitInfo,
        onRestarted: OnRestarted,
    ): Promise<void> {
        // 正常退出：不重启
        if (exitInfo.code === 0) return;

        // 限流：5s 内不重复重启
        const now = Date.now();
        if (now - this.lastRestartTime < RESTART_INTERVAL_MS) {
            await this.deps.showErrorMessage('Claw 重启过于频繁，放弃');
            return;
        }

        // 超过最大尝试次数
        if (this.restartAttempts >= MAX_RESTART_ATTEMPTS) {
            const choice = await this.deps.showErrorMessage(
                `Claw 已崩溃 ${MAX_RESTART_ATTEMPTS} 次，请检查日志后手动重启`,
                'Show Logs',
                'Restart',
            );
            if (choice === 'Show Logs') {
                this.deps.showOutputChannel();
            } else if (choice === 'Restart') {
                this.restartAttempts = 0;
                await this.attemptRestart(_transport, onRestarted);
            }
            return;
        }

        await this.attemptRestart(_transport, onRestarted);
    }

    /** 成功运行 5 分钟后重置尝试次数 */
    resetOnStable(): void {
        if (this.stableTimer) clearTimeout(this.stableTimer);
        this.stableTimer = this.deps.setTimeout(() => {
            this.restartAttempts = 0;
        }, STABLE_RESET_MS);
    }

    /** 获取当前尝试次数（测试用） */
    getAttempts(): number {
        return this.restartAttempts;
    }

    private async attemptRestart(
        transport: AcpTransport,
        onRestarted: OnRestarted,
    ): Promise<void> {
        this.restartAttempts++;
        this.lastRestartTime = Date.now();
        await this.deps.showWarningMessage(
            `Claw server crashed, restarting (${this.restartAttempts}/${MAX_RESTART_ATTEMPTS})...`,
        );

        // 等待间隔，避免立即崩溃再次触发
        await new Promise((r) => this.deps.setTimeout(() => r(undefined), RESTART_INTERVAL_MS));
        await transport.start();
        await onRestarted();
        this.resetOnStable();
    }
}

/** 创建使用真实 vscode API 的 ErrorRecovery */
export function createErrorRecovery(
    outputChannel: vscode.OutputChannel,
    vscodeApi: typeof vscode,
): ErrorRecovery {
    return new ErrorRecovery({
        showWarningMessage: (msg) => vscodeApi.window.showWarningMessage(msg),
        showErrorMessage: (msg, ...items) => vscodeApi.window.showErrorMessage(msg, ...items),
        showOutputChannel: () => outputChannel.show(),
        setTimeout: (cb, ms) => setTimeout(cb, ms),
    });
}
