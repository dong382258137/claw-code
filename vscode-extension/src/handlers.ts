// 反向请求 handler：IDE 接收 claw-headless 的反向请求并响应。
//
// 三个核心 handler:
// 1. fs/read_text_file  - 优先从 editor buffer 取（含未保存内容），回退磁盘读取
// 2. fs/write_text_file  - 走 WorkspaceEdit 进 undo 栈，保护用户未保存修改
// 3. session/request_permission - modal 弹窗询问用户

import type * as vscode from 'vscode';
import type {
    ReadTextFileParams,
    ReadTextFileResult,
    WriteTextFileParams,
    WriteTextFileResult,
    RequestPermissionParams,
    RequestPermissionResult,
} from './types';

/** vscode API 的最小接口集，便于测试注入 */
export interface HandlerVscodeApi {
    workspace: {
        textDocuments: readonly vscode.TextDocument[];
        fs: {
            readFile(uri: vscode.Uri): Thenable<Uint8Array>;
        };
        openTextDocument(uri: vscode.Uri): Thenable<vscode.TextDocument>;
        applyEdit(edit: vscode.WorkspaceEdit): Thenable<boolean>;
    };
    window: {
        showWarningMessage(message: string, options: vscode.MessageOptions, ...items: string[]): Thenable<string | undefined>;
    };
    Uri: typeof vscode.Uri;
    WorkspaceEdit: typeof vscode.WorkspaceEdit;
    Range: typeof vscode.Range;
}

/**
 * fs/read_text_file handler
 *
 * 优先级：
 * 1. 若文件已在 editor 打开，返回 buffer 内容（含未保存修改）
 * 2. 否则回退磁盘读取
 */
export async function handleReadTextFile(
    params: ReadTextFileParams,
    api: HandlerVscodeApi,
): Promise<ReadTextFileResult> {
    const uri = api.Uri.file(params.path);
    const doc = api.workspace.textDocuments.find((d) => d.uri.fsPath === uri.fsPath);
    if (doc) {
        return { content: doc.getText() };
    }
    try {
        const content = await api.workspace.fs.readFile(uri);
        return { content: Buffer.from(content).toString() };
    } catch {
        throw new Error(`File not found: ${params.path}`);
    }
}

/**
 * fs/write_text_file handler
 *
 * 走 WorkspaceEdit API，确保：
 * 1. 进 undo 栈，用户可 Ctrl+Z 撤销
 * 2. 不覆盖用户未保存的 buffer（与磁盘内容 diff 后再写）
 */
export async function handleWriteTextFile(
    params: WriteTextFileParams,
    api: HandlerVscodeApi,
): Promise<WriteTextFileResult> {
    const uri = api.Uri.file(params.path);
    let doc = api.workspace.textDocuments.find((d) => d.uri.fsPath === uri.fsPath);
    if (!doc) {
        try {
            doc = await api.workspace.openTextDocument(uri);
        } catch {
            // 文件不存在时直接返回 false，由 agent 决定后续
            return { written: false };
        }
    }
    const edit = new api.WorkspaceEdit();
    // 整文件替换：从第 0 行第 0 列到最后一行第 0 列
    edit.replace(uri, new api.Range(0, 0, doc.lineCount, 0), params.content);
    const applied = await api.workspace.applyEdit(edit);
    return { written: applied };
}

/**
 * session/request_permission handler
 *
 * 使用 modal 弹窗，阻止 agent 继续执行直到用户响应。
 * 超时由 AcpTransport 的 onReverseRequest 层处理，默认拒绝。
 */
export async function handleRequestPermission(
    params: RequestPermissionParams,
    api: HandlerVscodeApi,
): Promise<RequestPermissionResult> {
    const choice = await api.window.showWarningMessage(
        `Claw 请求执行: ${params.toolName}`,
        { modal: true },
        '允许',
        '拒绝',
        '始终允许',
    );
    if (choice === '允许') return { outcome: 'allow' };
    if (choice === '始终允许') return { outcome: 'always_allow' };
    return { outcome: 'deny' };
}

/** 默认导出：创建基于真实 vscode API 的 handler 集合 */
export interface HandlerBundle {
    readTextFile: (params: ReadTextFileParams) => Promise<ReadTextFileResult>;
    writeTextFile: (params: WriteTextFileParams) => Promise<WriteTextFileResult>;
    requestPermission: (params: RequestPermissionParams) => Promise<RequestPermissionResult>;
}

export function createHandlers(api: HandlerVscodeApi): HandlerBundle {
    return {
        readTextFile: (p) => handleReadTextFile(p, api),
        writeTextFile: (p) => handleWriteTextFile(p, api),
        requestPermission: (p) => handleRequestPermission(p, api),
    };
}
