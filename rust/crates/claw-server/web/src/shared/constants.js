// 跨模块共享的静态常量。新增页面从这里取预设，避免各页面各存一份。

export const TABS = [
  { id: 'files', label: '文件' },
  { id: 'diff', label: '变更' },
  { id: 'tasks', label: '任务' },
  { id: 'attachments', label: '附件' },
  { id: 'terminal', label: '终端' },
]

export const QUICK_ACTIONS = [
  { cmd: '/files', label: '打开文件面板', run: 'files' },
  { cmd: '/diff', label: '查看工作区变更', run: 'diff' },
  { cmd: '/tasks', label: '查看任务与计划', run: 'tasks' },
  { cmd: '/terminal', label: '打开终端', run: 'terminal' },
  { cmd: '/attachments', label: '查看附件', run: 'attachments' },
  { cmd: '/settings', label: '打开设置', run: 'settings' },
]

export const MODEL_PRESETS = [
  { id: '', label: '默认模型', hint: '跟随工作区配置（当前 Trae）' },
  { id: 'trae/deepseek-v4.1-flash', label: 'Trae · DeepSeek-V4.1-Flash', badge: '0.06', hint: 'Trae 内置额度，推荐' },
  { id: 'deepseek-flash', label: 'DeepSeek-V4.1-Flash（官方 API）', hint: '官方 API，需要 DeepSeek 余额' },
]
export const MODEL_LOOKUP = Object.fromEntries(MODEL_PRESETS.filter((item) => item.id).map((item) => [item.id, item]))

export const PERMISSION_PRESETS = [
  { id: 'read-only', label: '只读', hint: '仅查看文件与只读命令', tone: 'muted' },
  { id: 'workspace-write', label: '工作区可写', hint: '可修改工作区，敏感操作需确认', tone: 'blue' },
  { id: 'danger-full-access', label: '完全访问', hint: '不再逐次确认，请谨慎使用', tone: 'orange' },
]
export const PERMISSION_LOOKUP = Object.fromEntries(PERMISSION_PRESETS.map((item) => [item.id, item]))

export const CAP_ICONS = {
  files: 'folder',
  diff: 'diff',
  tasks: 'check',
  attachments: 'paperclip',
  terminal: 'terminal',
  settings: 'settings',
}

export const VOICE_BAR_COUNT = 100
export const VOICE_FLAT_BARS = Array.from({ length: VOICE_BAR_COUNT }, () => 0.06)
