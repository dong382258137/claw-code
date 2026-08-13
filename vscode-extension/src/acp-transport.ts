// ACP 传输层：封装 claw-headless 子进程的 stdin/stdout 管道，
// 处理 JSON-RPC 2.0 framing（每行一条 NDJSON）。
//
// 设计原则：
// 1. 自实现 NDJSON framing，不依赖 vscode-languageserver（更轻量、可独立测试）
// 2. 核心可测试：通过 ITransportInput/ITransportOutput 抽象注入 fake stdin/stdout
// 3. 错误隔离：子进程崩溃不传染主 extension
// 4. 支持正向 request/response、反向 request、notification 三种消息类型

import { spawn, ChildProcess } from 'child_process';
import { EventEmitter } from 'events';
import * as readline from 'readline';
import type {
    AcpRequest,
    AcpNotification,
    AcpResponse,
    AcpSuccessResponse,
    AcpErrorResponse,
} from './types';
import { AcpErrorCode } from './types';

/** 传输层接收到的任意 JSON-RPC 消息（宽松类型，运行时按字段存在性判断） */
type AcpMessage = Record<string, unknown>;

/** 抽象 stdout 读取（便于测试注入） */
export interface ITransportInput {
    on(event: 'line', listener: (line: string) => void): this;
    on(event: 'close', listener: () => void): this;
    close(): void;
}

/** 抽象 stdin 写入（便于测试注入） */
export interface ITransportOutput {
    write(chunk: string | Buffer, cb?: (err?: Error | null) => void): boolean;
    end(cb?: () => void): void;
}

/** 进程退出信息 */
export interface ProcessExitInfo {
    code: number | null;
    signal: NodeJS.Signals | null;
}

export interface AcpTransportOptions {
    binaryPath: string;
    args: string[];
    cwd?: string;
    env?: Record<string, string>;
}

type PendingRequest = {
    resolve: (r: unknown) => void;
    reject: (e: Error) => void;
    timer?: NodeJS.Timeout;
};

/** 反向请求 handler 注册表 */
type ReverseRequestHandler = (params: unknown) => Promise<unknown>;

const DEFAULT_REQUEST_TIMEOUT_MS = 120_000; // 2 分钟，prompt 可能耗时
const DEFAULT_REVERSE_TIMEOUT_MS = 60_000; // 60s 等待用户操作

/**
 * ACP 传输层
 *
 * 事件：
 * - 'stderr' (line: string): 子进程 stderr 一行
 * - 'exit' (info: ProcessExitInfo): 子进程退出
 * - 'error' (err: Error): 子进程 spawn 错误
 * - 'notification:<method>' (params: unknown): agent → IDE 通知
 * - 'parse-error' ({line, error}): stdout 行解析失败
 * - 'unknown-message' (msg: AcpMessage): 无法分类的消息
 */
export class AcpTransport extends EventEmitter {
    private process: ChildProcess | null = null;
    private stdoutRL: readline.Interface | null = null;
    private nextId = 1;
    private pending = new Map<number | string, PendingRequest>();
    private reverseHandlers = new Map<string, ReverseRequestHandler>();
    private injectedInput: ITransportInput | null = null;
    private injectedOutput: ITransportOutput | null = null;

    constructor(private options: AcpTransportOptions) {
        super();
    }

    /**
     * 测试用：注入 fake input/output，绕过真实子进程。
     * 必须在 start() 之前调用，且此时 start() 不会 spawn 子进程。
     */
    withInjectedIO(input: ITransportInput, output: ITransportOutput): this {
        this.injectedInput = input;
        this.injectedOutput = output;
        return this;
    }

    /** 启动 claw-headless 子进程，初始化 JSON-RPC 通道 */
    async start(): Promise<void> {
        if (this.process || this.injectedInput) {
            // 测试模式或已启动
            if (this.injectedInput) {
                this.injectedInput.on('line', (line) => this.handleLine(line));
                this.injectedInput.on('close', () => this.handleExit({ code: 0, signal: null }));
                return;
            }
            throw new Error('Transport already started');
        }

        const { binaryPath, args, cwd, env } = this.options;
        try {
            this.process = spawn(binaryPath, args, {
                cwd,
                env: { ...process.env, ...env },
                stdio: ['pipe', 'pipe', 'pipe'],
            });
        } catch (err) {
            const e = new Error(
                `Failed to spawn ${binaryPath}: ${(err as Error).message}`,
            );
            this.emit('error', e);
            throw e;
        }

        if (!this.process.stdout || !this.process.stdin) {
            throw new Error('Spawned process missing stdio pipes');
        }

        this.stdoutRL = readline.createInterface({
            input: this.process.stdout,
            crlfDelay: Infinity,
        });
        this.stdoutRL.on('line', (line) => this.handleLine(line));

        this.process.stderr?.on('data', (data: Buffer) => {
            this.emit('stderr', data.toString());
        });

        // spawn 失败（ENOENT 等）:emit error 并把 process 置 null,
        // 否则 isRunning() 误报 true、request() 抛 "Transport not started" 且无日志。
        this.process.on('error', (err) => {
            this.emit('error', err);
            this.process = null;
            this.stdoutRL?.close();
            this.stdoutRL = null;
        });
        this.process.on('exit', (code, signal) =>
            this.handleExit({ code, signal: signal as NodeJS.Signals | null }),
        );
    }

    /** 发送 JSON-RPC request，返回 Promise 等待响应 */
    async request(method: string, params?: unknown, timeoutMs = DEFAULT_REQUEST_TIMEOUT_MS): Promise<unknown> {
        const output = this.getOutput();
        if (!output) {
            throw new Error('Transport not started');
        }
        const id = this.nextId++;
        const req: AcpRequest = { jsonrpc: '2.0', method, params, id };
        return new Promise((resolve, reject) => {
            const timer = setTimeout(() => {
                if (this.pending.delete(id)) {
                    reject(new Error(`Request ${method} timed out after ${timeoutMs}ms`));
                }
            }, timeoutMs);
            this.pending.set(id, { resolve, reject, timer });

            const line = JSON.stringify(req) + '\n';
            output.write(line, (err) => {
                if (err) {
                    if (this.pending.delete(id)) {
                        clearTimeout(timer);
                        reject(new Error(`Failed to write request ${method}: ${err.message}`));
                    }
                }
            });
        });
    }

    /** 发送 JSON-RPC notification（无 id，无响应） */
    notify(method: string, params?: unknown): void {
        const output = this.getOutput();
        if (!output) return;
        const notif: AcpNotification = { jsonrpc: '2.0', method, params };
        const line = JSON.stringify(notif) + '\n';
        output.write(line);
    }

    /** 注册反向请求 handler（IDE 接收 agent 的反向请求） */
    onReverseRequest(method: string, handler: ReverseRequestHandler, timeoutMs = DEFAULT_REVERSE_TIMEOUT_MS): void {
        // 用单一 handler 覆盖；同一方法不支持多 handler
        this.reverseHandlers.set(method, async (params) => {
            // 包装一层超时，handler 内部可能 await 用户操作
            return Promise.race([
                handler(params),
                new Promise((_, reject) =>
                    setTimeout(() => reject(new Error(`Reverse request ${method} handler timed out`)), timeoutMs),
                ),
            ]);
        });
    }

    /** 关闭 transport：发送 exit notification + 杀进程 */
    async stop(): Promise<void> {
        if (this.injectedInput) {
            this.injectedInput.close();
            this.injectedInput = null;
            this.injectedOutput = null;
            // reject 所有 pending
            for (const { reject, timer } of this.pending.values()) {
                clearTimeout(timer);
                reject(new Error('Transport stopped'));
            }
            this.pending.clear();
            return;
        }

        if (!this.process) return;
        try {
            this.notify('exit');
            // 等待 2s 让进程自然退出
            await new Promise((resolve) => setTimeout(resolve, 2000));
        } catch {
            // 忽略写入错误
        }
        if (this.process) {
            this.process.kill('SIGTERM');
            // 5s 后强制 kill
            setTimeout(() => this.process?.kill('SIGKILL'), 5000);
        }
    }

    /** 当前是否处于运行状态 */
    isRunning(): boolean {
        return this.process !== null || this.injectedInput !== null;
    }

    // ===== 内部方法 =====

    private getOutput(): ITransportOutput | null {
        if (this.injectedOutput) return this.injectedOutput;
        if (this.process?.stdin?.writable) return this.process.stdin;
        return null;
    }

    private handleLine(line: string): void {
        if (!line.trim()) return;
        let msg: AcpMessage;
        try {
            msg = JSON.parse(line) as AcpMessage;
        } catch (err) {
            this.emit('parse-error', { line, error: err });
            return;
        }

        const id = msg.id as number | string | undefined;
        const hasResult = 'result' in msg;
        const hasError = 'error' in msg;
        const method = msg.method as string | undefined;

        // Response（匹配 pending request）
        if (id !== undefined && hasResult) {
            const pending = this.pending.get(id);
            if (pending) {
                clearTimeout(pending.timer);
                this.pending.delete(id);
                pending.resolve((msg as unknown as AcpSuccessResponse).result);
            }
            return;
        }
        if (id !== undefined && hasError) {
            const pending = this.pending.get(id);
            if (pending) {
                clearTimeout(pending.timer);
                this.pending.delete(id);
                const err = (msg as unknown as AcpErrorResponse).error;
                pending.reject(new Error(`${err.code}: ${err.message}`));
            }
            return;
        }

        // Notification（agent → IDE，无 id）
        if (id === undefined && method) {
            this.emit(`notification:${method}`, (msg as unknown as AcpNotification).params);
            return;
        }

        // Reverse request（agent → IDE 请求，有 id 和 method）
        if (id !== undefined && method) {
            void this.handleReverseRequest(msg as unknown as AcpRequest);
            return;
        }

        this.emit('unknown-message', msg);
    }

    private async handleReverseRequest(req: AcpRequest): Promise<void> {
        const handler = this.reverseHandlers.get(req.method);
        const output = this.getOutput();
        if (!output) return;

        if (!handler) {
            const resp: AcpResponse = {
                jsonrpc: '2.0',
                id: req.id,
                error: { code: AcpErrorCode.METHOD_NOT_FOUND, message: `No handler for ${req.method}` },
            };
            output.write(JSON.stringify(resp) + '\n');
            return;
        }

        try {
            const result = await handler(req.params);
            const resp: AcpResponse = { jsonrpc: '2.0', id: req.id, result };
            output.write(JSON.stringify(resp) + '\n');
        } catch (err) {
            const resp: AcpResponse = {
                jsonrpc: '2.0',
                id: req.id,
                error: { code: AcpErrorCode.INTERNAL_ERROR, message: (err as Error).message },
            };
            output.write(JSON.stringify(resp) + '\n');
        }
    }

    private handleExit(info: ProcessExitInfo): void {
        this.emit('exit', info);
        const err = new Error(`claw-headless exited: code=${info.code} signal=${info.signal}`);
        for (const { reject, timer } of this.pending.values()) {
            clearTimeout(timer);
            reject(err);
        }
        this.pending.clear();
        this.process = null;
        this.stdoutRL?.close();
        this.stdoutRL = null;
        this.injectedInput = null;
        this.injectedOutput = null;
    }
}

/** 测试用：基于内存 Buffer 的 ITransportInput 实现 */
export class MemoryInput implements ITransportInput {
    private emitter = new EventEmitter();
    private lines: string[] = [];

    on(event: 'line', listener: (line: string) => void): this;
    on(event: 'close', listener: () => void): this;
    on(event: 'line' | 'close', listener: ((line: string) => void) | (() => void)): this {
        this.emitter.on(event, listener as (...args: unknown[]) => void);
        return this;
    }

    /** 测试驱动：模拟子进程发送一行 JSON-RPC 消息 */
    emitLine(line: string): void {
        this.lines.push(line);
        this.emitter.emit('line', line);
    }

    close(): void {
        this.emitter.emit('close');
    }
}

/** 测试用：基于内存 Buffer 的 ITransportOutput 实现，记录所有写入 */
export class MemoryOutput implements ITransportOutput {
    public written: string[] = [];

    write(chunk: string | Buffer, cb?: (err?: Error | null) => void): boolean {
        this.written.push(typeof chunk === 'string' ? chunk : chunk.toString());
        cb?.(null);
        return true;
    }

    end(cb?: () => void): void {
        cb?.();
    }

    /** 读取第 N 条写入（0-based） */
    lineAt(index: number): string | undefined {
        return this.written[index]?.trim();
    }

    /** 解析第 N 条写入为 JSON */
    jsonAt(index: number): unknown {
        const line = this.lineAt(index);
        return line ? JSON.parse(line) : undefined;
    }
}

// 避免未使用导入警告
export type _AcpErrorCode = typeof AcpErrorCode;
void (undefined as unknown as _AcpErrorCode);
