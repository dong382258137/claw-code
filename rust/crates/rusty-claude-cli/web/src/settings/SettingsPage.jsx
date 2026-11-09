import React, { useState } from 'react'

export function SettingsPage({ settings, meta, onClose, onSave }) {
  const [theme, setTheme] = useState(settings.theme || 'dark')
  const [permission, setPermission] = useState(settings.permission || 'workspace-write')
  return (
    <div className="settings-page">
      <div className="settings-head">
        <button onClick={onClose}>‹ 返回</button>
        <span className="title">设置</span>
      </div>
      <div className="settings-body">
        <div className="section">
          <h3>外观</h3>
          <label className="row">
            <span>主题</span>
            <select value={theme} onChange={(event) => setTheme(event.target.value)}>
              <option value="dark">深色</option>
              <option value="light">浅色</option>
            </select>
          </label>
        </div>
        <div className="section">
          <h3>Agent</h3>
          <label className="row">
            <span>默认权限模式</span>
            <select value={permission} onChange={(event) => setPermission(event.target.value)}>
              <option value="read-only">只读（不允许写文件）</option>
              <option value="workspace-write">工作区可写</option>
              <option value="danger-full-access">完全访问（危险）</option>
            </select>
          </label>
          <div className="row"><span className="muted">越权工具在 Web 端默认拒绝，完全访问会直接放行</span></div>
        </div>
        <div className="section">
          <h3>关于</h3>
          <div className="row"><span>版本</span><span className="muted">{meta.version || '—'} {meta.git_sha ? `· ${meta.git_sha}` : ''}</span></div>
          <div className="row"><span>工作区</span><span className="muted">{meta.workspace || '—'}</span></div>
        </div>
        <button className="primary" onClick={() => onSave({ theme, permission })}>保存</button>
      </div>
    </div>
  )
}
