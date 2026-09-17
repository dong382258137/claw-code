/**
 * claw 回合协调器（隔离世界，preload 运行）
 *
 * 职责：
 * 1. 监听主进程 'claw-inject'：把提示灌入输入框发送，等网页 SSE FINISHED
 *    完成信号后读回整段回复，壳侧先对危险命令挡一道（确认/拒绝），再回传
 *    reply 给 claw 守护进程。
 * 2. 注入 ⚡claw 悬浮按钮：把输入框任务作为 user_run 提交给守护进程。
 * 3. 展示连接状态 / 回合结束(final)提示与命令历史。
 *
 * 完成判定的根因（对比旧浏览器扩展的 DOM 稳定 600ms）：
 * DeepSeek/Claude/OpenAI 的网络拦截器(provider/*.js 的 observeBody +
 * createExtractor)在 SSE 流中看到 FINISHED/message_stop 才派发
 * 'cuckoo-ai-response' 事件——按服务端权威信号判定，根治"截断误判"。
 */
const { sendToChat, findInputArea } = require('./chat-input');
const { getProviderByUrl } = require('../../providers');
const { isDangerous } = require('../../main/dangerous-commands');
const {
  showConfirmDialog,
  showToast,
  addHistory,
  flashBadge,
  setTaskStatus,
} = require('../overlay/ui');
const state = require('./state');

// 等待网页回复的超时（ms）。守护进程 turn 超时默认 300s，留出回传余量。
const REPLY_TIMEOUT_MS = 270000;

/** 当前待回执的注入回合 */
let pending = null; // { turnId, timer }
/** ⚡claw 按钮引用（注入到页面右下角） */
let clawButton = null;

// ========== 代码块提取（markdown 围栏） ==========

const BASH_LANG = new Set(['bash', 'sh', 'shell']);
const RE_BASH_CALL = /\bawait\s+bash\s*\(\s*(['"])(.*?)\1\s*\)/;

/**
 * 提取全文中的围栏代码块。
 * @returns {Array<{lang:string, content:string, start:number, end:number}>}
 *   start/end 为行号（含 ``` 围栏行）。支持多行围栏解析。
 */
function extractCodeBlocks(text) {
  const lines = text.split('\n');
  const blocks = [];
  let inBlock = false;
  let lang = '';
  let buf = [];
  let start = 0;

  for (let i = 0; i < lines.length; i++) {
    const t = lines[i].trim();
    if (!inBlock) {
      if (t.startsWith('```')) {
        inBlock = true;
        lang = (t.slice(3).split(/\s/)[0] || '').toLowerCase();
        buf = [];
        start = i;
      }
    } else if (t.startsWith('```')) {
      blocks.push({ lang, content: buf.join('\n').trim(), start, end: i });
      inBlock = false;
    } else {
      buf.push(lines[i]);
    }
  }
  // 未闭合的围栏（截断或纯文本误判）：不当作命令块处理
  return blocks;
}

/** 块内是否命中危险命令（bash/sh/shell 块内容，或 claw 块里的 await bash("...")） */
function findDangerousInBlock(block) {
  if (BASH_LANG.has(block.lang)) {
    if (isDangerous(block.content)) {
      return [{ cmd: block.content.split('\n')[0].trim() }];
    }
    return [];
  }
  if (block.lang === 'claw') {
    const hits = [];
    for (const line of block.content.split('\n')) {
      const m = RE_BASH_CALL.exec(line);
      if (m && isDangerous(m[2])) hits.push({ cmd: m[2] });
    }
    return hits;
  }
  return [];
}

/** 重建文本：丢弃指定行号范围的代码块（整个块），其余原样保留 */
function applyRefusals(text, refusedStartLines) {
  const lines = text.split('\n');
  const drop = new Set(refusedStartLines);
  const out = [];
  let inBlock = false;
  let blockStart = -1;

  for (let i = 0; i < lines.length; i++) {
    const t = lines[i].trim();
    if (!inBlock) {
      if (t.startsWith('```')) {
        inBlock = true;
        blockStart = i;
      } else {
        out.push(lines[i]);
      }
    } else if (t.startsWith('```')) {
      if (!drop.has(blockStart)) {
        for (let j = blockStart; j <= i; j++) out.push(lines[j]);
      }
      inBlock = false;
    }
  }
  if (inBlock && !drop.has(blockStart)) {
    for (let j = blockStart; j < lines.length; j++) out.push(lines[j]);
  }
  return out.join('\n');
}

// ========== 灌入 + 完成等待 + 危险命令确认 ==========

/** 主进程路由来的 inject_prompt：灌入网页并等待 SSE FINISHED 后回传 reply */
async function handleInjectFromMain({ turnId, text }) {
  if (pending) {
    await replyError(turnId, '[shell] 上一轮注入尚未完成，忽略本次注入');
    return;
  }

  const replyPromise = new Promise((resolve) => {
    pending = { turnId, timer: null, resolve };
  });
  pending.timer = setTimeout(() => {
    const p = pending;
    pending = null;
    showToast('等待网页回复超时（270s）', 4000);
    p.resolve('[shell] 等待网页回复超时(270s)');
  }, REPLY_TIMEOUT_MS);

  const ok = sendToChat(text, 'claw注入');
  if (!ok) {
    const p = pending;
    pending = null;
    clearTimeout(p.timer);
    await replyError(turnId, '[shell] 找不到输入框/发送失败（请进入聊天界面）');
    return;
  }
  showToast('claw 正在注入并等待回复…', 1500);

  const finalText = await replyPromise;
  try {
    const res = await window.electronAPI.clawReply(turnId, finalText);
    if (!res || !res.ok) showToast('已获得回复，但回传失败：claw 守护可能已断开', 3500);
  } catch (e) {
    console.error('[Claw] 回传 reply 失败:', e && e.message);
  }
}

function replyError(turnId, text) {
  return window.electronAPI.clawReply(turnId, text).catch(() => {});
}

/** 收到一条 SSE FINISHED 的完整回复：确认危险命令后产出回传文本 */
async function finishReplyForPending(rawText) {
  const p = pending;
  if (!p) return;
  pending = null;
  clearTimeout(p.timer);

  let text = (rawText || '').trim();
  // 空回复兜底：直接回传空串让守护端判定失败，避免挂起
  if (!text) {
    showToast('网页回复为空', 3000);
    p.resolve('[shell] 网页返回了空回复');
    return;
  }

  const blocks = extractCodeBlocks(text);
  if (blocks.length > 0) {
    // 全部命令块先入历史（历史展示）
    for (const b of blocks) {
      if (BASH_LANG.has(b.lang) || b.lang === 'claw') {
        addHistory({
          id: 'cmd_' + Date.now() + '_' + Math.random().toString(36).slice(2, 7),
          command: previewBlock(b, blocks.length),
          success: true,
          canceled: false,
          output: '',
          timestamp: Date.now(),
        });
      }
    }

    // 危险命令确认：壳侧先挡一道（Rust safe_commands 再挡第二道）
    const dangerous = [];
    for (const b of blocks) {
      const hits = findDangerousInBlock(b);
      if (hits.length > 0) dangerous.push({ block: b, hits });
    }

    if (dangerous.length > 0) {
      const cmds = dangerous
        .map((d) => d.hits.map((h) => h.cmd).join('、'))
        .join('\n');
      const refused = !(await showConfirmDialog(
        '⚠️ 检测到危险命令：\n' + cmds + '\n\n确认交给本地 claw 执行？' +
        '\n（拒绝后将跳过这些命令并告知模型换更安全的方式）',
        { okText: '确认执行', showCancel: true, cancelText: '拒绝执行' }
      ));
      if (refused) {
        for (const d of dangerous) {
          for (const h of d.hits) {
            addHistory({
              id: 'cmd_' + Date.now() + '_' + Math.random().toString(36).slice(2, 7),
              command: h.cmd,
              success: false,
              canceled: true,
              output: '用户拒绝执行',
              timestamp: Date.now(),
            });
          }
        }
        const refusedStarts = dangerous.map((d) => d.block.start);
        text = applyRefusals(text, refusedStarts);
        const note =
          '\n\n[shell] 用户拒绝了 ' + cmds + ' 等危险命令，未执行；请改用更安全的方式完成。';
        text = text.replace(/\s+$/, '') + note;
        showToast('已拒绝危险命令并告知模型', 3000);
      } else {
        for (const d of dangerous) {
          for (const h of d.hits) {
            addHistory({
              id: 'cmd_' + Date.now() + '_' + Math.random().toString(36).slice(2, 7),
              command: h.cmd,
              success: true,
              canceled: false,
              output: '已放行给 claw 执行',
              timestamp: Date.now(),
            });
          }
        }
      }
    }
  }

  p.resolve(text);
}

function previewBlock(block, total) {
  const head = (block.content.split('\n')[0] || '').slice(0, 80);
  if (total === 1 && BASH_LANG.has(block.lang)) return head;
  return '[' + block.lang + '] ' + (head || '(空)');
}

// ========== ⚡claw 悬浮按钮（user_run 入口） ==========

function showToastMin(text) {
  let el = document.getElementById('claw-shell-toast');
  if (!el) {
    el = document.createElement('div');
    el.id = 'claw-shell-toast';
    el.style.cssText =
      'position:fixed;right:16px;bottom:16px;z-index:999999;background:#111;color:#fff;' +
      'padding:8px 12px;border-radius:8px;font-size:13px;box-shadow:0 2px 8px rgba(0,0,0,.4);';
    document.body.appendChild(el);
  }
  el.textContent = text;
  clearTimeout(showToastMin._timer);
  showToastMin._timer = setTimeout(() => { if (el) el.remove(); }, 3500);
}

function ensureClawButton() {
  if (document.getElementById('claw-shell-run')) return;
  const btn = document.createElement('button');
  btn.id = 'claw-shell-run';
  btn.textContent = '⚡claw';
  btn.title = '把输入框内容交给本地 claw 智能体执行（可操作电脑文件）';
  btn.style.cssText =
    'position:fixed;right:18px;bottom:96px;z-index:999998;padding:6px 12px;' +
    'border:1px solid #444;border-radius:999px;background:#111;color:#fff;' +
    'font-size:13px;line-height:1.5;cursor:pointer;box-shadow:0 2px 10px rgba(0,0,0,.35);';
  btn.addEventListener('click', async () => {
    if (document.getElementById('claw-shell-run').dataset.disabled === '1') {
      showToastMin('claw 正在执行中，请等待完成');
      return;
    }
    const input = findInputArea();
    const text = input ? (input.value || input.textContent || '').trim() : '';
    if (!text) {
      showToastMin('先在输入框里写下任务，再点我执行');
      return;
    }
    btn.textContent = '⏳claw';
    btn.dataset.disabled = '1';
    try {
      const resp = await window.electronAPI.clawRun(text);
      showToastMin(
        resp && resp.ok
          ? '已交给本地 claw，完成后结果会回到对话里'
          : '发送失败：' + ((resp && resp.error) || '本地守护未连接？')
      );
    } catch (e) {
      showToastMin('发送失败：' + (e && e.message));
    } finally {
      btn.textContent = '⚡claw';
      delete btn.dataset.disabled;
    }
  });
  document.body.appendChild(btn);
  clawButton = btn;
}

// ========== 状态/回合结束 展示 ==========

function updateClawStatus(status) {
  const el = document.getElementById('cuckoo-claw-status');
  if (el) {
    el.textContent = (status && status.connected)
      ? '● claw 守护已连接'
      : '○ claw 守护未连接（请先启动 claw-web-bridge）';
    el.style.color = status && status.connected ? '#7cffb2' : '#ff7070';
  }
}

/** 更新覆盖层面板的「claw 回合执行中」忙闲指示 + ⚡claw 按钮禁用态 */
function setClawBusy(busy) {
  setTaskStatus(busy);
  if (clawButton) {
    if (busy) clawButton.dataset.disabled = '1';
    else delete clawButton.dataset.disabled;
  }
}

// ========== 启动 ==========

function reportDiag() {
  try {
    const input = findInputArea();
    const payload = JSON.stringify({
      stage: 'ok',
      url: window.location.pathname,
      provider: getProviderByUrl(window.location.href) ? getProviderByUrl(window.location.href).id : null,
      inputFound: !!input,
    });
    window.electronAPI.clawDiag(payload).catch(() => {});
  } catch (_) { /* ignore */ }
}

/**
 * 启动 claw 回合协调器：
 * - 注册 cuckoo-ai-response（SSE FINISHED）完成信号监听
 * - 注册主进程 claw-inject/claw-final/claw-status 消息
 * - 注入 ⚡claw 悬浮按钮
 */
function startClawTurn() {
  window.addEventListener('cuckoo-ai-response', (ev) => {
    const detail = ev && ev.detail;
    if (!detail || !detail.finished) return;
    if (detail.tokenUsage) state.serverTokenUsage = detail.tokenUsage;
    if (pending) {
      console.log('[Claw][注入] 收到 SSE FINISHED 回复，长度=' + (detail.text || '').length);
      setClawBusy(false);
      finishReplyForPending(detail.text).catch((err) => {
        console.error('[Claw][注入] 处理回复出错:', err);
        const p = pending;
        pending = null;
        if (p) { clearTimeout(p.timer); p.resolve('[shell] 处理回复出错: ' + (err && err.message)); }
      });
    }
  });

  window.electronAPI.onClawInject(({ turnId, text }) => {
    console.log('[Claw][注入] 收到 inject_prompt ' + turnId + ' (' + text.length + ' chars)');
    setClawBusy(true);
    handleInjectFromMain({ turnId, text }).catch((err) => {
      console.error('[Claw][注入] 注入出错:', err);
      setClawBusy(false);
      replyError(turnId, '[shell] 注入出错: ' + (err && err.message)).catch(() => {});
    });
  });

  window.electronAPI.onClawFinal(({ text }) => {
    setClawBusy(false);
    showToastMin('✅ claw 任务完成');
    try { window.electronAPI.showAiNotification().catch(() => {}); } catch (e) { /* ignore */ }
    if (text) {
      addHistory({
        id: 'cmd_' + Date.now() + '_final',
        command: '[claw 回合完成] ' + text.slice(0, 60),
        success: true,
        canceled: false,
        output: text,
        timestamp: Date.now(),
      });
    }
    flashBadge();
  });

  window.electronAPI.onClawStatus(updateClawStatus);
  // 初始状态同步 + 初始进程探活
  window.electronAPI.getClawStatus().then(updateClawStatus).catch(() => {});

  ensureClawButton();
  setTimeout(reportDiag, 2000);
}

module.exports = { startClawTurn };