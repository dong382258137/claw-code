// 扩展入口：activate/deactivate + 命令注册 + 生命周期管理
//
// 职责：
// 1. 注册 5 个命令：startServer / stopServer / openChat / cancelPrompt / showStatus
// 2. 启动时根据 claw.autoStart 自动启动
// 3. 监听配置变更，提示重启
// 4. deactivate 时优雅关闭 transport

import * as vscode from 'vscode';
import { AcpTransport } from './acp-transport';
import { ErrorRecovery, createErrorRecovery } from './error-recovery';
import { createHandlers } from './handlers';
import { StatusBarManager } from './status-bar';
import { ChatPanelManager } from './chat-panel';
import {
    API_KEY_SECRET_KEY,
    createSetupWizard,
    runSetupWizard,
} from './setup-wizard';
import type { ClawConfig } from './types';

let outputChannel: vscode.OutputChannel;
let transport: AcpTransport | null = null;
let errorRecovery: ErrorRecovery;
let statusBar: StatusBarManager;
let chatManager: ChatPanelManager | null = null;
/** 扩展 context（保存 SecretStorage 引用，供 startClawServer 读取 API key） */
let extensionContext: vscode.ExtensionContext;

export function activate(context: vscode.ExtensionContext): void {
    extensionContext = context;
    outputChannel = vscode.window.createOutputChannel('Claw Plus');
    errorRecovery = createErrorRecovery(outputChannel, vscode);
    statusBar = new StatusBarManager(vscode);

    context.subscriptions.push(
        vscode.commands.registerCommand('claw.startServer', startClawServer),
        vscode.commands.registerCommand('claw.stopServer', stopClawServer),
        vscode.commands.registerCommand('claw.openChat', openChat),
        vscode.commands.registerCommand('claw.cancelPrompt', cancelActivePrompt),
        vscode.commands.registerCommand('claw.showStatus', showStatusBarMenu),
        // 首次运行配置向导：手动触发 + 首次自动检查
        vscode.commands.registerCommand('claw.setupWizard', () => setupWizard(context, true)),
        statusBar,
        outputChannel,
    );

    // 监听配置变更
    context.subscriptions.push(
        vscode.workspace.onDidChangeConfiguration((e) => {
            if (e.affectsConfiguration('claw')) {
                if (transport) {
                    outputChannel.appendLine(
                        '[info] Configuration changed. Restart server to apply.',
                    );
                }
            }
        }),
    );

    // 首次运行：自动弹向导（已完成过则内部静默跳过）；autoStart 时启动服务
    const config = getConfig();
    if (config.autoStart) {
        void startClawServer();
    } else {
        void setupWizard(context, false);
    }
}

export function deactivate(): void {
    if (transport) {
        void transport.stop();
    }
    chatManager?.dispose();
}

/** 首次运行配置向导：检查 binary + API key，缺什么引导补什么 */
async function setupWizard(context: vscode.ExtensionContext, force: boolean): Promise<void> {
    const deps = createSetupWizard(context, vscode);
    const result = await runSetupWizard(deps, getConfig(), force);
    outputChannel.appendLine(
        `[info] Setup wizard done: binaryReady=${result.binaryReady} apiKeyReady=${result.apiKeyReady}`,
    );
    // 就绪则自动启动
    if (result.binaryReady && result.apiKeyReady && !transport?.isRunning()) {
        await startClawServer();
    }
}

// ===== 命令实现 =====

async function startClawServer(): Promise<void> {
    if (transport?.isRunning()) {
        vscode.window.showInformationMessage('Claw server already running');
        return;
    }

    const config = getConfig();
    statusBar.setStatus('starting');

    // 从 SecretStorage 读取 API key 注入子进程环境（优先于环境变量）
    const apiKey = await contextSecretsApiKey();
    const env: Record<string, string> = {};
    if (apiKey) {
        env.DEEPSEEK_API_KEY = apiKey;
    }

    transport = new AcpTransport({
        binaryPath: config.binaryPath,
        args: ['--model', config.model, '--permission-mode', config.permissionMode],
        cwd: vscode.workspace.workspaceFolders?.[0]?.uri.fsPath,
        env,
    });

    transport.on('stderr', (line) => {
        outputChannel.append(`[stderr] ${line}`);
    });
    transport.on('exit', (info) => {
        outputChannel.appendLine(
            `[info] Claw exited: code=${info.code} signal=${info.signal}`,
        );
        statusBar.setStatus('error');
        if (transport) {
            void errorRecovery.handleProcessExit(transport, info, async () => {
                await initializeSession();
                statusBar.setStatus('running');
                errorRecovery.resetOnStable();
            });
        }
    });
    transport.on('parse-error', ({ line, error }) => {
        outputChannel.appendLine(`[warn] Parse error: ${error} | line: ${line.slice(0, 100)}`);
    });
    transport.on('unknown-message', (msg) => {
        outputChannel.appendLine(`[warn] Unknown message: ${JSON.stringify(msg).slice(0, 200)}`);
    });
    transport.on('notification:session/update', (params) => {
        chatManager?.routeSessionUpdate(params as never);
    });

    // 注册反向请求 handler
    const handlers = createHandlers(vscode);
    transport.onReverseRequest('fs/read_text_file', (p) => handlers.readTextFile(p as never));
    transport.onReverseRequest('fs/write_text_file', (p) => handlers.writeTextFile(p as never));
    transport.onReverseRequest('session/request_permission', (p) =>
        handlers.requestPermission(p as never),
    );

    try {
        await transport.start();
        await initializeSession();
        statusBar.setStatus('running');
        outputChannel.appendLine('[info] Claw server started');
    } catch (err) {
        vscode.window.showErrorMessage(
            `Failed to start Claw: ${(err as Error).message}`,
        );
        statusBar.setStatus('error');
        transport = null;
    }
}

async function initializeSession(): Promise<void> {
    if (!transport) return;
    const result = (await transport.request('initialize', {
        protocolVersion: 1,
        clientCapabilities: {
            fs_read_text_file: true,
            fs_write_text_file: true,
            session_request_permission: true,
        },
    })) as unknown;
    outputChannel.appendLine(`[debug] Initialized: ${JSON.stringify(result)}`);
    // 初始化后创建 ChatPanelManager（lazy 创建，避免未启动时空跑）
    if (!chatManager && transport) {
        chatManager = new ChatPanelManager(vscode, transport);
    }
}

async function stopClawServer(): Promise<void> {
    if (!transport) return;
    chatManager?.dispose();
    chatManager = null;
    await transport.stop();
    transport = null;
    statusBar.setStatus('stopped');
    outputChannel.appendLine('[info] Claw server stopped');
}

async function openChat(): Promise<void> {
    if (!transport?.isRunning()) {
        await startClawServer();
        if (!transport?.isRunning()) return;
    }
    if (!chatManager && transport) {
        chatManager = new ChatPanelManager(vscode, transport);
    }
    await chatManager?.openChat(vscode.workspace.workspaceFolders?.[0]?.uri.fsPath);
}

function cancelActivePrompt(): void {
    chatManager?.cancelAll();
}

async function showStatusBarMenu(): Promise<void> {
    const items: string[] = transport?.isRunning()
        ? ['Stop Server', 'Open Chat', 'Cancel Active Prompt']
        : ['Start Server', 'Open Chat'];
    const choice = await vscode.window.showQuickPick(items, {
        placeHolder: 'Claw Plus actions',
    });
    if (!choice) return;
    if (choice === 'Start Server') await startClawServer();
    else if (choice === 'Stop Server') await stopClawServer();
    else if (choice === 'Open Chat') await openChat();
    else if (choice === 'Cancel Active Prompt') cancelActivePrompt();
}

// ===== 配置读取 =====

function getConfig(): ClawConfig {
    const cfg = vscode.workspace.getConfiguration('claw');
    return {
        binaryPath: cfg.get('binaryPath', 'claw-plus-headless'),
        model: cfg.get('model', 'deepseek-v4-flash'),
        permissionMode: cfg.get('permissionMode', 'workspace-write'),
        autoStart: cfg.get('autoStart', false),
        logLevel: cfg.get('logLevel', 'info'),
    };
}

/** 从 SecretStorage 读取 API key（无则 undefined） */
async function contextSecretsApiKey(): Promise<string | undefined> {
    try {
        return await extensionContext.secrets.get(API_KEY_SECRET_KEY);
    } catch {
        return undefined;
    }
}
