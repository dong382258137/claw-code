import React from 'react'
import { withToken } from '../api'

function reportClientError(message, stack) {
  try {
    fetch(withToken('/api/client-error'), {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ message: String(message || ''), stack: stack ? String(stack).slice(0, 8000) : null, url: location.href }),
      keepalive: true,
    }).catch(() => {})
  } catch (_) {}
}

export class ClientErrorBoundary extends React.Component {
  constructor(props) {
    super(props)
    this.state = { error: null }
  }

  static getDerivedStateFromError(error) {
    return { error }
  }

  componentDidCatch(error, info) {
    reportClientError(error?.message || error, `${error?.stack || ''}\n${info?.componentStack || ''}`)
  }

  render() {
    if (!this.state.error) return this.props.children
    return (
      <div className="crash">
        <h2>界面渲染出错</h2>
        <pre>{String(this.state.error?.message || this.state.error)}</pre>
        <p>错误已记录到工作区 <code>.claw/web-client-errors.log</code>，请把这个文件发给开发者。</p>
        <button onClick={() => { this.setState({ error: null }); location.reload() }}>重新加载</button>
      </div>
    )
  }
}

// 全局兜底：同步异常与未处理的 Promise 拒绝都回传服务端日志。
window.addEventListener('error', (event) => {
  reportClientError(event.message, event.error?.stack || `${event.filename}:${event.lineno}:${event.colno}`)
})
window.addEventListener('unhandledrejection', (event) => {
  reportClientError(event.reason?.message || event.reason, event.reason?.stack)
})
