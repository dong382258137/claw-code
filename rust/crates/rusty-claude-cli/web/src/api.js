const params = new URLSearchParams(location.search)

export const token = params.get('t') || ''

export const withToken = (path) =>
  path + (path.includes('?') ? '&' : '?') + 't=' + encodeURIComponent(token)

/// 多工作区：给文件/附件等 API 附加 workspace 参数。
export const withWorkspace = (path, workspace) =>
  workspace
    ? path + (path.includes('?') ? '&' : '?') + 'workspace=' + encodeURIComponent(workspace)
    : path

export async function api(path, options = {}) {
  const response = await fetch(withToken(path), options)
  if (!response.ok) {
    const text = await response.text().catch(() => '')
    throw new Error(`HTTP ${response.status}${text ? ' · ' + text.slice(0, 200) : ''}`)
  }
  const type = response.headers.get('content-type') || ''
  return type.includes('application/json') ? response.json() : response.text()
}

export const postJson = (path, body) =>
  api(path, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(body),
  })

export const wsUrl = (path) =>
  `${location.protocol === 'https:' ? 'wss' : 'ws'}://${location.host}${path}?t=${encodeURIComponent(token)}`

export const rawUrl = (path, workspace) =>
  withToken(withWorkspace('/api/workspace/raw?path=' + encodeURIComponent(path), workspace))
