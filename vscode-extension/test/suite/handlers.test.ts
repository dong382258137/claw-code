// Handlers 单元测试
//
// 通过 mock HandlerVscodeApi 注入，测试：
// 1. fs/read_text_file 优先返回 editor buffer
// 2. fs/read_text_file 回退磁盘读取
// 3. fs/read_text_file 文件不存在抛错
// 4. fs/write_text_file 走 WorkspaceEdit
// 5. session/request_permission allow/deny/always_allow

import * as assert from 'assert';
import {
    handleReadTextFile,
    handleWriteTextFile,
    handleRequestPermission,
    type HandlerVscodeApi,
} from '../../src/handlers';
import type * as vscode from 'vscode';

// ===== mock 工厂 =====

function mockTextDocument(text: string, fsPath: string): vscode.TextDocument {
    return {
        getText: () => text,
        uri: { fsPath } as vscode.Uri,
    } as unknown as vscode.TextDocument;
}

function mockVscodeApi(opts: {
    documents?: vscode.TextDocument[];
    readFileImpl?: (uri: { fsPath: string }) => Promise<Uint8Array>;
    applyEditResult?: boolean;
    openDocResult?: vscode.TextDocument | null;
    showWarningChoice?: string | undefined;
}): HandlerVscodeApi {
    return {
        workspace: {
            textDocuments: opts.documents ?? [],
            fs: {
                readFile: (uri: vscode.Uri) =>
                    opts.readFileImpl
                        ? opts.readFileImpl(uri)
                        : Promise.reject(new Error('not found')),
            },
            openTextDocument: async () => {
                if (!opts.openDocResult) throw new Error('openTextDocument failed');
                return opts.openDocResult;
            },
            applyEdit: async () => opts.applyEditResult ?? true,
        },
        window: {
            showWarningMessage: async () => opts.showWarningChoice,
        },
        Uri: {
            file: (p: string) => ({ fsPath: p } as vscode.Uri),
        } as typeof vscode.Uri,
        WorkspaceEdit: class {
            replace() {}
        } as unknown as typeof vscode.WorkspaceEdit,
        Range: class {
            constructor(
                _a: number,
                _b: number,
                _c: number,
                _d: number,
            ) {}
        } as unknown as typeof vscode.Range,
    };
}

suite('handlers', () => {
    suite('handleReadTextFile', () => {
        test('returns editor buffer content when document is open', async () => {
            const api = mockVscodeApi({
                documents: [mockTextDocument('buffer content', '/tmp/open.ts')],
            });
            const result = await handleReadTextFile({ path: '/tmp/open.ts' }, api);
            assert.strictEqual(result.content, 'buffer content');
        });

        test('falls back to disk read when document not open', async () => {
            const api = mockVscodeApi({
                documents: [],
                readFileImpl: async () => new TextEncoder().encode('disk content'),
            });
            const result = await handleReadTextFile({ path: '/tmp/closed.ts' }, api);
            assert.strictEqual(result.content, 'disk content');
        });

        test('throws when file not found on disk', async () => {
            const api = mockVscodeApi({
                documents: [],
                readFileImpl: async () => {
                    throw new Error('ENOENT');
                },
            });
            await assert.rejects(
                handleReadTextFile({ path: '/tmp/missing.ts' }, api),
                /File not found/,
            );
        });
    });

    suite('handleWriteTextFile', () => {
        test('writes via WorkspaceEdit when file already open', async () => {
            const doc = mockTextDocument('old', '/tmp/open.ts');
            const api = mockVscodeApi({
                documents: [doc],
                applyEditResult: true,
            });
            const result = await handleWriteTextFile(
                { path: '/tmp/open.ts', content: 'new content' },
                api,
            );
            assert.strictEqual(result.written, true);
        });

        test('opens document then writes when file not open', async () => {
            const doc = mockTextDocument('old', '/tmp/closed.ts');
            const api = mockVscodeApi({
                documents: [],
                openDocResult: doc,
                applyEditResult: true,
            });
            const result = await handleWriteTextFile(
                { path: '/tmp/closed.ts', content: 'new content' },
                api,
            );
            assert.strictEqual(result.written, true);
        });

        test('returns written:false when applyEdit fails', async () => {
            const doc = mockTextDocument('old', '/tmp/x.ts');
            const api = mockVscodeApi({
                documents: [doc],
                applyEditResult: false,
            });
            const result = await handleWriteTextFile(
                { path: '/tmp/x.ts', content: 'new' },
                api,
            );
            assert.strictEqual(result.written, false);
        });

        test('returns written:false when openTextDocument fails (file does not exist)', async () => {
            const api = mockVscodeApi({
                documents: [],
                openDocResult: null, // openTextDocument will throw
            });
            const result = await handleWriteTextFile(
                { path: '/tmp/missing.ts', content: 'new' },
                api,
            );
            assert.strictEqual(result.written, false);
        });
    });

    suite('handleRequestPermission', () => {
        test('returns allow when user selects "允许"', async () => {
            const api = mockVscodeApi({ showWarningChoice: '允许' });
            const result = await handleRequestPermission(
                { toolName: 'Bash', toolInput: {}, options: [] },
                api,
            );
            assert.strictEqual(result.outcome, 'allow');
        });

        test('returns always_allow when user selects "始终允许"', async () => {
            const api = mockVscodeApi({ showWarningChoice: '始终允许' });
            const result = await handleRequestPermission(
                { toolName: 'Bash', toolInput: {}, options: [] },
                api,
            );
            assert.strictEqual(result.outcome, 'always_allow');
        });

        test('returns deny when user dismisses dialog', async () => {
            const api = mockVscodeApi({ showWarningChoice: undefined });
            const result = await handleRequestPermission(
                { toolName: 'Bash', toolInput: {}, options: [] },
                api,
            );
            assert.strictEqual(result.outcome, 'deny');
        });

        test('returns deny when user selects "拒绝"', async () => {
            const api = mockVscodeApi({ showWarningChoice: '拒绝' });
            const result = await handleRequestPermission(
                { toolName: 'Bash', toolInput: {}, options: [] },
                api,
            );
            assert.strictEqual(result.outcome, 'deny');
        });
    });
});
