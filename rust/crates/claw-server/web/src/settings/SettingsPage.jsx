import React, { useState } from 'react'

const NAV = [
  { id: 'settings-appearance', label: '外观' },
  { id: 'settings-agent', label: 'Agent' },
  { id: 'settings-about', label: '关于' },
]

export function SettingsPage({ settings, meta, onClose, onSave }) {
  const [theme, setTheme] = useState(settings.theme || 'dark')
  const [permission, setPermission] = useState(settings.permission || 'workspace-write')
  const [activeNav, setActiveNav] = useState('settings-appearance')
  const embedding = meta.embedding || {}
  const embeddingState = embedding.state || 'loading'
  const embeddingLabel =
    {
      ready: '可用',
      keyword: '关键词降级',
      loading: '初始化中',
      disabled: '未启用',
    }[embeddingState] || embeddingState
  const embeddingDetail = embedding.detail || '本地向量 + 全文混合检索'

  const jump = (id) => {
    setActiveNav(id)
    document.getElementById(id)?.scrollIntoView({ behavior: 'smooth', block: 'start' })
  }

  return (
    <div className="settings-page">
      <div className="settings-head">
        <button type="button" className="settings-back" onClick={onClose}>‹ 返回</button>
        <div className="settings-heading">
          <span className="settings-eyebrow">SYSTEM</span>
          <span className="title">设置</span>
        </div>
        <span className="settings-version">v{meta.version || '—'}{meta.git_sha ? ` · ${meta.git_sha.slice(0, 7)}` : ''}</span>
      </div>

      <div className="settings-body">
        <aside className="settings-nav">
          {NAV.map((item) => (
            <button
              key={item.id}
              type="button"
              className={activeNav === item.id ? 'active' : ''}
              onClick={() => jump(item.id)}
            >
              {item.label}
            </button>
          ))}
          <p className="settings-nav-note">设置仅保存在本机，随 claw serve 一起生效。</p>
        </aside>

        <div className="settings-content">
          <section className="settings-group" id="settings-appearance">
            <header className="settings-group-head">
              <h3>外观</h3>
              <p>界面主题与阅读密度。</p>
            </header>
            <div className="settings-row">
              <div className="settings-row-label">
                <b>主题</b>
                <small>切换后立即应用到整个 Web 界面</small>
              </div>
              <div className="theme-choice">
                <button
                  type="button"
                  className={theme === 'dark' ? 'theme-option active' : 'theme-option'}
                  onClick={() => setTheme('dark')}
                >
                  <span className="theme-swatch dark" aria-hidden="true" />
                  深空
                </button>
                <button
                  type="button"
                  className={theme === 'light' ? 'theme-option active' : 'theme-option'}
                  onClick={() => setTheme('light')}
                >
                  <span className="theme-swatch light" aria-hidden="true" />
                  冰蓝
                </button>
              </div>
            </div>
          </section>

          <section className="settings-group" id="settings-agent">
            <header className="settings-group-head">
              <h3>Agent</h3>
              <p>工具调用边界与默认权限。</p>
            </header>
            <div className="settings-row">
              <div className="settings-row-label">
                <b>默认权限模式</b>
                <small>越权工具在 Web 端默认拒绝，完全访问会直接放行</small>
              </div>
              <select value={permission} onChange={(event) => setPermission(event.target.value)}>
                <option value="read-only">只读（不允许写文件）</option>
                <option value="workspace-write">工作区可写</option>
                <option value="danger-full-access">完全访问（危险）</option>
              </select>
            </div>
          </section>

          <section className="settings-group" id="settings-about">
            <header className="settings-group-head">
              <h3>关于</h3>
              <p>当前构建与服务运行位置。</p>
            </header>
            <div className="settings-row">
              <div className="settings-row-label"><b>版本</b></div>
              <span className="settings-value">{meta.version || '—'} {meta.git_sha ? `· ${meta.git_sha}` : ''}</span>
            </div>
            <div className="settings-row">
              <div className="settings-row-label"><b>工作区</b></div>
              <span className="settings-value path">{meta.workspace || '—'}</span>
            </div>
            <div className="settings-row">
              <div className="settings-row-label">
                <b>语义检索</b>
                <small>{embeddingDetail}</small>
              </div>
              <span className={`settings-value${embeddingState === 'keyword' ? ' warn' : ''}`}>
                {embeddingLabel}
              </span>
            </div>
          </section>
        </div>
      </div>

      <div className="settings-savebar">
        <span>更改会保存在本机 Web 设置中</span>
        <button type="button" className="primary" onClick={() => onSave({ theme, permission })}>保存设置</button>
      </div>
    </div>
  )
}
