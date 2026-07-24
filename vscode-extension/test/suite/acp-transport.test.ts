// AcpTransport 单元测试
//
// 通过 MemoryInput / MemoryOutput 注入，绕过真实子进程，测试：
// 1. 正向 request/response 配对
// 2. notification 派发
// 3. 反向 request handler 调用
// 4. 错误响应 reject
// 5. 超时 reject
// 6. parse-error 事件

import * as assert from 'assert';
import { AcpTransport, MemoryInput, MemoryOutput } from '../../src/acp-transport';

suite('AcpTransport', () => {
    test('request sends JSON-RPC request and resolves on response', async () => {
        const input = new MemoryInput();
        const output = new MemoryOutput();
        const transport = new AcpTransport({
            binaryPath: 'fake',
            args: [],
        }).withInjectedIO(input, output);
        await transport.start();

        // 模拟异步响应
        const reqPromise = transport.request('initialize', { protocolVersion: 1 });
        // 等待 output 写入
        await new Promise((r) => setImmediate(r));
        const sent = JSON.parse(output.lineAt(0) as string) as { id: number; method: string };
        input.emitLine(JSON.stringify({ jsonrpc: '2.0', id: sent.id, result: { ok: true } }));

        const result = await reqPromise;
        assert.deepStrictEqual(result, { ok: true });
        assert.strictEqual(sent.method, 'initialize');
        await transport.stop();
    });

    test('error response rejects with code and message', async () => {
        const input = new MemoryInput();
        const output = new MemoryOutput();
        const transport = new AcpTransport({
            binaryPath: 'fake',
            args: [],
        }).withInjectedIO(input, output);
        await transport.start();

        const reqPromise = transport.request('unknown/method');
        await new Promise((r) => setImmediate(r));
        const sent = JSON.parse(output.lineAt(0) as string) as { id: number };
        input.emitLine(
            JSON.stringify({
                jsonrpc: '2.0',
                id: sent.id,
                error: { code: -32601, message: 'method not found' },
            }),
        );

        await assert.rejects(reqPromise, /-32601: method not found/);
        await transport.stop();
    });

    test('notification emits event with params', async () => {
        const input = new MemoryInput();
        const output = new MemoryOutput();
        const transport = new AcpTransport({
            binaryPath: 'fake',
            args: [],
        }).withInjectedIO(input, output);
        await transport.start();

        let received: unknown = null;
        transport.on('notification:session/update', (params) => {
            received = params;
        });
        input.emitLine(
            JSON.stringify({
                jsonrpc: '2.0',
                method: 'session/update',
                params: { sessionId: 's1', update: { type: 'AgentMessageChunk' } },
            }),
        );

        await new Promise((r) => setImmediate(r));
        assert.deepStrictEqual(received, {
            sessionId: 's1',
            update: { type: 'AgentMessageChunk' },
        });
        await transport.stop();
    });

    test('reverse request invokes registered handler and writes response', async () => {
        const input = new MemoryInput();
        const output = new MemoryOutput();
        const transport = new AcpTransport({
            binaryPath: 'fake',
            args: [],
        }).withInjectedIO(input, output);
        await transport.start();

        transport.onReverseRequest('fs/read_text_file', async (params) => {
            assert.deepStrictEqual(params, { path: '/tmp/x' });
            return { content: 'hello' };
        });

        input.emitLine(
            JSON.stringify({
                jsonrpc: '2.0',
                id: 42,
                method: 'fs/read_text_file',
                params: { path: '/tmp/x' },
            }),
        );

        // 等待 handler 执行 + 响应写入
        await new Promise((r) => setTimeout(r, 50));
        const resp = JSON.parse(output.lineAt(0) as string) as {
            id: number;
            result: { content: string };
        };
        assert.strictEqual(resp.id, 42);
        assert.strictEqual(resp.result.content, 'hello');
        await transport.stop();
    });

    test('reverse request with unknown method returns -32601', async () => {
        const input = new MemoryInput();
        const output = new MemoryOutput();
        const transport = new AcpTransport({
            binaryPath: 'fake',
            args: [],
        }).withInjectedIO(input, output);
        await transport.start();

        input.emitLine(
            JSON.stringify({
                jsonrpc: '2.0',
                id: 1,
                method: 'session/fork', // 未注册 handler
                params: {},
            }),
        );

        await new Promise((r) => setTimeout(r, 50));
        const resp = JSON.parse(output.lineAt(0) as string) as {
            id: number;
            error: { code: number; message: string };
        };
        assert.strictEqual(resp.id, 1);
        assert.strictEqual(resp.error.code, -32601);
        assert.ok(resp.error.message.includes('session/fork'));
        await transport.stop();
    });

    test('reverse request handler throwing returns -32603', async () => {
        const input = new MemoryInput();
        const output = new MemoryOutput();
        const transport = new AcpTransport({
            binaryPath: 'fake',
            args: [],
        }).withInjectedIO(input, output);
        await transport.start();

        transport.onReverseRequest('fs/read_text_file', async () => {
            throw new Error('boom');
        });

        input.emitLine(
            JSON.stringify({
                jsonrpc: '2.0',
                id: 7,
                method: 'fs/read_text_file',
                params: {},
            }),
        );

        await new Promise((r) => setTimeout(r, 50));
        const resp = JSON.parse(output.lineAt(0) as string) as {
            id: number;
            error: { code: number; message: string };
        };
        assert.strictEqual(resp.id, 7);
        assert.strictEqual(resp.error.code, -32603);
        assert.ok(resp.error.message.includes('boom'));
        await transport.stop();
    });

    test('invalid JSON line emits parse-error event', async () => {
        const input = new MemoryInput();
        const output = new MemoryOutput();
        const transport = new AcpTransport({
            binaryPath: 'fake',
            args: [],
        }).withInjectedIO(input, output);
        await transport.start();

        let parseErr: { line: string; error: unknown } | null = null;
        transport.on('parse-error', (e: { line: string; error: unknown }) => {
            parseErr = e as { line: string; error: unknown };
        });

        input.emitLine('not json');
        await new Promise((r) => setImmediate(r));
        assert.ok(parseErr);
        assert.strictEqual((parseErr as { line: string }).line, 'not json');
        await transport.stop();
    });

    test('request timeout rejects', async () => {
        const input = new MemoryInput();
        const output = new MemoryOutput();
        const transport = new AcpTransport({
            binaryPath: 'fake',
            args: [],
        }).withInjectedIO(input, output);
        await transport.start();

        // 不发送响应，等待超时
        await assert.rejects(
            transport.request('initialize', {}, 100),
            /timed out after 100ms/,
        );
        await transport.stop();
    });

    test('notify writes notification without id', async () => {
        const input = new MemoryInput();
        const output = new MemoryOutput();
        const transport = new AcpTransport({
            binaryPath: 'fake',
            args: [],
        }).withInjectedIO(input, output);
        await transport.start();

        transport.notify('session/cancel', { sessionId: 's1' });
        await new Promise((r) => setImmediate(r));

        const sent = JSON.parse(output.lineAt(0) as string) as {
            method: string;
            params: unknown;
            id?: number;
        };
        assert.strictEqual(sent.method, 'session/cancel');
        assert.deepStrictEqual(sent.params, { sessionId: 's1' });
        assert.strictEqual(sent.id, undefined);
        await transport.stop();
    });
});
