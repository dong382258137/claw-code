// ACP 端到端 smoke test：模拟 VS Code 扩展的完整链路
// 用法:node scripts/acp-smoke-test.mjs [--model <name>] [--prompt <text>]
//
// 覆盖:
// 1. spawn claw-plus-headless (stdio ACP server)
// 2. initialize 握手 (protocolVersion 协商)
// 3. session/new 创建会话
// 4. session/prompt 发起一轮对话 (验证 assistant 回复通知)
//
// 退出码: 0 = 握手+会话创建成功; 1 = 任一步失败

import { spawn } from 'node:child_process';
import * as readline from 'node:readline';

const args = process.argv.slice(2);
function argValue(name, def) {
    const i = args.indexOf(name);
    return i >= 0 && args[i + 1] ? args[i + 1] : def;
}
const MODEL = argValue('--model', 'deepseek-v4-flash');
const PROMPT_TEXT = argValue('--prompt', 'Reply with exactly: OK');

const binary = 'claw-plus-headless';
const child = spawn(binary, ['--model', MODEL, '--permission-mode', 'workspace-write'], {
    stdio: ['pipe', 'pipe', 'pipe'],
});

const rl = readline.createInterface({ input: child.stdout, crlfDelay: Infinity });
child.stderr.on('data', (d) => {
    const line = d.toString().trim();
    if (line) console.error(`[stderr] ${line.slice(0, 200)}`);
});

let nextId = 1;
const pending = new Map();
const notifications = [];

function request(method, params) {
    const id = nextId++;
    return new Promise((resolve, reject) => {
        const timer = setTimeout(() => {
            pending.delete(id);
            reject(new Error(`timeout waiting for ${method}`));
        }, 120_000);
        pending.set(id, { resolve, reject, timer, method });
        child.stdin.write(`${JSON.stringify({ jsonrpc: '2.0', method, params, id })}\n`);
    });
}

rl.on('line', (line) => {
    if (!line.trim()) return;
    let msg;
    try {
        msg = JSON.parse(line);
    } catch {
        console.error(`[parse-error] ${line}`);
        return;
    }
    const id = msg.id;
    if (id !== undefined && (msg.result !== undefined || msg.error !== undefined)) {
        const p = pending.get(id);
        if (p) {
            clearTimeout(p.timer);
            pending.delete(id);
            if (msg.error) {
                p.reject(new Error(`${p.method} error ${msg.error.code}: ${msg.error.message}`));
            } else {
                p.resolve(msg.result);
            }
        }
        return;
    }
    if (id === undefined && msg.method) {
        notifications.push(msg);
        return;
    }
    console.error(`[unknown-message] ${line}`);
});

child.on('exit', (code, signal) => {
    console.error(`[info] ${binary} exited: code=${code} signal=${signal}`);
    process.exitCode = 1;
});

function fail(reason) {
    console.error(`[FAIL] ${reason}`);
    child.kill('SIGTERM');
    process.exit(1);
}

async function main() {
    console.log(`[1/4] initialize (model=${MODEL})...`);
    const init = await request('initialize', { protocolVersion: 1 });
    console.log(`      protocolVersion=${init.protocolVersion} authMethods=${JSON.stringify(init.authMethods)}`);

    console.log('[2/4] session/new...');
    const ns = await request('session/new', { cwd: process.cwd(), mcpServers: [] });
    const sessionId = ns.sessionId;
    console.log(`      sessionId=${sessionId}`);

    console.log(`[3/4] session/prompt ("${PROMPT_TEXT}")...`);
    const pr = await request('session/prompt', {
        sessionId,
        prompt: [{ type: 'text', text: PROMPT_TEXT }],
    });
    console.log(`      stopReason=${pr.stopReason}`);

    console.log('[4/4] collecting notifications...');
    // notification 经 gateway mpsc 异步转发到 stdout，等待其 flush
    await new Promise((r) => setTimeout(r, 1500));
    for (const n of notifications) {
        console.error(`[notif] ${JSON.stringify(n).slice(0, 300)}`);
    }
    const chunks = notifications.filter(
        (n) => n.params?.sessionId === sessionId && n.params?.update?.sessionUpdate === 'agent_message_chunk',
    );
    const text = chunks
        .map((c) => c.params.update.content?.text ?? '')
        .join('')
        .trim();
    console.log(`      notifications=${notifications.length} assistant_text=${JSON.stringify(text.slice(0, 120))}`);

    child.kill('SIGTERM');
    console.log('[PASS] ACP smoke test: initialize + session/new + prompt OK');
    process.exit(0);
}

main().catch((err) => fail(err.message));
