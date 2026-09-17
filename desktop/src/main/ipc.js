/**
 * IPC 处理器注册（渲染进程 → 主进程）
 *
 * 裁剪说明：本地工具/JsRunner 执行链已整体移除（执行权在 claw 守护进程），
 * 这里只保留窗口/通知类 IPC + 转发到 claw-client 的桥接 IPC。
 */
const { ipcMain, Notification } = require('electron');

const windowState = require('./window');
const profileManager = require('./profile-manager');
const clawClient = require('./claw-client');

function registerIpcHandlers() {
  // ========== claw 守护桥接 ==========

  // ⚡claw 按钮：提交用户任务给守护进程（user_run）
  ipcMain.handle('claw-run', async (_event, { text }) => {
    if (!text || typeof text !== 'string' || !text.trim()) {
      return { ok: false, error: '任务为空' };
    }
    const ok = clawClient.sendUserRun(text.trim());
    return ok
      ? { ok: true }
      : { ok: false, error: 'claw 守护未连接（端口 18793 未就绪？）' };
  });

  // preload 注入/读回一轮后，把网页回复回传（reply）
  ipcMain.handle('claw-reply', async (_event, { turnId, text }) => {
    if (!turnId) return { ok: false, error: '缺少 turnId' };
    const ok = clawClient.sendReply(turnId, text || '');
    return ok ? { ok: true } : { ok: false, error: 'claw 守护已断开' };
  });

  // 自动诊断上报（diag，写入守护进程日志便于排查）
  ipcMain.handle('claw-diag', async (_event, { payload }) => {
    clawClient.sendDiag(String(payload || ''));
    return { ok: true };
  });

  // 当前连接状态快照（渲染进程初始化时同步一次）
  ipcMain.handle('claw-status-get', async () => {
    return { ...clawClient.status };
  });

  // ========== 窗口/通知类 ==========

  // AI 回复完成时：窗口已聚焦则不打扰；否则弹通知并让任务栏闪烁
  ipcMain.handle('show-ai-notification', async (event) => {
    try {
      const ctx = windowState.getContextByWebContents(event.sender);
      const win = ctx ? ctx.win : windowState.getMainWindow();

      if (win && !win.isDestroyed() && win.isFocused()) {
        return { success: true, skipped: true, reason: 'window-focused' };
      }

      if (win && !win.isDestroyed()) {
        let windowName = 'Claw Code';
        if (ctx && ctx.profileId) {
          const profile = profileManager.getProfileById(ctx.profileId);
          if (profile && profile.name) windowName = profile.name;
        }

        const notification = new Notification({
          title: windowName + ' - AI任务已完成',
          body: 'AI 已完成回复',
        });
        notification.show();

        win.flashFrame(true);
        win.once('focus', () => {
          if (!win.isDestroyed()) win.flashFrame(false);
        });
      }

      return { success: true };
    } catch (err) {
      return { success: false, error: err.message };
    }
  });

  // 站点原生发送：向聚焦输入框注入真实级 Enter（合成事件免疫的平台用）
  ipcMain.handle('chat-send-enter', async (event) => {
    const sender = event.sender;
    if (!sender || sender.isDestroyed()) return false;
    try {
      sender.sendInputEvent({ type: 'keyDown', keyCode: 'Return', key: 'Enter' });
      sender.sendInputEvent({ type: 'char', keyCode: 'Return', key: '\r' });
      sender.sendInputEvent({ type: 'keyUp', keyCode: 'Return', key: 'Enter' });
      return true;
    } catch (err) {
      console.error('[Claw Code] 原生 Enter 发送失败:', err.message);
      return false;
    }
  });
}

module.exports = { registerIpcHandlers };