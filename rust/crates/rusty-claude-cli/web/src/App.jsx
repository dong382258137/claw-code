import React, { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { Terminal } from '@xterm/xterm'
import { FitAddon } from '@xterm/addon-fit'
import '@xterm/xterm/css/xterm.css'
import { api, postJson, token, withToken, withWorkspace, wsUrl, rawUrl } from './api'
import { Markdown } from './markdown'
import { groupTurns, processTitle } from './turns'
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
  const push = useCallback((text, kind = 'info', action = null) => {
    if (!text) return
    const id = `${Date.now()}-${Math.random()}`
    setToasts((list) => {
      const last = list[list.length - 1]
      // 同类错误 2 秒内只提示一次，避免失败重试时刷屏。
      if (last && last.text === text && last.kind === kind && Date.now() - last.at < 2000) return list
      // 错误提示常驻且只保留最新一条：断流/失败的原因需要用户读到，
      // 不能像普通提示那样 5 秒后自己消失。
      const base = kind === 'error' ? list.filter((toast) => toast.kind !== 'error') : list
      return [...base, { id, text, kind, action, at: Date.now() }]
    })
    if (kind !== 'error') {
      setTimeout(() => setToasts((list) => list.filter((toast) => toast.id !== id)), 5000)
    }
  }, [])
  return { toasts, push }
}

/* ---------------- 错误边界 ---------------- */

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

window.addEventListener('error', (event) => {
  reportClientError(event.message, event.error?.stack || `${event.filename}:${event.lineno}:${event.colno}`)
})
window.addEventListener('unhandledrejection', (event) => {
  reportClientError(event.reason?.message || event.reason, event.reason?.stack)
})

export default function App() {
  const [sessions, setSessions] = useState([])
  const [sessionTotal, setSessionTotal] = useState(0)
  const [workspaces, setWorkspaces] = useState([])
  const [workspacePickerOpen, setWorkspacePickerOpen] = useState(false)
  const [filter, setFilter] = useState('')
  const [current, setCurrent] = useState(null)
  const [messages, setMessages] = useState([])
  const [optimistic, setOptimistic] = useState(null)
  const [running, setRunning] = useState(false)
  const [live, setLive] = useState('')
  const [liveThinking, setLiveThinking] = useState('')
  const [liveTools, setLiveTools] = useState([])
  const [approvals, setApprovals] = useState([])
  const [upgrading, setUpgrading] = useState('')
  const [loadingSession, setLoadingSession] = useState(false)
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
  // 稳定的错误回调：面板的 load 依赖它，若每次渲染换新函数会触发无限重取。
  const panelError = useCallback((message) => {
    if (message) push(message, 'error')
  }, [push])

  const currentId = current?.id || null
  const activeWorkspace = current?.workspace || meta.workspace || ''

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

  const loadSessions = useCallback(async () => {
    try {
      const [body, wsBody] = await Promise.all([
        api('/api/sessions?all=1&limit=200&offset=0'),
        api('/api/workspaces').catch(() => ({ workspaces: [] })),
      ])
      setSessions(body.sessions || [])
      setSessionTotal(body.total || 0)
      setWorkspaces(wsBody.workspaces || [])
    } catch (_) {}
  }, [])
  const loadMoreSessions = useCallback(async () => {
    try {
      const body = await api(`/api/sessions?all=1&limit=200&offset=${sessions.length}`)
      setSessions((list) => [...list, ...(body.sessions || []).filter((item) => !list.some((old) => old.id === item.id))])
      setSessionTotal(body.total || 0)
    } catch (error) {
      push(`加载更多失败：${error.message}`, 'error')
    }
  }, [push, sessions.length])

  const addWorkspace = useCallback(async (path) => {
    const trimmed = String(path || '').trim()
    if (!trimmed) return
    try {
      await postJson('/api/workspaces', { path: trimmed })
      await loadSessions()
      push('工作区已添加')
    } catch (error) {
      push(`添加工作区失败：${error.message}`, 'error')
    }
  }, [loadSessions, push])

  // 原生文件夹选择框：返回选中的工作区（取消返回 null）。
  const pickWorkspaceFolder = useCallback(async () => {
    const body = await postJson('/api/workspaces/pick', {})
    if (body.workspace) {
      await loadSessions()
      return body.workspace
    }
    return null
  }, [loadSessions])
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
    setLive('')
    setMessages([])
    setLoadingSession(true)
    api(`/api/session/${encodeURIComponent(currentId)}`)
      .then((view) => { if (!closed) { setCurrent((current) => ({ ...view, title: current?.title || view.id })); setMessages(view.messages || []); setLoadingSession(false) } })
      .catch((error) => { setLoadingSession(false); push(`读取会话失败：${error.message}`, 'error') })

    const source = new EventSource(withToken(`/api/session/${encodeURIComponent(currentId)}/stream`))
    source.addEventListener('snapshot', (event) => {
      if (closed) return
      try {
        const view = JSON.parse(event.data)
        setCurrent((current) => ({ ...view, title: current?.title || view.id }))
        setMessages(view.messages || [])
        setLoadingSession(false)
        const last = view.messages?.[view.messages.length - 1]
        if (last && last.role === 'assistant') { setLive(''); setLiveThinking(''); setLiveTools([]) }
        setOptimistic((pending) => {
          if (!pending) return null
          return last && last.role === 'user' ? null : pending
        })
      } catch (_) {}
    })
    // 服务端可能已经退出或重启（token 随之更换）：断线时对账一次真实 running
    // 状态，别让 UI 永远停在“运行中”。
    let lastReconcile = 0
    const reconcile = () => {
      if (closed) return
      const now = Date.now()
      if (now - lastReconcile < 5000) return
      lastReconcile = now
      api(`/api/session/${encodeURIComponent(currentId)}/status`)
        .then((status) => {
          if (status?.running) return
          setRunning(false)
          setLive('')
          setLiveThinking('')
          setLiveTools([])
          setOptimistic(null)
        })
        .catch(() => {
          setRunning(false)
          push('与本地服务断开：服务可能已重启（token 已更换），请用新打开的页面继续。', 'error')
        })
    }
    source.onopen = () => setConnected(true)
    source.onerror = () => { setConnected(false); reconcile() }

    // token 级流式事件（进程内 turn 的 StatusEmitter → SSE）。
    const events = new EventSource(withToken(`/api/session/${encodeURIComponent(currentId)}/events`))
    events.addEventListener('stream', (event) => {
      if (closed) return
      try {
        const data = JSON.parse(event.data)
        if (data.kind === 'delta') setLive((previous) => previous + data.text)
        else if (data.kind === 'start') setRunning(true)
        else if (data.kind === 'thinking') setLiveThinking((previous) => (data.done && data.text ? data.text : previous + (data.done ? '' : data.text)))
        else if (data.kind === 'approval_request') setApprovals((list) => (list.some((item) => item.id === data.id) ? list : [...list, data]))
        else if (data.kind === 'tool_use') setLiveTools((list) => [...list, { id: data.id, name: data.name, input: data.input, output: '', done: false, is_error: false }])
        else if (data.kind === 'tool_result') {
          setLiveTools((list) => {
            // runtime 的 ToolResult 事件可能不带 id：退化为匹配最近的同名未完成工具。
            let index = data.id ? list.findIndex((tool) => tool.id === data.id) : -1
            if (index === -1) {
              for (let i = list.length - 1; i >= 0; i -= 1) {
                if (!list[i].done && list[i].name === data.name) { index = i; break }
              }
            }
            if (index === -1) return [...list, { id: data.id || `${data.name}-${Date.now()}`, name: data.name, input: '', output: data.output, done: true, is_error: data.is_error }]
            const next = list.slice()
            next[index] = { ...next[index], output: data.output, done: true, is_error: data.is_error }
            return next
          })
        }
        else if (data.kind === 'done') { setRunning(false) }
        else if (data.kind === 'upgrading') { setUpgrading(data.message || '正在热升级：重建并重启 Web 服务…'); setRunning(false) }
        else if (data.kind === 'error') { setLive(''); setLiveThinking(''); setLiveTools([]); push(`流式错误：${data.message}`, 'error') }
      } catch (_) {}
    })
    events.onerror = () => { setConnected(false); reconcile() }
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

  // Web 热升级：服务重启期间轮询 /api/meta，恢复后自动 reload 拿到新前端。
  useEffect(() => {
    if (!upgrading) return undefined
    const started = Date.now()
    const timer = setInterval(async () => {
      if (Date.now() - started > 240_000) {
        clearInterval(timer)
        setUpgrading('升级超时：请查看 .claw/upgrade-helper.log，或手动重启 claw-web')
        return
      }
      try {
        const response = await fetch(withToken('/api/meta'))
        if (response.ok) {
          clearInterval(timer)
          location.reload()
        }
      } catch (_) {}
    }, 1500)
    return () => clearInterval(timer)
  }, [upgrading])

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

  useEffect(() => {
    const onResize = () => setPanelWidth((width) => clampWidth(width, dock, sidebarCollapsed))
    window.addEventListener('resize', onResize)
    return () => window.removeEventListener('resize', onResize)
  }, [dock, sidebarCollapsed])

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
  const selectSession = useCallback((session) => {
    setCurrent({ id: session.id, title: session.title, modified: session.modified, workspace: session.workspace })
    setMessages([])
    setLive('')
    setOptimistic(null)
    setApprovals([])
  }, [])

  const createSession = useCallback(async (workspace) => {
    try {
      const created = await postJson('/api/sessions', { workspace: workspace || activeWorkspace || undefined })
      const session = {
        id: created.id,
        title: created.title || '新会话',
        modified: Math.floor(Date.now() / 1000),
        workspace: created.workspace || workspace || activeWorkspace,
      }
      setSessions((list) => [session, ...list.filter((item) => item.id !== session.id)])
      selectSession(session)
      setTimeout(() => document.querySelector('.composer textarea')?.focus(), 80)
      push('已新建会话')
    } catch (error) {
      push(`新建会话失败：${error.message}`, 'error')
    }
  }, [activeWorkspace, push, selectSession])

  const startInFolder = useCallback((path) => {
    setWorkspacePickerOpen(false)
    createSession(path)
  }, [createSession])

  const pickAndStart = useCallback(async () => {
    try {
      const workspace = await pickWorkspaceFolder()
      if (workspace) startInFolder(workspace.path)
    } catch (error) {
      if (String(error.message).includes('HTTP 501')) {
        const path = window.prompt('当前平台不支持文件夹选择框，请输入工作区绝对路径')
        if (path) {
          await addWorkspace(path)
          startInFolder(path)
        }
      } else {
        push(`选择工作区失败：${error.message}`, 'error')
      }
    }
  }, [addWorkspace, pickWorkspaceFolder, push, startInFolder])

  const renameSession = useCallback(async (session) => {
    const title = window.prompt('重命名会话', session.title || session.id)
    if (title === null) return
    const trimmed = title.trim()
    if (!trimmed) { push('标题不能为空', 'error'); return }
    try {
      await api(`/api/session/${encodeURIComponent(session.id)}`, {
        method: 'PATCH',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ title: trimmed }),
      })
      setSessions((list) => list.map((item) => (item.id === session.id ? { ...item, title: trimmed } : item)))
      setCurrent((value) => (value?.id === session.id ? { ...value, title: trimmed } : value))
    } catch (error) {
      push(`重命名失败：${error.message}`, 'error')
    }
  }, [push])

  const deleteSession = useCallback(async (session) => {
    if (!window.confirm(`删除会话「${session.title || session.id}」？此操作不可恢复。`)) return
    try {
      await api(`/api/session/${encodeURIComponent(session.id)}`, { method: 'DELETE' })
      setSessions((list) => list.filter((item) => item.id !== session.id))
      setSessionTotal((total) => Math.max(0, total - 1))
      if (currentId === session.id) {
        setCurrent(null)
        setMessages([])
        setLive('')
        setLiveTools([])
        setApprovals([])
      }
      push('会话已删除')
    } catch (error) {
      push(`删除失败：${error.message}`, 'error')
    }
  }, [currentId, push])

  const decideApproval = useCallback(async (approval, allow) => {
    setApprovals((list) => list.filter((item) => item.id !== approval.id))
    try {
      await postJson(`/api/session/${encodeURIComponent(currentId)}/approval`, { id: approval.id, allow })
    } catch (error) {
      push(`审批应答失败：${error.message}`, 'error')
    }
  }, [currentId, push])

  const send = useCallback(async (text) => {
    if (!currentId) { push('请先在左侧选择一个会话', 'error'); return }
    const trimmed = text.trim()
    if (!trimmed) return
    setLive('')
    setLiveThinking('')
    setLiveTools([])
    setOptimistic({ text: trimmed, at: Date.now() })
    setRunning(true)
    try {
      await postJson(`/api/session/${encodeURIComponent(currentId)}/prompt`, {
        text: trimmed,
        permission: webSettings.permission || 'workspace-write',
        model: webSettings.model || undefined,
      })
    } catch (error) {
      setOptimistic(null)
      setRunning(false)
      push(`发送失败：${error.message}`, 'error', { label: '重试', run: () => send(text) })
    }
  }, [currentId, push, webSettings.permission])

  const stop = useCallback(async () => {
    if (!currentId || !running) return
    try {
      await api(`/api/session/${encodeURIComponent(currentId)}/interrupt`, { method: 'POST' })
      setRunning(false)
      push('已请求中断')
    } catch (error) {
      push(`中断失败：${error.message}`, 'error')
    }
  }, [currentId, push, running])

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
      onSelect={(session) => selectSession(session)}
      onNew={createSession}
      onNewIn={(workspace) => createSession(workspace)}
      onAddWorkspace={addWorkspace}
      onOpenPicker={() => setWorkspacePickerOpen(true)}
      workspaces={workspaces}
      activeWorkspace={activeWorkspace}
      onRename={renameSession}
      onDelete={deleteSession}
      total={sessionTotal}
      onLoadMore={loadMoreSessions}
      onSettings={() => setSettingsOpen(true)}
      onTheme={() => saveSettings({ theme: webSettings.theme === 'dark' ? 'light' : 'dark' })}
      theme={webSettings.theme}
    />
  )

  const main = (
    <main>
      <TopBar
        title={current?.title || current?.id || '未选择会话'}
        chips={[
          current?.model,
          current?.workspace ? basename(current.workspace) : null,
          current ? `${current.message_count ?? messages.length} 条` : null,
          current?.modified ? `更新于 ${relTime(current.modified)}` : null,
          running ? '运行中' : null,
        ].filter(Boolean)}
        connected={connected}
        running={running}
        model={webSettings.model || ''}
        onModel={(value) => saveSettings({ model: value })}
        onSettings={() => setSettingsOpen(true)}
        onTogglePanel={() => (panelOpen ? closePanel() : openPanel())}
        panelOpen={panelOpen}
      />
      {!connected && <div className="banner">连接中断，正在重连…</div>}
      {upgrading && <div className="banner upgrade">⚙ {upgrading}</div>}
      <Conversation messages={messages} optimistic={optimistic} live={live} liveThinking={liveThinking} liveTools={liveTools} loading={loadingSession} />
      <Composer
        running={running}
        onSend={send}
        onStop={stop}
        onQuickAction={runQuickAction}
        onError={(message) => push(message, 'error')}
        currentId={currentId}
        permission={webSettings.permission || 'workspace-write'}
        onPermission={(value) => saveSettings({ permission: value })}
        model={webSettings.model || ''}
        onModel={(value) => saveSettings({ model: value })}
        workspace={activeWorkspace}
        stats={current?.stats}
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
      workspace={activeWorkspace}
      meta={meta}
      onError={panelError}
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
      {workspacePickerOpen && (
        <WorkspacePicker
          workspaces={workspaces}
          onChoose={startInFolder}
          onNativePick={pickAndStart}
          onClose={() => setWorkspacePickerOpen(false)}
        />
      )}
      <Toasts toasts={toasts} />
      {approvals.length > 0 && (
        <div className="approval-mask">
          <div className="approval">
            <h3>需要你的批准</h3>
            {approvals.map((approval) => (
              <div key={approval.id} className="approval-item">
                <div>
                  工具 <b>{approval.tool}</b> 需要「{approval.required_mode}」权限
                  {approval.reason ? `：${approval.reason}` : ''}
                </div>
                {approval.input && <pre>{approval.input}</pre>}
                <div className="approval-actions">
                  <button className="danger" onClick={() => decideApproval(approval, false)}>拒绝</button>
                  <button className="primary" onClick={() => decideApproval(approval, true)}>允许</button>
                </div>
              </div>
            ))}
            <div className="muted">不操作会在 120 秒后自动拒绝。</div>
          </div>
        </div>
      )}
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

function fmtClock(ms) {
  if (!ms) return ''
  const date = new Date(ms)
  const pad = (value) => String(value).padStart(2, '0')
  return `${pad(date.getHours())}:${pad(date.getMinutes())}`
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
        <div key={toast.id} className={`toast ${toast.kind}`}>
          <span>{toast.text}</span>
          {toast.action && (
            <button className="toast-action" onClick={() => toast.action.run()}>{toast.action.label}</button>
          )}
        </div>
      ))}
    </div>
  )
}

function Sidebar({ sessions, filter, onFilter, currentId, collapsed, onToggle, onSelect, onNew, onNewIn, onAddWorkspace, onOpenPicker, workspaces, activeWorkspace, onRename, onDelete, total, onLoadMore, onSettings, onTheme, theme }) {
  const [collapsedGroups, setCollapsedGroups] = useState(() => new Set())
  const query = filter.trim().toLowerCase()
  const filtered = sessions.filter((session) =>
    !query ||
    (session.title || '').toLowerCase().includes(query) ||
    session.id.toLowerCase().includes(query) ||
    (session.workspace || '').toLowerCase().includes(query))

  const projects = useMemo(() => {
    const order = []
    for (const item of workspaces) {
      if (item.path && !order.includes(item.path)) order.push(item.path)
    }
    for (const session of sessions) {
      if (session.workspace && !order.includes(session.workspace)) order.push(session.workspace)
    }
    return order.map((path) => ({
      path,
      label: basename(path) || path,
      sessions: filtered.filter((session) => session.workspace === path),
      count: sessions.filter((session) => session.workspace === path).length,
    })).filter((project) => !query || project.sessions.length > 0)
  }, [workspaces, sessions, filtered, query])

  const toggleGroup = (path) => {
    setCollapsedGroups((current) => {
      const next = new Set(current)
      if (next.has(path)) next.delete(path)
      else next.add(path)
      return next
    })
  }

  return (
    <aside className={collapsed ? 'collapsed' : ''}>
      <div className="brand">
        <span><b>CLAW</b></span>
        {!collapsed && (
          <div className="brand-actions">
            <button onClick={() => onNew(activeWorkspace)} title="在当前工作区新建任务">＋</button>
            <button onClick={onTheme} title="切换主题">{theme === 'dark' ? '◐' : '◑'}</button>
            <button onClick={onSettings} title="设置">⚙</button>
            <button onClick={onToggle} title="收起侧栏 (Ctrl+B)">‹</button>
          </div>
        )}
      </div>
      {collapsed && (
        <div className="rail">
          <button onClick={() => onNew(activeWorkspace)} title="新建任务">＋</button>
          <button onClick={onToggle} title="展开侧栏 (Ctrl+B)">☰</button>
          <button onClick={onSettings} title="设置">⚙</button>
          <button onClick={onTheme} title="切换主题">◐</button>
        </div>
      )}
      {!collapsed && (
        <>
          <button className="new-session" onClick={() => onNew(activeWorkspace)}>＋ 新建任务</button>
          <div className="search">
            <input id="session-search" value={filter} onChange={(event) => onFilter(event.target.value)} placeholder="搜索会话 / Ctrl+K" />
          </div>
          <div className="projects-head">
            <span>项目</span>
            <button
              title="选择工作区文件夹"
              onClick={onOpenPicker}
            >＋</button>
          </div>
          <div className="session-list">
            {projects.length === 0 && <div className="empty">没有匹配的会话</div>}
            {projects.map((project) => {
              const isCollapsed = collapsedGroups.has(project.path)
              const active = project.path === activeWorkspace
              return (
                <div className={`project ${active ? 'active-workspace' : ''}`} key={project.path}>
                  <div className="project-head" title={project.path}>
                    <button className="project-toggle" onClick={() => toggleGroup(project.path)}>
                      {isCollapsed ? '▸' : '▾'} 📁 {project.label}
                      <span className="project-count">{project.count}</span>
                    </button>
                    <button
                      className="project-new"
                      title={`在 ${project.label} 新建任务`}
                      onClick={() => onNewIn(project.path)}
                    >＋</button>
                  </div>
                  {!isCollapsed && project.sessions.map((session) => (
                    <div key={session.id} className={`session-row nested ${session.id === currentId ? 'active' : ''}`}>
                      <button className="session" onClick={() => onSelect(session)} title={session.workspace || ''}>
                        <span className="title">{session.title || session.id}</span>
                        <span className="meta">{relTime(session.modified)}</span>
                      </button>
                      <span className="session-actions">
                        <button title="重命名" onClick={(event) => { event.stopPropagation(); onRename(session) }}>✎</button>
                        <button title="删除" onClick={(event) => { event.stopPropagation(); onDelete(session) }}>×</button>
                      </span>
                    </div>
                  ))}
                </div>
              )
            })}
            {total > sessions.length && (
              <div className="session-more">
                <button onClick={onLoadMore}>加载更多（{sessions.length}/{total}）</button>
              </div>
            )}
          </div>
        </>
      )}
    </aside>
  )
}

function WorkspacePicker({ workspaces, onChoose, onNativePick, onClose }) {
  const [query, setQuery] = useState('')
  const [busy, setBusy] = useState(false)
  const inputRef = useRef(null)
  useEffect(() => {
    inputRef.current?.focus()
    const onKey = (event) => { if (event.key === 'Escape') onClose() }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [onClose])
  const filtered = workspaces.filter((item) => {
    const text = query.trim().toLowerCase()
    return !text || item.name.toLowerCase().includes(text) || item.path.toLowerCase().includes(text)
  })
  return (
    <div className="picker-mask" onMouseDown={onClose}>
      <div className="workspace-picker" onMouseDown={(event) => event.stopPropagation()}>
        <div className="picker-search">
          <span>🔍</span>
          <input ref={inputRef} value={query} onChange={(event) => setQuery(event.target.value)} placeholder="搜索文件夹" />
        </div>
        <div className="picker-list">
          {filtered.map((item) => (
            <button key={item.path} className="picker-row" onClick={() => onChoose(item.path)} title={item.path}>
              <span className="picker-icon">📁</span>
              <span className="picker-text">
                <b>{item.name}</b>
                <small>{item.path}</small>
              </span>
              {item.session_count > 0 && <span className="picker-count">{item.session_count}</span>}
            </button>
          ))}
          {filtered.length === 0 && <div className="empty">没有匹配的文件夹</div>}
        </div>
        <button
          className="picker-native"
          disabled={busy}
          onClick={async () => {
            setBusy(true)
            try { await onNativePick() } finally { setBusy(false) }
          }}
        >
          <span>＋</span> {busy ? '正在选择…' : '选择文件夹…'}
        </button>
      </div>
    </div>
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
      <button onClick={onSettings} title="设置">⚙</button>
      <button onClick={onTogglePanel}>{panelOpen ? '隐藏面板' : '显示面板'}</button>
    </div>
  )
}

function Conversation({ messages, optimistic, live, liveThinking, liveTools, loading }) {
  const ref = useRef(null)
  const stick = useRef(true)
  const [atBottom, setAtBottom] = useState(true)
  const [visibleCount, setVisibleCount] = useState(120)
  useEffect(() => {
    const node = ref.current
    if (!node) return
    if (stick.current) node.scrollTop = node.scrollHeight
  }, [messages, optimistic, live, liveThinking, liveTools])
  useEffect(() => {
    if ((messages || []).length === 0) setVisibleCount(120)
  }, [messages.length])
  const onScroll = () => {
    const node = ref.current
    if (!node) return
    const bottom = node.scrollHeight - node.scrollTop - node.clientHeight < 48
    stick.current = bottom
    setAtBottom(bottom)
  }
  const jumpToLatest = () => {
    const node = ref.current
    if (node) node.scrollTop = node.scrollHeight
    stick.current = true
    setAtBottom(true)
  }
  const visible = useMemo(
    () => (messages || []).filter((message) => (message.blocks || []).length > 0),
    [messages],
  )
  const shown = visible.length > visibleCount ? visible.slice(-visibleCount) : visible
  const turns = useMemo(() => groupTurns(shown), [shown])
  return (
    <>
      <div className="messages" ref={ref} onScroll={onScroll}>
        <div className="chat-column">
        {loading && visible.length === 0 && (
          <div className="skeleton-list">
            <div className="skeleton" /><div className="skeleton short" /><div className="skeleton" />
          </div>
        )}
        {!loading && visible.length === 0 && !optimistic && !live && (
          <div className="empty">
            <div>选择一个会话，或直接在下方输入。</div>
            <div className="empty-hint">Enter 发送 · Shift+Enter 换行 · / 快捷操作 · @ 引用文件</div>
          </div>
        )}
        {visible.length > shown.length && (
          <div className="load-earlier">
            <button onClick={() => setVisibleCount((count) => count + 120)}>
              加载更早（还有 {visible.length - shown.length} 条）
            </button>
          </div>
        )}
        {turns.map((turn, index) => <TurnView key={index} turn={turn} />)}
        {optimistic && (
          <UserMessage
            pending
            message={{ role: 'user', blocks: [{ kind: 'text', text: optimistic.text }], ts: optimistic.at }}
          />
        )}
        {live && <LiveTurn thinking={liveThinking} tools={liveTools || []} text={live} />}
        </div>
      </div>
      {!atBottom && <button className="jump" onClick={jumpToLatest}>回到最新 ↓</button>}
    </>
  )
}

function UserMessage({ message, pending }) {
  const text = (message.blocks || []).filter((block) => block.kind === 'text').map((block) => block.text).join('\n')
  return (
    <div className="msg user">
      <div className="msg-head">
        <span className="who">你</span>
        {message.ts ? <span className="ts">{fmtClock(message.ts)}</span> : null}
        {pending ? <span className="pending">发送中…</span> : null}
      </div>
      <div className="msg-body"><Markdown text={text} /></div>
    </div>
  )
}

function AnswerMessage({ text, ts }) {
  const [copied, setCopied] = useState(false)
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(text)
      setCopied(true)
      setTimeout(() => setCopied(false), 1200)
    } catch (_) {}
  }
  return (
    <div className="msg assistant">
      <div className="msg-head">
        <span className="who">助手</span>
        {ts ? <span className="ts">{fmtClock(ts)}</span> : null}
        <button className="ghost" onClick={copy}>{copied ? '已复制' : '复制'}</button>
      </div>
      <div className="msg-body"><Markdown text={text} /></div>
    </div>
  )
}

function ProcessRow({ row }) {
  if (row.kind === 'thinking') {
    return (
      <details className="process-thinking">
        <summary>思考</summary>
        <pre>{row.text}</pre>
      </details>
    )
  }
  if (row.kind === 'tool') {
    const state = row.output === null ? 'running' : row.error ? 'error' : 'done'
    const mark = state === 'running' ? '◌' : state === 'error' ? '✗' : '✓'
    const output = row.output || ''
    const isDiff = /^(diff --git|@@ )/m.test(output) || output.includes('\n@@ ')
    return (
      <details className={`process-tool ${state}`} open={state === 'running'}>
        <summary><span className="tool-mark">{mark}</span> {row.name}</summary>
        {row.input ? <pre className="tool-input">{row.input}</pre> : null}
        {output ? (isDiff ? <pre className="patch">{colorizePatch(output)}</pre> : <pre>{output}</pre>) : null}
      </details>
    )
  }
  return <div className="process-text"><Markdown text={row.text} /></div>
}

function ProcessGroup({ rows, live = false, durationMs = 0 }) {
  const [open, setOpen] = useState(live)
  useEffect(() => { if (live) setOpen(true) }, [live])
  if (!rows.length) return null
  return (
    <div className={`process ${live ? 'live' : ''}`}>
      <button type="button" className="process-head" onClick={() => setOpen((value) => !value)}>
        <span className="process-icon">✦</span>
        <span className="process-label">{processTitle(rows, live, durationMs)}</span>
        <span className={`process-chevron ${open ? 'open' : ''}`}>⌄</span>
      </button>
      {open && <div className="process-body">{rows.map((row, index) => <ProcessRow key={index} row={row} />)}</div>}
    </div>
  )
}

function TurnView({ turn }) {
  const durationMs = turn.ts && turn.end ? Math.max(0, turn.end - turn.ts) : 0
  return (
    <div className="turn">
      {turn.user && <UserMessage message={turn.user} />}
      {turn.rows.length > 0 && <ProcessGroup rows={turn.rows} durationMs={durationMs} />}
      {turn.answer && <AnswerMessage text={turn.answer} ts={turn.end} />}
    </div>
  )
}

function LiveTurn({ thinking, tools, text }) {
  const rows = []
  if (thinking) rows.push({ kind: 'thinking', text: thinking })
  for (const tool of tools) {
    rows.push({
      kind: 'tool',
      name: tool.name,
      input: tool.input || '',
      output: tool.done ? (tool.output || '') : null,
      error: Boolean(tool.is_error),
    })
  }
  return (
    <div className="turn live">
      {rows.length > 0 && <ProcessGroup rows={rows} live />}
      {text && (
        <div className="msg assistant">
          <div className="msg-head"><span className="who">助手</span><span className="pending">流式输出…</span></div>
          <div className="msg-body"><Markdown text={text} /><span className="cursor" /></div>
        </div>
      )}
    </div>
  )
}

function Composer({ running, onSend, onStop, onQuickAction, onError, currentId, permission, onPermission, model, onModel, workspace, stats }) {
  const [value, setValue] = useState('')
  const [files, setFiles] = useState([])
  const [menu, setMenu] = useState(null)
  const [cursor, setCursor] = useState(0)
  const [attachments, setAttachments] = useState([])
  const [uploading, setUploading] = useState(false)
  const [modelDraft, setModelDraft] = useState(model || '')
  const ref = useRef(null)
  const areaRef = useRef(null)
  const uploadRef = useRef(null)

  useEffect(() => { ref.current = value }, [value])
  useEffect(() => { setModelDraft(model || '') }, [model])
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

  const uploadFile = async (list) => {
    const file = list?.[0]
    if (!file) return
    setUploading(true)
    try {
      const response = await fetch(withToken(withWorkspace('/api/upload?name=' + encodeURIComponent(file.name), workspace)), { method: 'POST', body: file })
      if (!response.ok) throw new Error(`HTTP ${response.status}`)
      const body = await response.json()
      setAttachments((items) => [...items, { name: file.name, path: body.path, size: file.size }])
      setValue((previous) => (previous.trim() ? previous.replace(/\s*$/, '') + '\n' : '') + '@' + body.path + ' ')
      requestAnimationFrame(() => areaRef.current?.focus())
    } catch (error) {
      onError?.(`上传失败：${error.message}`)
    } finally {
      setUploading(false)
      if (uploadRef.current) uploadRef.current.value = ''
    }
  }

  const removeAttachment = (path) => {
    setAttachments((items) => items.filter((item) => item.path !== path))
    setValue((previous) => previous.split('\n').filter((line) => line.trim() !== '@' + path).join('\n'))
  }

  const commitModel = () => {
    const next = modelDraft.trim()
    if (next !== (model || '')) onModel(next)
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
      {attachments.length > 0 && (
        <div className="composer-attachments">
          {attachments.map((item) => (
            <span key={item.path} className="attachment-chip" title={item.path}>
              📎 {item.name}
              <button type="button" className="ghost" onClick={() => removeAttachment(item.path)}>×</button>
            </span>
          ))}
        </div>
      )}
      <div className="composer-box">
        <textarea
          ref={areaRef}
          value={value}
          rows={2}
          placeholder={currentId ? '输入消息…' : '先选择一个会话'}
          onChange={(event) => { setValue(event.target.value); grow(event) }}
          onKeyDown={onKeyDown}
        />
        <div className="composer-bar">
          <button type="button" className="composer-plus" title="上传附件" onClick={() => uploadRef.current?.click()} disabled={uploading}>
            {uploading ? '…' : '＋'}
          </button>
          <input ref={uploadRef} type="file" style={{ display: 'none' }} onChange={(event) => uploadFile(event.target.files)} />
          <label className="permission-chip" title="本轮权限模式">
            <span className="permission-shield">🛡</span>
            <select value={permission} onChange={(event) => onPermission(event.target.value)}>
              <option value="read-only">只读</option>
              <option value="workspace-write">工作区可写</option>
              <option value="danger-full-access">完全权限</option>
            </select>
            <span className="chevron">⌄</span>
          </label>
          <span className="bar-spacer" />
          <label className="model-select" title="本轮模型（留空使用工作区默认）">
            <input
              list="model-presets"
              value={modelDraft}
              placeholder="默认模型"
              onChange={(event) => setModelDraft(event.target.value)}
              onBlur={commitModel}
              onKeyDown={(event) => { if (event.key === 'Enter') { commitModel(); event.target.blur() } }}
            />
            <span className="chevron">⌄</span>
          </label>
          <datalist id="model-presets">
            <option value="deepseek-v4-flash" />
            <option value="deepseek-chat" />
            <option value="deepseek-reasoner" />
          </datalist>
          {running
            ? <button type="button" className="send-circle stop" onClick={onStop} title="停止">■</button>
            : <button type="submit" className="send-circle" disabled={!value.trim()} title="发送（Enter）">↑</button>}
        </div>
      </div>
      {(stats?.turns > 0 || stats?.total_tokens > 0) && (
        <div className="composer-metrics">
          <span>↻ {stats.turns || 0} 轮 {stats.steps || 0} 步{stats.duration_ms > 0 && stats.output_tokens > 0 ? ` · ${Math.max(1, Math.round((stats.output_tokens || 0) / (stats.duration_ms / 1000)))} tok/s` : ''}</span>
          <span>💾 {formatTokens(stats.total_tokens || 0)} tok · 缓存命中 {stats.cache_hit_pct || 0}%</span>
        </div>
      )}
    </form>
  )
}

function formatTokens(value) {
  const count = Number(value) || 0
  if (count >= 1e9) return `${(count / 1e9).toFixed(1)}B`
  if (count >= 1e6) return `${(count / 1e6).toFixed(1)}M`
  if (count >= 1e3) return `${(count / 1e3).toFixed(1)}k`
  return String(count)
}

/* ---------------- 右侧面板 ---------------- */

function Panel({ tab, onTab, onClose, onDock, width, currentId, workspace, meta, onError, onToast }) {
  // 终端只在第一次打开时挂载，之后保持连接；避免每次加载页面都起一个 PTY。
  const [terminalMounted, setTerminalMounted] = useState(tab === 'terminal')
  const [fullscreen, setFullscreen] = useState(false)
  useEffect(() => { if (tab === 'terminal') setTerminalMounted(true) }, [tab])
  useEffect(() => {
    if (!fullscreen) return undefined
    const onKey = (event) => { if (event.key === 'Escape') setFullscreen(false) }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [fullscreen])
  return (
    <section className={`panel ${fullscreen ? 'fullscreen' : ''}`} style={fullscreen ? undefined : { width }}>
      <div className="panel-head">
        {TABS.map((item) => (
          <button key={item.id} className={tab === item.id ? 'tab active' : 'tab'} onClick={() => onTab(item.id)}>{item.label}</button>
        ))}
        <span className="spacer" />
        <button onClick={onDock} title="切换停靠侧">⇄</button>
        <button onClick={() => setFullscreen((value) => !value)} title={fullscreen ? '退出全屏 (Esc)' : '全屏'}>{fullscreen ? '⤢' : '⛶'}</button>
        <button onClick={onClose} title="收起">×</button>
      </div>
      <div className="panel-body">
        {/* 五个面板常驻，切换只隐藏：终端连接、文件浏览状态不会因切标签丢失。 */}
        <div className={`pane-wrap ${tab === 'files' ? 'active' : ''}`}><FilesPane workspace={workspace} onError={onError} /></div>
        <div className={`pane-wrap ${tab === 'diff' ? 'active' : ''}`}><DiffPane workspace={workspace} onError={onError} /></div>
        <div className={`pane-wrap ${tab === 'tasks' ? 'active' : ''}`}><TasksPane currentId={currentId} workspace={workspace} onError={onError} /></div>
        <div className={`pane-wrap ${tab === 'attachments' ? 'active' : ''}`}><AttachmentsPane workspace={workspace} onError={onError} /></div>
        <div className={`pane-wrap ${tab === 'terminal' ? 'active' : ''}`}>{terminalMounted && <TerminalPane />}</div>
      </div>
    </section>
  )
}

function FilesPane({ workspace, onError }) {
  const [files, setFiles] = useState([])
  const [filter, setFilter] = useState('')
  const [preview, setPreview] = useState(null)
  const [editing, setEditing] = useState(false)
  const [draft, setDraft] = useState('')
  const [loading, setLoading] = useState(false)

  const load = useCallback(() => {
    setLoading(true)
    api(withWorkspace('/api/workspace/files', workspace)).then((body) => setFiles(body.files || [])).catch((error) => onError?.(`文件列表失败：${error.message}`)).finally(() => setLoading(false))
  }, [onError, workspace])

  useEffect(() => { load() }, [load])

  const open = useCallback(async (path) => {
    try {
      const file = await api(withWorkspace(`/api/workspace/file?path=${encodeURIComponent(path)}`, workspace))
      setPreview(file)
      setEditing(false)
      setDraft(file.content || '')
    } catch (error) {
      onError?.(`预览失败：${error.message}`)
    }
  }, [onError, workspace])

  const save = async () => {
    if (!preview) return
    try {
      const result = await postJson('/api/workspace/file', { path: preview.path, content: draft, expected_mtime_ms: preview.mtime_ms || null, workspace: workspace || undefined })
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
          <button onClick={() => {
            if (editing && !window.confirm('有未保存的修改，确定放弃？')) return
            setEditing(false)
            setPreview(null)
          }}>‹ 文件</button>
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
              ? <img className="preview-image" src={rawUrl(preview.path, workspace)} alt={preview.path} />
              : preview.kind === 'pdf'
                ? <iframe className="preview-frame" src={rawUrl(preview.path, workspace)} title={preview.path} />
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

/// 逐 hunk 审查：每个 hunk 可单独「丢弃」，等价 `git checkout -p` 对该块答 no。
function DiffPane({ workspace, onError }) {
  const [diff, setDiff] = useState({ files: [], untracked: [], patch: '', hunks: [] })
  const [selected, setSelected] = useState('')
  // 选中文件的单文件 patch：未跟踪文件没有 hunk，只能靠它展示内容。
  const [filePatch, setFilePatch] = useState('')
  const [busy, setBusy] = useState('')

  const load = useCallback(() => {
    api(withWorkspace('/api/workspace/diff', workspace))
      .then((next) => setDiff({
        files: next.files || [],
        untracked: next.untracked || [],
        patch: next.patch || '',
        hunks: next.hunks || [],
      }))
      .catch((error) => onError?.(`diff 失败：${error.message}`))
  }, [onError, workspace])
  useEffect(() => { load() }, [load])

  // 选中文件变化时取该文件的单文件 patch（未跟踪文件没有 hunk，只有这条路能展示）。
  useEffect(() => {
    if (!selected) { setFilePatch(''); return }
    api(withWorkspace(`/api/workspace/diff?path=${encodeURIComponent(selected)}`, workspace))
      .then((single) => setFilePatch(single.patch || ''))
      .catch(() => setFilePatch(''))
  }, [selected, workspace])

  const discard = async (hunk) => {
    const key = `${hunk.path}#${hunk.index}`
    setBusy(key)
    try {
      await postJson(withWorkspace('/api/workspace/diff/discard', workspace), {
        path: hunk.path,
        index: hunk.index,
        // 指纹是唯一权威判据：服务端用它确认「讨论的是同一块」，
        // 再逐行比对文件内容，任一不符即整块拒绝。
        fingerprint: hunk.fingerprint,
      })
      // 丢弃一块后，同文件其余 hunk 的行号与 index 都会重排，必须整表重取。
      load()
    } catch (error) {
      onError?.(`丢弃失败：${error.message}`)
      load()
    } finally {
      setBusy('')
    }
  }

  const visible = selected ? diff.hunks.filter((hunk) => hunk.path === selected) : []
  const hasHunks = visible.length > 0

  return (
    <div className="pane">
      <div className="pane-toolbar">
        {selected
          ? <button onClick={() => { setSelected(''); load() }}>‹ 列表</button>
          : <button onClick={load}>刷新</button>}
        <span className="hint">{diff.files.length} 个已修改 · {diff.untracked.length} 个未跟踪</span>
      </div>
      <div className="pane-body scroll">
        {!selected && diff.files.map((file) => (
          <button key={file.path} className="diff-row" onClick={() => setSelected(file.path)}>
            <span>{file.path}</span>
            <span><span className="add">+{file.added}</span> <span className="del">-{file.removed}</span></span>
          </button>
        ))}
        {!selected && diff.untracked.length > 0 && <div className="group">未跟踪</div>}
        {!selected && diff.untracked.map((path) => (
          <button key={path} className="diff-row" onClick={() => setSelected(path)}><span>{path}</span><span className="muted">新增</span></button>
        ))}

        {selected && hasHunks && visible.map((hunk) => {
          const key = `${hunk.path}#${hunk.index}`
          return (
            <div key={key} className="hunk">
              <div className="hunk-head">
                <span className="hunk-title">{hunk.header || `@@ 第 ${hunk.new_start} 行`}</span>
                <button
                  className="ghost"
                  disabled={busy === key}
                  title="把这一块改回未修改状态（其余块保留）"
                  onClick={() => discard(hunk)}
                >{busy === key ? '丢弃中…' : '丢弃此块'}</button>
              </div>
              <pre className="patch">
                {(hunk.lines || []).map((line, index) => (
                  <span
                    key={index}
                    className={line.startsWith('+') ? 'l-add' : line.startsWith('-') ? 'l-del' : ''}
                  >{line + '\n'}</span>
                ))}
              </pre>
            </div>
          )
        })}

        {/* 无 hunk 时（未跟踪文件、二进制、纯 mode 变更）回退到该文件的原始 patch 文本。 */}
        {selected && !hasHunks && (
          <pre className="patch">
            {(() => {
              const lines = String(filePatch || '').split('\n')
              const shown = lines.slice(0, 4000)
              return (
                <>
                  {shown.map((line, index) => <span key={index} className={line.startsWith('+') ? 'l-add' : line.startsWith('-') ? 'l-del' : line.startsWith('@@') ? 'l-hunk' : ''}>{line + '\n'}</span>)}
                  {lines.length > shown.length && <span className="muted">{`…已截断 ${lines.length - shown.length} 行…`}</span>}
                  {!filePatch && <span className="muted">该文件没有可展示的文本差异（未跟踪或二进制）。</span>}
                </>
              )
            })()}
          </pre>
        )}
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

function TasksPane({ currentId, workspace, onError }) {
  const [data, setData] = useState({ todos: [], tasks: [], crons: [], plan_mode: null })
  const [goal, setGoal] = useState(null)
  const [plans, setPlans] = useState([])
  const [webTodos, setWebTodos] = useState([])
  const [newTodo, setNewTodo] = useState('')

  const load = useCallback(() => {
    if (!currentId) return
    Promise.all([
      api(`/api/session/${encodeURIComponent(currentId)}/tasks`).catch(() => ({ todos: [], tasks: [], crons: [] })),
      api(withWorkspace('/api/workspace/goal', workspace)).catch(() => ({ goal: null })),
      api(withWorkspace('/api/workspace/plans', workspace)).catch(() => ({ plans: [] })),
      api(`/api/session/${encodeURIComponent(currentId)}/todos`).catch(() => ({ items: [] })),
    ]).then(([tasks, goalBody, planBody, todoBody]) => {
      setData(tasks)
      setGoal(goalBody.goal)
      setPlans(planBody.plans || [])
      setWebTodos(todoBody.items || [])
    }).catch((error) => onError?.(`任务加载失败：${error.message}`))
  }, [currentId, onError, workspace])
  useEffect(() => { load() }, [load])

  const saveTodos = useCallback(async (items) => {
    setWebTodos(items)
    try {
      await postJson(`/api/session/${encodeURIComponent(currentId)}/todos`, { items })
    } catch (error) {
      onError?.(`待办保存失败：${error.message}`)
    }
  }, [currentId, onError])

  if (!currentId) return <div className="empty">先选择会话</div>
  const goalHtml = goal && (goal.text || goal.state) && (
    <div className="goal">
      <b>目标</b> {goal.text}
      <div className="muted">状态 {goal.state || '—'} · tokens {goal.tokens_used || 0}{goal.token_budget ? ` / ${goal.token_budget}` : ''} · 阻塞 {goal.blocked_count || 0} 次</div>
    </div>
  )
  return (
    <div className="pane">
      <div className="pane-toolbar"><button onClick={load}>刷新</button>{data.plan_mode && <span className="chip">计划模式</span>}</div>
      <div className="pane-body scroll">
        {goalHtml}
        {plans.length > 0 && <div className="group">计划</div>}
        {plans.map((plan) => {
          const steps = plan.plan && Array.isArray(plan.plan.steps) ? plan.plan.steps : []
          const done = steps.filter((step) => step.status === 'completed' || step.status === 'done').length
          const status = plan.plan?.status || ''
          return (
            <details key={plan.id} className="plan">
              <summary>
                <span>▤ {plan.plan?.title || plan.id}</span>
                <span className="muted">{status ? `${status} · ` : ''}{steps.length ? `${done}/${steps.length} 步` : ''}</span>
              </summary>
              {steps.length > 0
                ? steps.map((step, index) => {
                    const stepStatus = step.status || ''
                    const finished = stepStatus === 'completed' || stepStatus === 'done'
                    const running = stepStatus === 'in_progress' || stepStatus === 'running'
                    return (
                      <div key={index} className={`row todo ${finished ? 'done' : ''}`}>
                        <span>{finished ? '✔' : running ? '▶' : '○'} {step.description || step.title || step.id || `第 ${index + 1} 步`}</span>
                        <span className="muted">{stepStatus}</span>
                      </div>
                    )
                  })
                : <div className="muted" style={{ padding: '4px 8px' }}>该计划没有步骤明细</div>}
            </details>
          )
        })}
        <div className="group">当前待办（可编辑 · 下一条消息会带给 agent）</div>
        {webTodos.map((todo, index) => (
          <div key={index} className={`row todo ${todo.done ? 'done' : ''}`}>
            <label className="todo-check">
              <input
                type="checkbox"
                checked={Boolean(todo.done)}
                onChange={() => saveTodos(webTodos.map((item, i) => (i === index ? { ...item, done: !item.done } : item)))}
              />
              <span>{todo.text}</span>
            </label>
            <button className="ghost" title="删除" onClick={() => saveTodos(webTodos.filter((_, i) => i !== index))}>×</button>
          </div>
        ))}
        {webTodos.length === 0 && <div className="muted" style={{ padding: '2px 8px 6px' }}>还没有待办，添加一条试试</div>}
        <div className="todo-add">
          <input
            value={newTodo}
            placeholder="添加待办，Enter 保存"
            onChange={(event) => setNewTodo(event.target.value)}
            onKeyDown={(event) => {
              if (event.key !== 'Enter') return
              event.preventDefault()
              const text = newTodo.trim()
              if (!text) return
              saveTodos([...webTodos, { text, done: false }])
              setNewTodo('')
            }}
          />
          <button onClick={() => {
            const text = newTodo.trim()
            if (!text) return
            saveTodos([...webTodos, { text, done: false }])
            setNewTodo('')
          }}>添加</button>
        </div>

        {data.todos.length > 0 && <div className="group">历史 TodoWrite（只读）</div>}
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

function AttachmentsPane({ workspace, onError }) {
  const [data, setData] = useState({ items: [], pastes: [], uploads: [] })
  const [preview, setPreview] = useState(null)
  const [uploading, setUploading] = useState(false)
  const inputRef = useRef(null)
  const load = useCallback(() => {
    api(withWorkspace('/api/attachments', workspace)).then(setData).catch((error) => onError?.(`附件加载失败：${error.message}`))
  }, [onError, workspace])
  useEffect(() => { load() }, [load])

  const openPaste = useCallback(async (path) => {
    try {
      const file = await api(withWorkspace(`/api/workspace/file?path=${encodeURIComponent(path)}`, workspace))
      setPreview({ kind: 'text', path, content: file.content || '' })
    } catch (error) {
      onError?.(`读取粘贴内容失败：${error.message}`)
    }
  }, [onError, workspace])

  const upload = useCallback(async (files) => {
    const file = files?.[0]
    if (!file) return
    setUploading(true)
    try {
      const response = await fetch(
        withToken(withWorkspace('/api/upload?name=' + encodeURIComponent(file.name), workspace)),
        { method: 'POST', body: file },
      )
      if (!response.ok) throw new Error(`HTTP ${response.status} ${await response.text()}`)
      const body = await response.json()
      load()
      onError?.(null)
      setPreview(file.type.startsWith('image/') ? { kind: 'image', path: body.path } : { kind: 'text', path: body.path, content: `已上传：${body.path}` })
    } catch (error) {
      onError?.(`上传失败：${error.message}`)
    } finally {
      setUploading(false)
      if (inputRef.current) inputRef.current.value = ''
    }
  }, [load, onError])

  if (preview) {
    return (
      <div className="pane">
        <div className="pane-toolbar">
          <button onClick={() => setPreview(null)}>‹ 附件</button>
          <span className="path" title={preview.path}>{preview.path}</span>
        </div>
        <div className="pane-body">
          {preview.kind === 'image'
            ? <img className="preview-image" src={rawUrl(preview.path, workspace)} alt={preview.path} />
            : <pre className="file-text">{preview.content}</pre>}
        </div>
      </div>
    )
  }

  return (
    <div className="pane">
      <div className="pane-toolbar">
        <button onClick={load}>刷新</button>
        <button onClick={() => inputRef.current?.click()} disabled={uploading}>{uploading ? '上传中…' : '上传'}</button>
        <input ref={inputRef} type="file" style={{ display: 'none' }} onChange={(event) => upload(event.target.files)} />
        <span className="hint">{data.items.length} 图 / {data.pastes.length} 粘贴 / {data.uploads.length} 上传</span>
      </div>
      <div className="pane-body scroll">
        {data.items.length > 0 && <div className="group">工作区图片</div>}
        <div className="attach-grid">
          {data.items.map((item) => (
            <button key={item.path} type="button" className="attach" title={item.path} onClick={() => setPreview({ kind: 'image', path: item.path })}>
              <img src={rawUrl(item.path)} alt="" />
              <span>{basename(item.path)}</span>
            </button>
          ))}
        </div>
        {data.uploads.length > 0 && <div className="group">上传（.claw/uploads）</div>}
        {data.uploads.map((item) => (
          <div key={item.path} className="row upload-row">
            <button type="button" className="link" onClick={() => (item.image ? setPreview({ kind: 'image', path: item.path }) : openPaste(item.path))}>
              {item.name}
            </button>
            <span className="muted">{Math.max(1, Math.round((item.size || 0) / 1024))}K</span>
            <button className="ghost" title="复制引用路径" onClick={() => navigator.clipboard?.writeText('@' + item.path)}>复制路径</button>
          </div>
        ))}
        {data.pastes.length > 0 && <div className="group">粘贴缓存</div>}
        {data.pastes.map((item) => (
          <button key={item.path} type="button" className="row" onClick={() => openPaste(item.path)}>
            <span>{basename(item.path)}</span><span className="muted">{Math.max(1, Math.round((item.size || 0) / 1024))}K</span>
          </button>
        ))}
        {data.items.length === 0 && data.pastes.length === 0 && data.uploads.length === 0 && <div className="empty">没有附件，点“上传”添加</div>}
      </div>
    </div>
  )
}

function TerminalPane() {
  const [status, setStatus] = useState('未连接')
  const [fullscreen, setFullscreen] = useState(false)
  const hostRef = useRef(null)
  const termRef = useRef(null)
  const fitRef = useRef(null)
  const socketRef = useRef(null)

  const sendJson = useCallback((payload) => {
    const socket = socketRef.current
    if (socket?.readyState === 1) socket.send(JSON.stringify(payload))
  }, [])

  // 尺寸以 xterm 拟合出的 cols/rows 为准（字符网格），不再按像素估算。
  const pushResize = useCallback(() => {
    const fit = fitRef.current
    const term = termRef.current
    if (!fit || !term) return
    try {
      fit.fit()
    } catch (_) {
      return
    }
    sendJson({ type: 'resize', cols: term.cols, rows: term.rows })
  }, [sendJson])

  // 终端只初始化一次：xterm 实例随渲染重建会丢掉滚屏缓冲区与光标状态。
  useEffect(() => {
    const host = hostRef.current
    if (!host) return
    const term = new Terminal({
      cursorBlink: true,
      fontFamily: 'Consolas, "Cascadia Mono", monospace',
      fontSize: 13,
      scrollback: 5000,
    })
    const fit = new FitAddon()
    term.loadAddon(fit)
    term.open(host)
    termRef.current = term
    fitRef.current = fit
    try {
      fit.fit()
    } catch (_) {}

    const socket = new WebSocket(wsUrl('/ws/terminal'))
    socket.binaryType = 'arraybuffer'
    socket.onopen = () => {
      setStatus('已连接')
      setTimeout(pushResize, 120)
    }
    // 原始字节直通 xterm：由它解析 ANSI/颜色/光标/清屏，不再丢掉控制序列，
    // 这样 vim/htop/top 这类全屏程序才能正常工作。
    socket.onmessage = (event) => {
      if (typeof event.data === 'string') term.write(event.data)
      else term.write(new Uint8Array(event.data))
    }
    socket.onclose = () => setStatus('已断开')
    socket.onerror = () => setStatus('连接失败')
    socketRef.current = socket

    // 键盘输入直通 PTY：方向键、Tab 补全、Ctrl+C 都走 xterm 的按键编码。
    const disposable = term.onData((data) => sendJson({ type: 'input', data }))

    const observer = new ResizeObserver(() => pushResize())
    observer.observe(host)
    window.addEventListener('resize', pushResize)
    return () => {
      disposable.dispose()
      observer.disconnect()
      window.removeEventListener('resize', pushResize)
      socket.close()
      term.dispose()
      termRef.current = null
      fitRef.current = null
    }
  }, [pushResize])

  useEffect(() => {
    if (fullscreen) setTimeout(pushResize, 80)
  }, [fullscreen, pushResize])

  useEffect(() => {
    const onKey = (event) => { if (event.key === 'Escape') setFullscreen(false) }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [])

  const send = (data) => sendJson({ type: 'input', data })
  return (
    <div className={`pane ${fullscreen ? 'term-fullscreen' : ''}`}>
      <div className="pane-toolbar">
        <span className="hint">{status}</span>
        <span className="spacer" />
        <button onClick={() => termRef.current?.clear()}>清屏</button>
        <button onClick={() => send('\u0003')}>Ctrl+C</button>
        <button onClick={() => setFullscreen((value) => !value)}>{fullscreen ? '退出全屏' : '全屏'}</button>
      </div>
      <div className="term-host" ref={hostRef} />
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
