import React, { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { api, postJson, token, withToken, wsUrl, rawUrl } from './api'
import { Markdown } from './markdown'

const ANSI_RE = /\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07]*\x07|\x1b[@-_]/g
const TABS = [
  { id: 'files', label: '文件' },
  { id: 'diff', label: '变更' },
  { id: 'tasks', label: '任务' },
  { id: 'attachments', label: '附件' },
  { id: 'terminal', label: '终端' },
]
const QUICK_ACTIONS = [
  { cmd: '/files', label: '打开文件面板', run: 'files' },
  { cmd: '/diff', label: '查看工作区变更', run: 'diff' },
  { cmd: '/tasks', label: '查看任务与计划', run: 'tasks' },
  { cmd: '/terminal', label: '打开终端', run: 'terminal' },
  { cmd: '/attachments', label: '查看附件', run: 'attachments' },
  { cmd: '/settings', label: '打开设置', run: 'settings' },
]

function useToasts() {
  const [toasts, setToasts] = useState([])
  const push = useCallback((text, kind = 'info') => {
    const id = `${Date.now()}-${Math.random()}`
    setToasts((list) => [...list, { id, text, kind }])
    setTimeout(() => setToasts((list) => list.filter((toast) => toast.id !== id)), 3600)
  }, [])
  return { toasts, push }
}

export default function App() {
  const [sessions, setSessions] = useState([])
  const [filter, setFilter] = useState('')
  const [current, setCurrent] = useState(null)
  const [messages, setMessages] = useState([])
  const [optimistic, setOptimistic] = useState(null)
  const [running, setRunning] = useState(false)
  const [live, setLive] = useState('')
  const [connected, setConnected] = useState(true)
  const [sidebarCollapsed, setSidebarCollapsed] = useState(false)
  const [panelOpen, setPanelOpen] = useState(true)
  const [panelTab, setPanelTab] = useState('files')
  const [dock, setDock] = useState('right')
  const [panelWidth, setPanelWidth] = useState(420)
  const [settingsOpen, setSettingsOpen] = useState(false)
  const [webSettings, setWebSettings] = useState({ theme: 'dark', permission: 'workspace-write' })
  const [meta, setMeta] = useState({ version: '', git_sha: '', workspace: '' })
  const { toasts, push } = useToasts()

  const currentId = current?.id || null

  /* ---------- 初始化：设置 / 元信息 / 会话列表 ---------- */
  useEffect(() => {
    api('/api/web-settings').then((settings) => {
      setWebSettings(settings)
      setSidebarCollapsed(Boolean(settings.sidebar_collapsed))
      setPanelOpen(settings.panel_open !== false)
      setPanelTab(settings.panel_tab || 'files')
      setDock(settings.panel_side === 'left' ? 'left' : 'right')
      setPanelWidth(clampWidth(settings.panel_width, settings.panel_side === 'left' ? 'left' : 'right', Boolean(settings.sidebar_collapsed)))
      document.documentElement.dataset.theme = settings.theme || 'dark'
    }).catch(() => {})
    api('/api/meta').then(setMeta).catch(() => {})
  }, [])

  const loadSessions = useCallback(() => {
    api('/api/sessions').then(setSessions).catch(() => {})
  }, [])
  useEffect(() => {
    loadSessions()
    const timer = setInterval(loadSessions, 10000)
    return () => clearInterval(timer)
  }, [loadSessions])

  /* ---------- 会话订阅 ---------- */
  useEffect(() => {
    if (!currentId) return undefined
    let closed = false
    setOptimistic(null)
    api(`/api/session/${encodeURIComponent(currentId)}`)
      .then((view) => { if (!closed) { setCurrent(view); setMessages(view.messages || []) } })
      .catch((error) => push(`读取会话失败：${error.message}`, 'error'))

    const source = new EventSource(withToken(`/api/session/${encodeURIComponent(currentId)}/stream`))
    source.addEventListener('snapshot', (event) => {
      if (closed) return
      try {
        const view = JSON.parse(event.data)
        setCurrent(view)
        setMessages(view.messages || [])
        const last = view.messages?.[view.messages.length - 1]
        if (last && last.role === 'assistant') setLive('')
        setOptimistic((pending) => {
          if (!pending) return null
          return last && last.role === 'user' ? null : pending
        })
      } catch (_) {}
    })
    source.onopen = () => setConnected(true)
    source.onerror = () => setConnected(false)

    // token 级流式事件（进程内 turn 的 StatusEmitter → SSE）。
    const events = new EventSource(withToken(`/api/session/${encodeURIComponent(currentId)}/events`))
    events.addEventListener('stream', (event) => {
      if (closed) return
      try {
        const data = JSON.parse(event.data)
        if (data.kind === 'delta') setLive((previous) => previous + data.text)
        else if (data.kind === 'start') setRunning(true)
        else if (data.kind === 'done') { setLive(''); setRunning(false) }
        else if (data.kind === 'error') push(`流式错误：${data.message}`, 'error')
      } catch (_) {}
    })
    return () => { closed = true; source.close(); events.close() }
  }, [currentId, push])

  useEffect(() => {
    if (!currentId) return undefined
    const timer = setInterval(() => {
      api(`/api/session/${encodeURIComponent(currentId)}/status`)
        .then((state) => setRunning(Boolean(state.running)))
        .catch(() => {})
    }, 1200)
    return () => clearInterval(timer)
  }, [currentId])

  /* ---------- 布局 ---------- */
  const saveSettings = useCallback(async (patch, { silent = true } = {}) => {
    setWebSettings((settings) => ({ ...settings, ...patch }))
    try {
      const saved = await postJson('/api/web-settings', patch)
      setWebSettings(saved)
      if (patch.theme) document.documentElement.dataset.theme = patch.theme
      if (!silent) push('设置已保存')
    } catch (error) {
      if (!silent) push(`保存失败：${error.message}`, 'error')
    }
  }, [push])

  const toggleSidebar = useCallback(() => {
    setSidebarCollapsed((value) => {
      const next = !value
      saveSettings({ sidebar_collapsed: next })
      setPanelWidth((width) => clampWidth(width, dock, next))
      return next
    })
  }, [dock, saveSettings])

  const openPanel = useCallback((tab) => {
    setPanelOpen(true)
    if (tab) setPanelTab(tab)
    saveSettings({ panel_open: true, ...(tab ? { panel_tab: tab } : {}) })
  }, [saveSettings])

  const closePanel = useCallback(() => {
    setPanelOpen(false)
    saveSettings({ panel_open: false })
  }, [saveSettings])

  const switchTab = useCallback((tab) => {
    setPanelTab(tab)
    saveSettings({ panel_tab: tab })
  }, [saveSettings])

  const switchDock = useCallback(() => {
    const next = dock === 'left' ? 'right' : 'left'
    setDock(next)
    setPanelOpen(true)
    setPanelWidth((width) => clampWidth(width, next, sidebarCollapsed))
    saveSettings({ panel_side: next, panel_open: true })
  }, [dock, saveSettings, sidebarCollapsed])

  const startResize = useCallback((event) => {
    event.preventDefault()
    const onMove = (moveEvent) => {
      const sidebar = sidebarCollapsed ? 56 : 280
      const width = dock === 'left'
        ? clampWidth(moveEvent.clientX - sidebar - 4, 'left', sidebarCollapsed)
        : clampWidth(window.innerWidth - moveEvent.clientX, 'right', sidebarCollapsed)
      setPanelWidth(width)
    }
    const onUp = () => {
      document.removeEventListener('mousemove', onMove)
      document.removeEventListener('mouseup', onUp)
      document.body.style.cursor = ''
      setPanelWidth((width) => { saveSettings({ panel_width: width }); return width })
    }
    document.body.style.cursor = 'col-resize'
    document.addEventListener('mousemove', onMove)
    document.addEventListener('mouseup', onUp)
  }, [dock, saveSettings, sidebarCollapsed])

  /* ---------- 快捷键 ---------- */
  useEffect(() => {
    const onKey = (event) => {
      if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === 'k') {
        event.preventDefault()
        document.getElementById('session-search')?.focus()
      }
      if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === 'b') {
        event.preventDefault()
        toggleSidebar()
      }
      if (event.key === 'Escape' && settingsOpen) setSettingsOpen(false)
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [settingsOpen, toggleSidebar])

  /* ---------- 动作 ---------- */
  const selectSession = useCallback((id) => setCurrent((view) => ({ ...(view || {}), id })), [])

  const send = useCallback(async (text) => {
    if (!currentId) { push('先选择一个会话', 'error'); return }
    const trimmed = text.trim()
    if (!trimmed) return
    setOptimistic({ text: trimmed, at: Date.now() })
    setRunning(true)
    try {
      await postJson(`/api/session/${encodeURIComponent(currentId)}/prompt`, {
        text: trimmed,
        permission: webSettings.permission || 'workspace-write',
      })
    } catch (error) {
      setOptimistic(null)
      setRunning(false)
      push(`发送失败：${error.message}`, 'error')
    }
  }, [currentId, push, webSettings.permission])

  const stop = useCallback(async () => {
    if (!currentId) return
    try {
      await api(`/api/session/${encodeURIComponent(currentId)}/interrupt`, { method: 'POST' })
      setRunning(false)
      push('已请求中断')
    } catch (error) {
      push(`中断失败：${error.message}`, 'error')
    }
  }, [currentId, push])

  const runQuickAction = useCallback((action) => {
    if (action === 'settings') setSettingsOpen(true)
    else openPanel(action)
  }, [openPanel])

  const sidebar = (
    <Sidebar
      sessions={sessions}
      filter={filter}
      onFilter={setFilter}
      currentId={currentId}
      collapsed={sidebarCollapsed}
      onToggle={toggleSidebar}
      onSelect={(id) => { selectSession(id); setOptimistic(null) }}
      onSettings={() => setSettingsOpen(true)}
      onTheme={() => saveSettings({ theme: webSettings.theme === 'dark' ? 'light' : 'dark' })}
      theme={webSettings.theme}
    />
  )

  const main = (
    <main>
      <TopBar
        title={current?.id || '未选择会话'}
        chips={[
          current?.model,
          current?.workspace ? basename(current.workspace) : null,
          current ? `${current.message_count ?? messages.length} 条` : null,
          running ? '运行中' : null,
        ].filter(Boolean)}
        connected={connected}
        running={running}
        onSettings={() => setSettingsOpen(true)}
        onTogglePanel={() => (panelOpen ? closePanel() : openPanel())}
        panelOpen={panelOpen}
      />
      {!connected && <div className="banner">连接中断，正在重连…</div>}
      <Conversation messages={messages} optimistic={optimistic} live={live} />
      <Composer
        running={running}
        onSend={send}
        onStop={stop}
        onQuickAction={runQuickAction}
        onError={(message) => push(message, 'error')}
        currentId={currentId}
      />
      {settingsOpen && (
        <SettingsPage
          settings={webSettings}
          meta={meta}
          onClose={() => setSettingsOpen(false)}
          onSave={async (patch) => { await saveSettings(patch, { silent: false }); setSettingsOpen(false) }}
        />
      )}
    </main>
  )

  const panel = panelOpen && (
    <Panel
      tab={panelTab}
      onTab={switchTab}
      onClose={closePanel}
      onDock={switchDock}
      width={panelWidth}
      currentId={currentId}
      meta={meta}
      onError={(message) => push(message, 'error')}
      onToast={push}
    />
  )

  return (
    <div className={`app ${dock === 'left' ? 'dock-left' : ''}`}>
      {sidebar}
      {dock === 'left' && panelOpen && <div className="splitter" onMouseDown={startResize} />}
      {dock === 'left' && panel}
      {main}
      {dock === 'right' && panelOpen && <div className="splitter" onMouseDown={startResize} />}
      {dock === 'right' && panel}
      <Toasts toasts={toasts} />
    </div>
  )
}

/* ---------------- 基础组件 ---------------- */

function clampWidth(desired, dock, collapsed) {
  const sidebar = collapsed ? 56 : 280
  const available = window.innerWidth - sidebar - 400 - 4
  const viewportCap = Math.floor(window.innerWidth * 0.7)
  const upper = Math.max(300, Math.min(viewportCap, available))
  return Math.max(300, Math.min(Math.round(desired || window.innerWidth * 0.45), upper))
}

function basename(path) {
  return String(path).split(/[\\/]/).filter(Boolean).pop() || ''
}

function relTime(seconds) {
  if (!seconds) return ''
  const diff = Math.max(0, Date.now() / 1000 - seconds)
  if (diff < 60) return '刚刚'
  if (diff < 3600) return `${Math.floor(diff / 60)} 分钟前`
  if (diff < 86400) return `${Math.floor(diff / 3600)} 小时前`
  return `${Math.floor(diff / 86400)} 天前`
}

function fmtTime(ms) {
  if (!ms) return ''
  const date = new Date(ms)
  const pad = (value) => String(value).padStart(2, '0')
  return `${date.getMonth() + 1}-${pad(date.getDate())} ${pad(date.getHours())}:${pad(date.getMinutes())}`
}

function Toasts({ toasts }) {
  return (
    <div className="toasts">
      {toasts.map((toast) => (
        <div key={toast.id} className={`toast ${toast.kind}`}>{toast.text}</div>
      ))}
    </div>
  )
}

function Sidebar({ sessions, filter, onFilter, currentId, collapsed, onToggle, onSelect, onSettings, onTheme, theme }) {
  const grouped = useMemo(() => {
    const query = filter.trim().toLowerCase()
    const filtered = sessions.filter((session) =>
      !query ||
      (session.title || '').toLowerCase().includes(query) ||
      session.id.toLowerCase().includes(query) ||
      (session.workspace || '').toLowerCase().includes(query))
    const map = new Map()
    for (const session of filtered) {
      const key = basename(session.workspace) || '未知工作区'
      if (!map.has(key)) map.set(key, [])
      map.get(key).push(session)
    }
    return [...map.entries()]
  }, [sessions, filter])

  return (
    <aside className={collapsed ? 'collapsed' : ''}>
      <div className="brand">
        <span><b>CLAW</b></span>
        {!collapsed && (
          <div className="brand-actions">
            <button onClick={onTheme} title="切换主题">{theme === 'dark' ? '◐' : '◑'}</button>
            <button onClick={onSettings} title="设置">⚙</button>
            <button onClick={onToggle} title="收起侧栏 (Ctrl+B)">‹</button>
          </div>
        )}
      </div>
      {collapsed && (
        <div className="rail">
          <button onClick={onToggle} title="展开侧栏 (Ctrl+B)">☰</button>
          <button onClick={onSettings} title="设置">⚙</button>
          <button onClick={onTheme} title="切换主题">◐</button>
        </div>
      )}
      {!collapsed && (
        <>
          <div className="search">
            <input id="session-search" value={filter} onChange={(event) => onFilter(event.target.value)} placeholder="搜索会话 / Ctrl+K" />
          </div>
          <div className="session-list">
            {grouped.length === 0 && <div className="empty">没有匹配的会话</div>}
            {grouped.map(([workspace, items]) => (
              <React.Fragment key={workspace}>
                <div className="group">{workspace} · {items.length}</div>
                {items.map((session) => (
                  <button
                    key={session.id}
                    className={`session ${session.id === currentId ? 'active' : ''}`}
                    onClick={() => onSelect(session.id)}
                    title={session.workspace || ''}
                  >
                    <span className="title">{session.title || session.id}</span>
                    <span className="meta">{relTime(session.modified)}</span>
                  </button>
                ))}
              </React.Fragment>
            ))}
          </div>
        </>
      )}
    </aside>
  )
}

function TopBar({ title, chips, connected, running, onSettings, onTogglePanel, panelOpen }) {
  return (
    <div className="topbar">
      <span className="title">{title}</span>
      <div className="chips">
        {chips.map((chip, index) => <span key={index} className="chip">{chip}</span>)}
        {!connected && <span className="chip warn">离线</span>}
        {running && <span className="chip running">运行中</span>}
      </div>
      <button onClick={onTogglePanel}>{panelOpen ? '隐藏面板' : '显示面板'}</button>
    </div>
  )
}

function Conversation({ messages, optimistic, live }) {
  const ref = useRef(null)
  const stick = useRef(true)
  useEffect(() => {
    const node = ref.current
    if (!node) return
    if (stick.current) node.scrollTop = node.scrollHeight
  }, [messages, optimistic, live])
  const onScroll = () => {
    const node = ref.current
    if (!node) return
    stick.current = node.scrollHeight - node.scrollTop - node.clientHeight < 48
  }
  const visible = useMemo(
    () => (messages || []).filter((message) => (message.blocks || []).length > 0),
    [messages],
  )
  return (
    <div className="messages" ref={ref} onScroll={onScroll}>
      {visible.length === 0 && !optimistic && !live && <div className="empty">选择一个会话，或直接在下方输入。</div>}
      {visible.map((message, index) => <Message key={index} message={message} />)}
      {optimistic && (
        <div className="msg user optimistic" key="optimistic">
          <div className="msg-head"><span className="who">user</span><span className="pending">发送中…</span></div>
          <div className="msg-body"><Markdown text={optimistic.text} /></div>
        </div>
      )}
      {live && (
        <div className="msg assistant live" key="live">
          <div className="msg-head"><span className="who">assistant</span><span className="pending">流式输出…</span></div>
          <div className="msg-body">
            <Markdown text={live} />
            <span className="cursor" />
          </div>
        </div>
      )}
    </div>
  )
}

function Message({ message }) {
  const [copied, setCopied] = useState(false)
  const text = (message.blocks || []).filter((block) => block.kind === 'text').map((block) => block.text).join('\n')
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(text)
      setCopied(true)
      setTimeout(() => setCopied(false), 1200)
    } catch (_) {}
  }
  return (
    <div className={`msg ${message.role}`}>
      <div className="msg-head">
        <span className="who">{message.role}</span>
        {text && <button className="ghost" onClick={copy}>{copied ? '已复制' : '复制'}</button>}
      </div>
      <div className="msg-body">
        {(message.blocks || []).map((block, index) => <Block key={index} block={block} />)}
      </div>
    </div>
  )
}

function Block({ block }) {
  if (block.kind === 'thinking') {
    return (
      <details className="thinking">
        <summary>思考过程</summary>
        <pre>{block.text || ''}</pre>
      </details>
    )
  }
  if (block.kind === 'tool_use') {
    return (
      <details className="tool" open={(block.input || '').length <= 240}>
        <summary><span className="tool-name">▶ {block.name}</span></summary>
        <pre>{block.input || ''}</pre>
      </details>
    )
  }
  if (block.kind === 'tool_result') {
    return (
      <details className={`tool ${block.is_error ? 'error' : ''}`} open={(block.output || '').length <= 600}>
        <summary><span className="tool-name">{block.is_error ? '✗' : '✓'} {block.name}</span></summary>
        <pre>{block.output || ''}</pre>
      </details>
    )
  }
  return <Markdown text={block.text || ''} />
}

function Composer({ running, onSend, onStop, onQuickAction, onError, currentId }) {
  const [value, setValue] = useState('')
  const [files, setFiles] = useState([])
  const [menu, setMenu] = useState(null)
  const [cursor, setCursor] = useState(0)
  const ref = useRef(null)
  const areaRef = useRef(null)

  useEffect(() => { ref.current = value }, [value])
  useEffect(() => {
    if (menu?.kind !== 'file' || files.length) return
    api('/api/workspace/files').then((body) => setFiles(body.files || [])).catch(() => {})
  }, [menu, files.length])

  const slashMatches = useMemo(() => {
    if (!value.startsWith('/') || value.includes(' ')) return []
    return QUICK_ACTIONS.filter((action) => action.cmd.startsWith(value))
  }, [value])

  const fileFragment = useMemo(() => {
    const match = /(^|\s)@([^\s]*)$/.exec(value)
    return match ? match[2] : null
  }, [value])

  const fileMatches = useMemo(() => {
    if (fileFragment === null) return []
    const query = fileFragment.toLowerCase()
    return files.filter((file) => file.path.toLowerCase().includes(query)).slice(0, 8)
  }, [fileFragment, files])

  useEffect(() => {
    if (slashMatches.length) setMenu({ kind: 'slash', items: slashMatches })
    else if (fileMatches.length) setMenu({ kind: 'file', items: fileMatches })
    else setMenu(null)
    setCursor(0)
  }, [slashMatches, fileMatches])

  const choose = (item) => {
    if (menu?.kind === 'slash') {
      onQuickAction(item.run)
      setValue('')
    } else if (menu?.kind === 'file') {
      const replaced = value.replace(/@([^\s]*)$/, '@' + item.path)
      setValue(replaced + ' ')
    }
    setMenu(null)
    areaRef.current?.focus()
  }

  const submit = () => {
    if (running) return
    const text = value
    if (!text.trim()) return
    setValue('')
    onSend(text)
  }

  const onKeyDown = (event) => {
    if (menu) {
      if (event.key === 'ArrowDown') { event.preventDefault(); setCursor((index) => (index + 1) % menu.items.length); return }
      if (event.key === 'ArrowUp') { event.preventDefault(); setCursor((index) => (index - 1 + menu.items.length) % menu.items.length); return }
      if (event.key === 'Tab' || (event.key === 'Enter' && !event.ctrlKey)) { event.preventDefault(); choose(menu.items[cursor]); return }
      if (event.key === 'Escape') { setMenu(null); return }
    }
    if (event.key === 'Enter' && !event.shiftKey && !event.ctrlKey) {
      event.preventDefault()
      submit()
    }
  }

  const grow = (event) => {
    const node = event.target
    node.style.height = 'auto'
    node.style.height = `${Math.min(180, node.scrollHeight)}px`
  }

  return (
    <form className="composer" onSubmit={(event) => { event.preventDefault(); submit() }}>
      {menu && (
        <div className="menu">
          {menu.kind === 'slash'
            ? menu.items.map((item, index) => (
                <button type="button" key={item.cmd} className={index === cursor ? 'active' : ''} onMouseDown={(event) => { event.preventDefault(); choose(item) }}>
                  <b>{item.cmd}</b><span>{item.label}</span>
                </button>
              ))
            : menu.items.map((item, index) => (
                <button type="button" key={item.path} className={index === cursor ? 'active' : ''} onMouseDown={(event) => { event.preventDefault(); choose(item) }}>
                  <b>{item.path}</b><span>{Math.max(1, Math.round((item.size || 0) / 1024))}K</span>
                </button>
              ))}
        </div>
      )}
      <textarea
        ref={areaRef}
        value={value}
        rows={2}
        placeholder={currentId ? '输入消息；Enter 发送，Shift+Enter 换行，/ 快捷操作，@ 引用文件' : '先选择一个会话'}
        onChange={(event) => { setValue(event.target.value); grow(event) }}
        onKeyDown={onKeyDown}
      />
      <div className="composer-actions">
        <select value="workspace-write" disabled title="权限模式在设置里调整">
          <option>workspace-write</option>
        </select>
        {running
          ? <button type="button" className="danger" onClick={onStop}>停止</button>
          : <button type="submit" className="primary">发送</button>}
      </div>
      <div className="composer-hint">
        <span>/ 快捷操作 · @ 引用文件 · Ctrl+B 侧栏</span>
        <span>{value.length} 字符</span>
      </div>
    </form>
  )
}

/* ---------------- 右侧面板 ---------------- */

function Panel({ tab, onTab, onClose, onDock, width, currentId, meta, onError, onToast }) {
  return (
    <section className="panel" style={{ width }}>
      <div className="panel-head">
        {TABS.map((item) => (
          <button key={item.id} className={tab === item.id ? 'tab active' : 'tab'} onClick={() => onTab(item.id)}>{item.label}</button>
        ))}
        <span className="spacer" />
        <button onClick={onDock} title="切换停靠侧">⇄</button>
        <button onClick={onClose} title="收起">×</button>
      </div>
      <div className="panel-body">
        {tab === 'files' && <FilesPane onError={onError} />}
        {tab === 'diff' && <DiffPane onError={onError} />}
        {tab === 'tasks' && <TasksPane currentId={currentId} onError={onError} />}
        {tab === 'attachments' && <AttachmentsPane onError={onError} />}
        {tab === 'terminal' && <TerminalPane />}
      </div>
    </section>
  )
}

function FilesPane({ onError }) {
  const [files, setFiles] = useState([])
  const [filter, setFilter] = useState('')
  const [preview, setPreview] = useState(null)
  const [editing, setEditing] = useState(false)
  const [draft, setDraft] = useState('')
  const [loading, setLoading] = useState(false)

  const load = useCallback(() => {
    setLoading(true)
    api('/api/workspace/files').then((body) => setFiles(body.files || [])).catch((error) => onError?.(`文件列表失败：${error.message}`)).finally(() => setLoading(false))
  }, [onError])

  useEffect(() => { load() }, [load])

  const open = useCallback(async (path) => {
    try {
      const file = await api(`/api/workspace/file?path=${encodeURIComponent(path)}`)
      setPreview(file)
      setEditing(false)
      setDraft(file.content || '')
    } catch (error) {
      onError?.(`预览失败：${error.message}`)
    }
  }, [onError])

  const save = async () => {
    if (!preview) return
    try {
      const result = await postJson('/api/workspace/file', { path: preview.path, content: draft, expected_mtime_ms: preview.mtime_ms || null })
      setPreview({ ...preview, content: draft, mtime_ms: result.mtime_ms })
      setEditing(false)
      onError?.(null)
    } catch (error) {
      onError?.(`保存失败：${error.message}`)
    }
  }

  if (preview) {
    return (
      <div className="pane">
        <div className="pane-toolbar">
          <button onClick={() => setPreview(null)}>‹ 文件</button>
          <span className="path" title={preview.path}>{preview.path}</span>
          <span className="spacer" />
          {!editing && (preview.kind === 'text' || preview.kind === 'markdown') && <button onClick={() => setEditing(true)}>编辑</button>}
          {editing && <button onClick={save}>保存</button>}
          {editing && <button onClick={() => { setEditing(false); setDraft(preview.content || '') }}>取消</button>}
        </div>
        <div className="pane-body">
          {editing
            ? <textarea className="editor" value={draft} onChange={(event) => setDraft(event.target.value)} />
            : preview.kind === 'image'
              ? <img className="preview-image" src={rawUrl(preview.path)} alt={preview.path} />
              : preview.kind === 'pdf'
                ? <iframe className="preview-frame" src={rawUrl(preview.path)} title={preview.path} />
                : preview.kind === 'binary'
                  ? <div className="empty">二进制文件（{Math.max(1, Math.round((preview.size || 0) / 1024))} KB）</div>
                  : preview.kind === 'markdown'
                    ? <Markdown text={preview.content || ''} />
                    : <pre className="file-text">{preview.content || ''}</pre>}
        </div>
      </div>
    )
  }

  const filtered = files.filter((file) => !filter || file.path.toLowerCase().includes(filter.toLowerCase()))
  return (
    <div className="pane">
      <div className="pane-toolbar">
        <input value={filter} onChange={(event) => setFilter(event.target.value)} placeholder="过滤文件…" />
        <button onClick={load}>{loading ? '…' : '刷新'}</button>
      </div>
      <div className="pane-body scroll">
        {filtered.slice(0, 1500).map((file) => (
          <button key={file.path} className="file" onClick={() => open(file.path)} title={file.path}>
            <span>{file.path}</span>
            <span className="size">{file.size ? Math.max(1, Math.round(file.size / 1024)) + 'K' : ''}</span>
          </button>
        ))}
        {filtered.length === 0 && <div className="empty">没有匹配的文件</div>}
      </div>
    </div>
  )
}

function DiffPane({ onError }) {
  const [diff, setDiff] = useState({ files: [], untracked: [], patch: '' })
  const [selected, setSelected] = useState('')
  const load = useCallback(() => {
    api('/api/workspace/diff').then(setDiff).catch((error) => onError?.(`diff 失败：${error.message}`))
  }, [onError])
  useEffect(() => { load() }, [load])
  const openFile = async (path) => {
    try {
      const single = await api(`/api/workspace/diff?path=${encodeURIComponent(path)}`)
      setSelected(path)
      setDiff((current) => ({ ...current, patch: single.patch || '' }))
    } catch (error) {
      onError?.(`diff 失败：${error.message}`)
    }
  }
  return (
    <div className="pane">
      <div className="pane-toolbar">
        <button onClick={load}>刷新</button>
        <span className="hint">{diff.files.length} 个已修改 · {diff.untracked.length} 个未跟踪</span>
      </div>
      <div className="pane-body scroll">
        {diff.files.map((file) => (
          <button key={file.path} className="diff-row" onClick={() => openFile(file.path)}>
            <span>{file.path}</span>
            <span><span className="add">+{file.added}</span> <span className="del">-{file.removed}</span></span>
          </button>
        ))}
        {diff.untracked.length > 0 && <div className="group">未跟踪</div>}
        {diff.untracked.map((path) => (
          <button key={path} className="diff-row" onClick={() => openFile(path)}><span>{path}</span><span className="muted">new</span></button>
        ))}
        <pre className="patch">{colorizePatch(diff.patch)}</pre>
        {selected && <div className="hint">当前查看：{selected}</div>}
      </div>
    </div>
  )
}

function colorizePatch(patch) {
  return String(patch || '').split('\n').map((line, index) => {
    let cls = ''
    if (line.startsWith('@@')) cls = 'l-hunk'
    else if (line.startsWith('+++') || line.startsWith('---') || line.startsWith('diff ')) cls = 'l-meta'
    else if (line.startsWith('+')) cls = 'l-add'
    else if (line.startsWith('-')) cls = 'l-del'
    return <span key={index} className={cls}>{line + '\n'}</span>
  })
}

function TasksPane({ currentId, onError }) {
  const [data, setData] = useState({ todos: [], tasks: [], crons: [], plan_mode: null })
  const [goal, setGoal] = useState(null)
  const [plans, setPlans] = useState([])
  const load = useCallback(() => {
    if (!currentId) return
    Promise.all([
      api(`/api/session/${encodeURIComponent(currentId)}/tasks`).catch(() => ({ todos: [], tasks: [], crons: [] })),
      api('/api/workspace/goal').catch(() => ({ goal: null })),
      api('/api/workspace/plans').catch(() => ({ plans: [] })),
    ]).then(([tasks, goalBody, planBody]) => {
      setData(tasks)
      setGoal(goalBody.goal)
      setPlans(planBody.plans || [])
    }).catch((error) => onError?.(`任务加载失败：${error.message}`))
  }, [currentId, onError])
  useEffect(() => { load() }, [load])
  if (!currentId) return <div className="empty">先选择会话</div>
  const goalHtml = goal && (goal.text || goal.state) && (
    <div className="goal">
      <b>目标</b> {goal.text}
      <div className="muted">状态 {goal.state || '—'} · token {goal.tokens_used || 0}{goal.token_budget ? ` / ${goal.token_budget}` : ''} · 阻塞 {goal.blocked_count || 0} 次</div>
    </div>
  )
  return (
    <div className="pane">
      <div className="pane-toolbar"><button onClick={load}>刷新</button>{data.plan_mode && <span className="chip">Plan 模式</span>}</div>
      <div className="pane-body scroll">
        {goalHtml}
        {plans.length > 0 && <div className="group">计划</div>}
        {plans.map((plan) => {
          const steps = plan.plan && Array.isArray(plan.plan.steps) ? plan.plan.steps : []
          const done = steps.filter((step) => step.status === 'completed' || step.status === 'done').length
          return (
            <div key={plan.id} className="row">
              <span>▤ {plan.id}</span>
              <span className="muted">{steps.length ? `${done}/${steps.length} 步` : ''}</span>
            </div>
          )
        })}
        {data.todos.length > 0 && <div className="group">待办</div>}
        {data.todos.map((todo, index) => (
          <div key={index} className={`row todo ${todo.status === 'completed' ? 'done' : ''}`}>
            <span>{todo.status === 'completed' ? '✔' : todo.status === 'in_progress' ? '▶' : '○'} {todo.content}</span>
          </div>
        ))}
        {data.tasks.length > 0 && <div className="group">任务工具</div>}
        {data.tasks.map((task, index) => <div key={index} className="row"><span>◆ {task.tool}</span><span className="muted">{task.detail}</span></div>)}
        {data.crons.length > 0 && <div className="group">定时记录</div>}
        {data.crons.map((cron, index) => <div key={index} className="row"><span>⏱ {cron.tool}</span><span className="muted">{cron.detail}</span></div>)}
      </div>
    </div>
  )
}

function AttachmentsPane({ onError }) {
  const [data, setData] = useState({ items: [], pastes: [] })
  const load = useCallback(() => {
    api('/api/attachments').then(setData).catch((error) => onError?.(`附件加载失败：${error.message}`))
  }, [onError])
  useEffect(() => { load() }, [load])
  return (
    <div className="pane">
      <div className="pane-toolbar"><button onClick={load}>刷新</button><span className="hint">{data.items.length} 图 / {data.pastes.length} 粘贴</span></div>
      <div className="pane-body scroll">
        {data.items.length > 0 && <div className="group">工作区图片</div>}
        <div className="attach-grid">
          {data.items.map((item) => (
            <a key={item.path} href={rawUrl(item.path)} target="_blank" rel="noreferrer" className="attach" title={item.path}>
              <img src={rawUrl(item.path)} alt="" />
              <span>{basename(item.path)}</span>
            </a>
          ))}
        </div>
        {data.pastes.length > 0 && <div className="group">粘贴缓存</div>}
        {data.pastes.map((item) => (
          <a key={item.path} href={rawUrl(item.path)} target="_blank" rel="noreferrer" className="row">
            <span>{basename(item.path)}</span><span className="muted">{Math.max(1, Math.round((item.size || 0) / 1024))}K</span>
          </a>
        ))}
        {data.items.length === 0 && data.pastes.length === 0 && <div className="empty">没有附件</div>}
      </div>
    </div>
  )
}

function TerminalPane() {
  const [text, setText] = useState('')
  const [status, setStatus] = useState('未连接')
  const [line, setLine] = useState('')
  const socketRef = useRef(null)
  const outRef = useRef(null)

  useEffect(() => {
    const socket = new WebSocket(wsUrl('/ws/terminal'))
    socket.binaryType = 'arraybuffer'
    socket.onopen = () => setStatus('已连接')
    socket.onmessage = (event) => {
      const chunk = typeof event.data === 'string' ? event.data : new TextDecoder().decode(event.data)
      setText((previous) => (previous + chunk.replace(ANSI_RE, '')).slice(-200000))
    }
    socket.onclose = () => setStatus('已断开')
    socket.onerror = () => setStatus('连接失败')
    socketRef.current = socket
    return () => socket.close()
  }, [])

  useEffect(() => {
    if (outRef.current) outRef.current.scrollTop = outRef.current.scrollHeight
  }, [text])

  const send = (data) => {
    if (socketRef.current?.readyState === 1) socketRef.current.send(JSON.stringify({ type: 'input', data }))
  }
  return (
    <div className="pane">
      <div className="pane-toolbar">
        <span className="hint">{status}</span>
        <span className="spacer" />
        <button onClick={() => setText('')}>清屏</button>
        <button onClick={() => send('\u0003')}>Ctrl+C</button>
      </div>
      <pre className="terminal" ref={outRef}>{text}</pre>
      <div className="term-input">
        <input
          value={line}
          onChange={(event) => setLine(event.target.value)}
          onKeyDown={(event) => { if (event.key === 'Enter') { send(line + '\r'); setLine('') } }}
          placeholder="输入命令，Enter 发送（真 PTY）"
        />
      </div>
    </div>
  )
}

function SettingsPage({ settings, meta, onClose, onSave }) {
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
              <option value="dark">dark</option>
              <option value="light">light</option>
            </select>
          </label>
        </div>
        <div className="section">
          <h3>Agent</h3>
          <label className="row">
            <span>默认权限模式</span>
            <select value={permission} onChange={(event) => setPermission(event.target.value)}>
              <option value="workspace-write">workspace-write</option>
              <option value="read-only">read-only</option>
              <option value="danger-full-access">danger-full-access</option>
            </select>
          </label>
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
