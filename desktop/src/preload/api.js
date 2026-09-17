/**
 * 暴露给渲染进程的 API（contextBridge + window 兜底）
 *
 * 裁剪说明：本地命令/工具/JS 执行链已移除（执行权在 claw 守护进程），
 * 这里保留窗口/平台/通知类 API，并新增 claw 桥接 API。
 */
const { contextBridge, ipcRenderer } = require('electron');

// ========== 暴露给渲染进程的 API ==========
let electronAPI = {
  // ========== claw 守护桥接 ==========
  clawRun: (text) => {
    return ipcRenderer.invoke('claw-run', { text });
  },
  clawReply: (turnId, text) => {
    return ipcRenderer.invoke('claw-reply', { turnId, text });
  },
  clawDiag: (payload) => {
    return ipcRenderer.invoke('claw-diag', { payload });
  },
  getClawStatus: () => {
    return ipcRenderer.invoke('claw-status-get');
  },
  // 主进程推送：claw-status / claw-inject / claw-final
  onClawStatus: (cb) => {
    ipcRenderer.on('claw-status', (_e, status) => cb(status));
  },
  onClawInject: (cb) => {
    ipcRenderer.on('claw-inject', (_e, { turnId, text }) => cb({ turnId, text }));
  },
  onClawFinal: (cb) => {
    ipcRenderer.on('claw-final', (_e, { text }) => cb({ text }));
  },

  // ========== 窗口/平台类 ==========
  sendEnterToChat: () => {
    return ipcRenderer.invoke('chat-send-enter');
  },
  createProfileWindow: () => {
    return ipcRenderer.invoke('create-profile-window');
  },
  listProfiles: () => {
    return ipcRenderer.invoke('list-profiles');
  },
  openProfileWindow: (profileId) => {
    return ipcRenderer.invoke('open-profile-window', { profileId });
  },
  deleteProfileWindow: (profileId) => {
    return ipcRenderer.invoke('delete-profile', { profileId });
  },
  updateWindowName: (displayName) => {
    return ipcRenderer.invoke('update-window-name', { displayName });
  },
  showAiNotification: () => {
    return ipcRenderer.invoke('show-ai-notification');
  },
  listProviders: () => {
    return ipcRenderer.invoke('list-providers');
  },
  selectPlatform: (providerId) => {
    return ipcRenderer.invoke('select-platform', { providerId });
  },
  createProfileWindowWithProvider: (providerId) => {
    return ipcRenderer.invoke('create-profile-window', { providerId });
  },
};

try {
  contextBridge.exposeInMainWorld('electronAPI', electronAPI);
} catch (err) {
  console.error('[Claw Code] contextBridge.exposeInMainWorld 失败:', err);
}

// 无论 contextBridge 是否成功，都直接挂载到 window 作为备选
window.electronAPI = electronAPI;