// 插件市场共用文案与格式化：原因码/错误码要原样展示给用户，
// 不能只显示「不可用」，否则无法判断是缺 sha 还是路径不存在。
export const REASON_LABELS = {
  invalid_entry: '条目字段不完整',
  invalid_source: '来源形态无法识别',
  missing_integrity_anchor: '缺少 pinned sha/sha256，无法校验完整性',
  invalid_sha: '上游 sha 不是固定的 40 位 commit',
  missing_path: '插件或组件路径不存在',
  unreadable_path: '插件路径不可读',
  missing_plugin_manifest: '缺少插件清单且无法按内联声明安装',
}

export const ERROR_LABELS = {
  invalid_argument: '参数不合法',
  untrusted_source: '来源未受信任',
  not_found: '未找到',
  conflict: '操作冲突',
  not_installable: '条目不可安装',
  source_unavailable: '来源不可用',
  source_timeout: '来源超时',
  internal_error: '服务器错误',
}

export const SOURCE_STATUS_LABELS = {
  ok: '可用',
  cached: '缓存',
  not_loaded: '未加载',
  error: '异常',
  timeout: '超时',
  clone_failed: '拉取失败',
  git_not_found: '未找到 git',
}

export const SOURCE_TYPE_LABELS = {
  http: 'HTTP 索引',
  git: 'git 仓库',
  local: '本地目录',
}

// 后端时间戳是 unix 毫秒；缺失时返回空串，由调用方决定文案。
export function formatTime(value) {
  if (!value) return ''
  const date = new Date(value)
  if (Number.isNaN(date.getTime())) return ''
  return date.toLocaleString('zh-CN', { hour12: false })
}

export function errorText(error) {
  if (!error) return ''
  const label = ERROR_LABELS[error.code]
  const detail = error.detail || ''
  if (label) return `${label}（${error.code}）${detail ? `：${detail}` : ''}`
  return error.message || String(error)
}
