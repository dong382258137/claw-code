// Setup wizard 单元测试
//
// 通过 mock SetupWizardDeps 注入，测试 runSetupWizard 的三种核心分支：
// 1. 已完成过且非 force -> 直接返回就绪，不打扰
// 2. binary 缺失 -> 引导安装（一键安装 / 稍后再说）
// 3. API key 缺失 -> 引导获取 + 输入保存
// 4. 全部就绪 -> markDone 被调用
//
// 注：checkBinaryAvailable 是真实 spawn 冒烟，单独测一个 ENOENT 路径。

import * as assert from 'assert';
import {
    runSetupWizard,
    checkBinaryAvailable,
    type SetupWizardDeps,
} from '../../src/setup-wizard';
import type { ClawConfig } from '../../src/types';

const TEST_CONFIG: ClawConfig = {
    binaryPath: 'claw-plus-headless',
    model: 'deepseek-v4-flash',
    permissionMode: 'workspace-write',
    autoStart: false,
    logLevel: 'info',
};

// ===== mock 工厂 =====

function mockDeps(overrides: Partial<SetupWizardDeps> = {}): {
    deps: SetupWizardDeps;
    calls: {
        checkBinary: string[];
        setApiKey: string[];
        markDone: number;
        runInstaller: number;
        openExternal: string[];
        isDone: boolean;
    };
} {
    const calls = {
        checkBinary: [] as string[],
        setApiKey: [] as string[],
        markDone: 0,
        runInstaller: 0,
        openExternal: [] as string[],
        isDone: false,
    };
    const deps: SetupWizardDeps = {
        checkBinary: async (binaryPath) => {
            calls.checkBinary.push(binaryPath);
            return true;
        },
        pickBinary: async () => undefined,
        saveBinaryPath: async () => {},
        getApiKey: async () => 'sk-test-key',
        setApiKey: async (key) => {
            calls.setApiKey.push(key);
        },
        promptApiKey: async () => undefined,
        showError: async () => undefined,
        showInfo: async () => undefined,
        openExternal: async (url) => {
            calls.openExternal.push(url);
        },
        runInstaller: async () => {
            calls.runInstaller++;
        },
        isDone: async () => calls.isDone,
        markDone: async () => {
            calls.markDone++;
        },
        ...overrides,
    };
    return { deps, calls };
}

suite('setup-wizard', () => {
    // 隔离 DEEPSEEK_API_KEY 环境变量：本机可能已配置真实 key，
    // 会污染"key 缺失"分支的断言。每个测试前清除，测试后恢复。
    let savedEnvKey: string | undefined;
    setup(() => {
        savedEnvKey = process.env.DEEPSEEK_API_KEY;
        delete process.env.DEEPSEEK_API_KEY;
    });
    teardown(() => {
        if (savedEnvKey === undefined) {
            delete process.env.DEEPSEEK_API_KEY;
        } else {
            process.env.DEEPSEEK_API_KEY = savedEnvKey;
        }
    });

    test('已完成过且非 force：直接返回就绪，不检查 binary/key', async () => {
        const { deps, calls } = mockDeps();
        calls.isDone = true;

        const result = await runSetupWizard(deps, TEST_CONFIG, false);

        assert.strictEqual(result.binaryReady, true);
        assert.strictEqual(result.apiKeyReady, true);
        assert.strictEqual(result.cancelled, false);
        assert.deepStrictEqual(calls.checkBinary, [], '不应检查 binary');
        assert.strictEqual(calls.markDone, 0, '不应重复 markDone');
    });

    test('已完成过但 force=true：强制重新运行', async () => {
        const { deps, calls } = mockDeps();
        calls.isDone = true;

        const result = await runSetupWizard(deps, TEST_CONFIG, true);

        assert.strictEqual(result.binaryReady, true);
        assert.deepStrictEqual(calls.checkBinary, ['claw-plus-headless'], '应重新检查 binary');
    });

    test('binary 缺失 + 一键安装：调用安装器后重查，返回未就绪', async () => {
        let binaryOk = false;
        const { deps, calls } = mockDeps({
            checkBinary: async () => {
                calls.checkBinary.push(TEST_CONFIG.binaryPath);
                const ok = binaryOk;
                // 第二次重查（安装后）返回 true
                binaryOk = true;
                return ok;
            },
            showError: async () => '一键安装',
            getApiKey: async () => 'sk-test-key',
        });

        const result = await runSetupWizard(deps, TEST_CONFIG, false);

        assert.strictEqual(calls.runInstaller, 1, '应触发一键安装');
        assert.strictEqual(result.binaryReady, true, '安装后重查应就绪');
        assert.strictEqual(calls.checkBinary.length, 2, '应重查一次');
    });

    test('binary 缺失 + 稍后再说：返回 cancelled', async () => {
        const { deps, calls } = mockDeps({
            checkBinary: async () => false,
            showError: async () => '稍后再说',
        });

        const result = await runSetupWizard(deps, TEST_CONFIG, false);

        assert.strictEqual(result.cancelled, true);
        assert.strictEqual(calls.runInstaller, 0, '不应触发安装');
        assert.strictEqual(calls.markDone, 0, '取消不应 markDone');
    });

    test('API key 缺失 + 获取并输入：保存 key 且就绪', async () => {
        const { deps, calls } = mockDeps({
            getApiKey: async () => undefined,
            showInfo: async () => '获取 API Key',
            promptApiKey: async () => 'sk-123',
        });

        const result = await runSetupWizard(deps, TEST_CONFIG, false);

        assert.strictEqual(result.apiKeyReady, true);
        assert.deepStrictEqual(calls.openExternal, [
            'https://platform.deepseek.com/api_keys',
        ]);
        assert.deepStrictEqual(calls.setApiKey, ['sk-123']);
        assert.strictEqual(calls.markDone, 1, '就绪后应 markDone');
    });

    test('API key 缺失 + 直接输入：不打开浏览器也保存', async () => {
        const { deps, calls } = mockDeps({
            getApiKey: async () => undefined,
            showInfo: async () => '输入 Key',
            promptApiKey: async () => 'sk-456',
        });

        const result = await runSetupWizard(deps, TEST_CONFIG, false);

        assert.strictEqual(result.apiKeyReady, true);
        assert.deepStrictEqual(calls.openExternal, [], '不应打开浏览器');
        assert.deepStrictEqual(calls.setApiKey, ['sk-456']);
    });

    test('API key 缺失 + 稍后再说：未就绪，不 markDone', async () => {
        const { deps, calls } = mockDeps({
            getApiKey: async () => undefined,
            showInfo: async () => '稍后再说',
        });

        const result = await runSetupWizard(deps, TEST_CONFIG, false);

        assert.strictEqual(result.apiKeyReady, false);
        assert.strictEqual(calls.setApiKey.length, 0);
        assert.strictEqual(calls.markDone, 0);
    });

    test('环境变量已有 key：自动存入 SecretStorage', async () => {
        const { deps, calls } = mockDeps({
            getApiKey: async () => undefined,
        });
        const savedEnv = process.env.DEEPSEEK_API_KEY;
        process.env.DEEPSEEK_API_KEY = 'sk-env-key';
        try {
            const result = await runSetupWizard(deps, TEST_CONFIG, false);
            assert.strictEqual(result.apiKeyReady, true);
            assert.deepStrictEqual(calls.setApiKey, ['sk-env-key']);
            assert.strictEqual(calls.markDone, 1);
        } finally {
            if (savedEnv === undefined) {
                delete process.env.DEEPSEEK_API_KEY;
            } else {
                process.env.DEEPSEEK_API_KEY = savedEnv;
            }
        }
    });

    test('checkBinaryAvailable：不存在的命令返回 false', async () => {
        // 用不可能存在的命令名，验证 ENOENT 路径
        const ok = await checkBinaryAvailable('claw-this-binary-does-not-exist-xyz');
        assert.strictEqual(ok, false);
    });
});
