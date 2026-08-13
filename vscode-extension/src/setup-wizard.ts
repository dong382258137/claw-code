// 首次运行配置向导（onboarding）
//
// 目标：新用户零门槛上手。首次打开扩展时自动检查：
//   1. claw binary 是否可用
//   2. API key 是否已配置（SecretStorage，非 settings.json 明文）
// 缺什么补什么，全部通过交互式向导完成，避免用户手写配置文件。
//
// 设计：注入 vscode 依赖（与 error-recovery.ts 同模式），核心逻辑可单测。

import type * as vscode from 'vscode';
import { spawn } from 'child_process';
import type { ClawConfig } from './types';

/** SecretStorage key（存 API key，不进 settings.json） */
export const API_KEY_SECRET_KEY = 'claw.apiKey';
/** 首次运行标记（globalState），避免每次启动都弹向导 */
export const WIZARD_DONE_STATE_KEY = 'claw.wizardDone';

/** 向导依赖（可注入 mock 测试） */
export interface SetupWizardDeps {
    /** 检测 binary 是否可执行（spawn --help 冒烟） */
    checkBinary(binaryPath: string): Promise<boolean>;
    /** 用文件选择器定位 binary，返回绝对路径（取消返回 undefined） */
    pickBinary(): Thenable<string | undefined>;
    /** 将用户选择的 binary 路径写入配置（claw.binaryPath） */
    saveBinaryPath(path: string): Thenable<void>;
    /** 读取已存 API key（SecretStorage） */
    getApiKey(): Thenable<string | undefined>;
    /** 保存 API key（SecretStorage） */
    setApiKey(key: string): Thenable<void>;
    /** 用户输入（InputBox），返回 undefined 表示取消 */
    promptApiKey(prompt: string): Thenable<string | undefined>;
    /** 展示错误并提供动作按钮，返回选中动作 */
    showError(message: string, ...items: string[]): Thenable<string | undefined>;
    /** 展示信息并提供动作按钮 */
    showInfo(message: string, ...items: string[]): Thenable<string | undefined>;
    /** 打开系统浏览器 */
    openExternal(url: string): Thenable<void>;
    /** 执行 install.ps1（在系统终端里跑） */
    runInstaller(): Thenable<void>;
    /** 向导是否已完成过（globalState） */
    isDone(): Thenable<boolean>;
    /** 标记向导完成 */
    markDone(): Thenable<void>;
}

/** 向导决策结果（便于单测断言） */
export interface WizardResult {
    /** binary 是否已就绪 */
    binaryReady: boolean;
    /** API key 是否已就绪 */
    apiKeyReady: boolean;
    /** 用户是否取消了（放弃配置） */
    cancelled: boolean;
}

/**
 * 探测 binary 是否可执行。
 *
 * 对绝对路径 / PATH 中的命令统一用 spawn --help 冒烟：
 * - 命令不存在 -> 'error' 事件 -> false
 * - 命令存在但无法启动 -> false
 * - 正常退出 -> true
 *
 * 注意：不用 --version——claw-plus-headless 不支持该参数（会 exit(1)），
 * 会导致探测误判 binary 不可用；--help 是 headless 明确支持的参数（exit 0）。
 * Windows 下 spawn 一个不存在的命令会同步抛 ENOENT 或异步 error 事件，两者都覆盖。
 */
export async function checkBinaryAvailable(binaryPath: string): Promise<boolean> {
    if (!binaryPath) return false;
    return new Promise<boolean>((resolve) => {
        let settled = false;
        const finish = (ok: boolean): void => {
            if (!settled) {
                settled = true;
                resolve(ok);
            }
        };
        try {
            const child = spawn(binaryPath, ['--help'], { stdio: 'ignore' });
            child.on('error', () => finish(false));
            child.on('exit', (code) => finish(code === 0));
        } catch {
            finish(false);
        }
    });
}

/** 组装真实 vscode 依赖 */
export function createSetupWizard(
    context: vscode.ExtensionContext,
    vscodeApi: typeof vscode,
): SetupWizardDeps {
    return {
        checkBinary: checkBinaryAvailable,
        pickBinary: async () => {
            const picked = await vscodeApi.window.showOpenDialog({
                canSelectFiles: true,
                canSelectFolders: false,
                canSelectMany: false,
                openLabel: '选择 claw-plus-headless',
                filters: {
                    'Executable': ['exe'],
                    'All files': ['*'],
                },
            });
            return picked?.[0]?.fsPath;
        },
        saveBinaryPath: async (path) => {
            await vscodeApi.workspace
                .getConfiguration('claw')
                .update('binaryPath', path, vscodeApi.ConfigurationTarget.Global);
        },
        getApiKey: () => context.secrets.get(API_KEY_SECRET_KEY),
        setApiKey: (key) => context.secrets.store(API_KEY_SECRET_KEY, key),
        promptApiKey: (prompt) =>
            vscodeApi.window.showInputBox({
                prompt,
                password: true,
                ignoreFocusOut: true,
                placeHolder: 'sk-...',
            }),
        showError: (msg, ...items) => vscodeApi.window.showErrorMessage(msg, ...items),
        showInfo: (msg, ...items) => vscodeApi.window.showInformationMessage(msg, ...items),
        openExternal: async (url) => {
            await vscodeApi.env.openExternal(vscodeApi.Uri.parse(url));
        },
        runInstaller: () => runInstallerInTerminal(vscodeApi),
        isDone: async () => !!(await context.globalState.get(WIZARD_DONE_STATE_KEY, false)),
        markDone: () => context.globalState.update(WIZARD_DONE_STATE_KEY, true),
    };
}

/** 在 VS Code 集成终端中引导安装 claw（用户可看到进度/报错） */
async function runInstallerInTerminal(vscodeApi: typeof vscode): Promise<void> {
    const terminal = vscodeApi.window.createTerminal({ name: 'Claw Installer' });
    terminal.show();
    // claw 仓库可能不在当前工作区，无法可靠定位 install.ps1，
    // 所以打开终端并给出明确的安装指引，由用户按其环境执行。
    terminal.sendText(
        'echo "== Claw Plus install guide =="; ' +
            'echo "1) Clone/download the claw repo:"; ' +
            'echo "   git clone https://github.com/claw-code/claw-code.git && cd claw-code"; ' +
            'echo "2) Run the Windows installer:"; ' +
            'echo "   PowerShell -ExecutionPolicy Bypass -File .\\install.ps1 -Release"; ' +
            'echo "3) Reopen this window, then run the Claw setup wizard again."',
    );
}

/**
 * 运行首次配置向导。
 *
 * 流程：
 * 1. binary 检查：不可用 -> 提示安装（一键跑 install.ps1）
 * 2. API key 检查：未配置 -> 引导获取 + 输入并保存到 SecretStorage
 * 3. 全部就绪后标记完成（下次不再打扰）
 *
 * @param deps 注入的依赖
 * @param config 当前配置
 * @param force 强制运行（手动触发时 true，忽略完成标记）
 * @returns 就绪结果
 */
export async function runSetupWizard(
    deps: SetupWizardDeps,
    config: ClawConfig,
    force = false,
): Promise<WizardResult> {
    // 非强制模式：已完成过则直接返回（不打扰用户）
    if (!force && (await deps.isDone())) {
        return { binaryReady: true, apiKeyReady: true, cancelled: false };
    }

    // 1. binary 检查
    let binaryReady = await deps.checkBinary(config.binaryPath);
    if (!binaryReady) {
        const action = await deps.showError(
            `找不到 Claw 二进制（${config.binaryPath}）。` +
                '可以手动选择二进制文件，或运行安装脚本。',
            '选择文件',
            '一键安装',
            '稍后再说',
        );
        if (action === '选择文件') {
            // 文件选择器定位，根治 GUI 进程 PATH 不一致导致的 ENOENT
            const picked = await deps.pickBinary();
            if (picked) {
                await deps.saveBinaryPath(picked);
                binaryReady = await deps.checkBinary(picked);
            }
        } else if (action === '一键安装') {
            await deps.runInstaller();
            // 安装后自动重查一次
            binaryReady = await deps.checkBinary(config.binaryPath);
        } else {
            return { binaryReady: false, apiKeyReady: false, cancelled: true };
        }
    }

    // 2. API key 检查（SecretStorage 优先，其次环境变量）
    let apiKey = await deps.getApiKey();
    if (!apiKey && process.env.DEEPSEEK_API_KEY) {
        // 环境变量已有：顺带存一份到 SecretStorage，让扩展不依赖环境
        await deps.setApiKey(process.env.DEEPSEEK_API_KEY);
        apiKey = process.env.DEEPSEEK_API_KEY;
    }
    if (!apiKey) {
        const action = await deps.showInfo(
            'Claw 需要一个 DeepSeek API Key 才能对话。现在配置？',
            '获取 API Key',
            '输入 Key',
            '稍后再说',
        );
        if (action === '获取 API Key') {
            await deps.openExternal('https://platform.deepseek.com/api_keys');
            apiKey = await deps.promptApiKey('粘贴你的 DeepSeek API Key');
            if (apiKey) {
                await deps.setApiKey(apiKey);
            }
        } else if (action === '输入 Key') {
            apiKey = await deps.promptApiKey('粘贴你的 DeepSeek API Key');
            if (apiKey) {
                await deps.setApiKey(apiKey);
            }
        }
    }

    const result: WizardResult = {
        binaryReady,
        apiKeyReady: !!apiKey,
        cancelled: !binaryReady || (!apiKey && !(await deps.getApiKey())),
    };

    // 全部就绪才标记完成；未就绪留待下次提示
    if (result.binaryReady && result.apiKeyReady) {
        await deps.markDone();
    }
    return result;
}
