const params = new URLSearchParams(location.search)

export const token = params.get('t') || ''

export const withToken = (path) =>
  path + (path.includes('?') ? '&' : '?') + 't=' + encodeURIComponent(token)

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

export const rawUrl = (path) => withToken('/api/workspace/raw?path=' + encodeURIComponent(path))
