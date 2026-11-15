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
    // 后端错误体是 { code, message }；保留 code 供页面区分 untrusted_source、
    // not_installable 等场景，message 仍保留 HTTP 前缀以兼容既有调用方。
    let body = null
    try {
      body = text ? JSON.parse(text) : null
    } catch (_) {}
    const detail = body && typeof body.message === 'string' ? body.message : text
    const error = new Error(
      `HTTP ${response.status}${detail ? ' · ' + String(detail).slice(0, 200) : ''}`,
    )
    error.status = response.status
    error.code = body && body.code ? body.code : ''
    error.detail = detail || ''
    throw error
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
  `${location.protocol === 'https:' ? 'wss' : 'ws'}://${location.host}${path}${path.includes('?') ? '&' : '?'}t=${encodeURIComponent(token)}`

export const rawUrl = (path, workspace) =>
  withToken(withWorkspace('/api/workspace/raw?path=' + encodeURIComponent(path), workspace))
