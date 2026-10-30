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
  // 未选中会话时用户手选的目标工作区：只记意图、不立刻建会话，
  // 等用户真正发出第一条消息时再在该工作区新建。
  const [draftWorkspace, setDraftWorkspace] = useState('')
  // running 是 React state，重渲染前第二次 Enter 仍会进 send：用同步闸门挡住，
  // 否则第二个 prompt 会在第一个 turn 运行时撞工作区锁，弹出误导性错误。
  const sendLockRef = useRef(false)
  // 最近一次提交的原文：流式失败时把原文回填输入框，并提供「重试」动作，
  // 避免错误把用户刚打的字连同上下文一起丢掉。
  const lastPromptRef = useRef('')
  // SSE 订阅 effect 不能依赖 send（每次渲染都是新闭包），用 ref 拿最新实现。
  const sendRef = useRef(null)
  const [messages, setMessages] = useState([])
  const [optimistic, setOptimistic] = useState(null)
  const [running, setRunning] = useState(false)
  // SSE 回调里读 running 会拿到创建闭包时的旧值（该 effect 只依赖 currentId/push），
  // 用 ref 同步最新运行态，供「重试」动作判断本轮是否仍在跑。
  const runningRef = useRef(false)
  runningRef.current = running
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
  const [voiceDiscussionOpen, setVoiceDiscussionOpen] = useState(false)
  const [webSettings, setWebSettings] = useState({ theme: 'dark', permission: 'workspace-write' })
  const [meta, setMeta] = useState({ version: '', git_sha: '', workspace: '' })
  const { toasts, push } = useToasts()
  // 文件面板「引用选区」→ 输入框的通道。带时间戳，便于同一引用重复插入。
  const [pendingRef, setPendingRef] = useState(null)
  // 语音讨论「结束并整理」→ 输入框的通道。整理结果只填入待编辑，绝不直接发送；
  // 语音转写难免有错字，直接发出去用户没有任何纠正机会（这正是本通道存在的原因）。
  const [pendingDraft, setPendingDraft] = useState(null)
  // 稳定的错误回调：面板的 load 依赖它，若每次渲染换新函数会触发无限重取。
  const panelError = useCallback((message) => {
    if (message) push(message, 'error')
  }, [push])

  const currentId = current?.id || null
  // 「当前工作区」在没有选中会话时取用户手选的目标工作区，其次退回服务端默认工作区。
  const activeWorkspace = current?.workspace || draftWorkspace || meta.workspace || ''

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
    // 无会话时输入会自动新建会话并立刻发消息：这条待发消息带着目标 sessionId，
    // 不能因为 currentId 刚切换到它就被清掉，否则用户刚打的字会闪一下消失。
    setOptimistic((pending) => (pending && pending.sessionId === currentId ? pending : null))
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
        else if (data.kind === 'error') {
          // 可恢复错误（传输层已做退避重试、runtime 还可能再恢复一次）不清屏：
          // 清空已流式内容会让用户误以为整轮作废，而且恢复成功后内容会从空开始。
          if (!data.recoverable) {
            setLive(''); setLiveThinking(''); setLiveTools([])
            // 原文回填输入框，用户可直接重发或修改后重发。
            if (lastPromptRef.current) setPendingDraft(lastPromptRef.current)
          }
          const retryText = lastPromptRef.current
          push(`流式错误${data.recoverable ? '（可重试）' : ''}：${data.message}`, 'error',
            retryText ? {
              label: '重试',
              // 可恢复错误时本轮可能还在跑，重试会撞会话锁；等 done 后再允许。
              run: () => { if (!runningRef.current) sendRef.current?.(retryText, currentId) },
            } : null)
        }
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

  // 返回新建的会话（失败返回 null）。发送路径需要在「没有选中会话」时自动建会话，
  // 并立刻用返回的 id 发消息 —— 不能依赖 React state 在下一帧才更新。
  const createSession = useCallback(async (workspace, options = {}) => {
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
      if (!options.silent) push('已新建会话')
      return session
    } catch (error) {
      push(`新建会话失败：${error.message}`, 'error')
      return null
    }
  }, [activeWorkspace, push, selectSession])

  const startInFolder = useCallback((path) => {
    setWorkspacePickerOpen(false)
    createSession(path)
  }, [createSession])

  // 没有选中会话时，底栏切换工作区只记「目标工作区」，不立刻建空会话 ——
  // 用户此时往往只是想换个地方开新任务，先建一个空会话会白占一条历史。
  const pickTargetWorkspace = useCallback((path) => {
    setDraftWorkspace(path)
    push(`已选择工作区：${basename(path)}，直接输入即可发送`)
  }, [push])

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

  // 没有选中会话时也能直接发消息：自动在当前工作区新建一个会话再发。
  // 「先建会话才能输入」是最常见的操作摩擦——用户想说的是话，不是先做一次管理动作。
  // retrySessionId：重试时复用已建好的会话，避免重试路径再建一个空会话。
  const send = useCallback(async (text, retrySessionId) => {
    if (sendLockRef.current) return
    const trimmed = text.trim()
    if (!trimmed) return
    lastPromptRef.current = trimmed
    sendLockRef.current = true
    try {
      let sessionId = retrySessionId || currentId
      if (!sessionId) {
        // 先置 running：建会话是异步的，期间再按一次 Enter 会再建一个会话。
        setRunning(true)
        const created = await createSession(activeWorkspace, { silent: true })
        if (!created) { setRunning(false); return }
        sessionId = created.id
      }
      setLive('')
      setLiveThinking('')
      setLiveTools([])
      // 带上 sessionId：新会话的订阅 effect 会立刻跑一次，不能把刚输入的内容清掉。
      setOptimistic({ text: trimmed, at: Date.now(), sessionId })
      setRunning(true)
      try {
        await postJson(`/api/session/${encodeURIComponent(sessionId)}/prompt`, {
          text: trimmed,
          permission: webSettings.permission || 'workspace-write',
          model: webSettings.model || undefined,
        })
      } catch (error) {
        setOptimistic(null)
        setRunning(false)
        push(`发送失败：${error.message}`, 'error', { label: '重试', run: () => send(text, sessionId) })
      }
    } finally {
      sendLockRef.current = false
    }
  }, [activeWorkspace, createSession, currentId, push, webSettings.model, webSettings.permission])
  sendRef.current = send

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

  // 语音讨论不是任务，也不该是 agent 会话：
  // 它只需要一个纯对话通道（/api/voice/chat，无工具、不落盘），
  // 因此这里不再创建 session、不切 currentId、不动主会话的任何状态。
  // 打开后主界面照旧，讨论内容只活在 VoiceDiscussion 组件内存里。
  const openVoiceDiscussion = useCallback(() => {
    if (running) { push('请等待当前回复结束后再开始语音讨论', 'error'); return }
    setVoiceDiscussionOpen(true)
  }, [push, running])

  const closeVoiceDiscussion = useCallback(() => {
    setVoiceDiscussionOpen(false)
  }, [])

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
        workspaces={workspaces}
        sessionModel={current?.model || ''}
        onVoiceDiscussion={openVoiceDiscussion}
        onNewInWorkspace={startInFolder}
        onPickWorkspace={pickTargetWorkspace}
        onOpenWorkspacePicker={() => setWorkspacePickerOpen(true)}
        stats={current?.stats}
        pendingRef={pendingRef}
        onRefUsed={setPendingRef}
        pendingDraft={pendingDraft}
        onDraftUsed={setPendingDraft}
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
      onReference={setPendingRef}
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
      {voiceDiscussionOpen && (
        <VoiceDiscussion
          model={webSettings.model}
          onClose={closeVoiceDiscussion}
          onError={(message) => push(message, 'error')}
          onDraft={setPendingDraft}
        />
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
  // 侧栏项目组默认全部收起：首次打开只列出项目本身，便于先选工作区再看会话。
  // 这里只记录用户手动「展开」过的项目（空集 = 全收起），比记录收起集合少一次
  // 依赖 sessions 异步加载的初始化时序。搜索时忽略收起状态，否则命中的会话会被
  // 藏在收起的分组里、搜索看起来像失效。
  const [expandedGroups, setExpandedGroups] = useState(() => new Set())
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
    setExpandedGroups((current) => {
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
              // 搜索时强制展开：否则命中的会话藏在收起的项目里，搜索看起来像失效。
              const isExpanded = expandedGroups.has(project.path) || Boolean(query)
              const active = project.path === activeWorkspace
              return (
                <div className={`project ${active ? 'active-workspace' : ''}`} key={project.path}>
                  <div className="project-head" title={project.path}>
                    <button className="project-toggle" onClick={() => toggleGroup(project.path)}>
                      {isExpanded ? '▾' : '▸'} 📁 {project.label}
                      <span className="project-count">{project.count}</span>
                    </button>
                    <button
                      className="project-new"
                      title={`在 ${project.label} 新建任务`}
                      onClick={() => onNewIn(project.path)}
                    >＋</button>
                  </div>
                  {isExpanded && project.sessions.map((session) => (
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
            <div>直接输入即可发送 —— 未选会话时会在当前工作区新建任务。</div>
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

/* ---------------- 输入区（Trae 风格） ---------------- */

const MODEL_PRESETS = [
  { id: '', label: '默认模型', hint: '跟随工作区配置（当前 Trae）' },
  { id: 'trae/deepseek-v4.1-flash', label: 'Trae · DeepSeek-V4.1-Flash', badge: '0.06', hint: 'Trae 内置额度，推荐' },
  { id: 'deepseek-v4-flash', label: 'DeepSeek-V4-Flash（官方 API）', hint: '官方 API，需要 DeepSeek 余额' },
  { id: 'deepseek-v4-pro', label: 'DeepSeek-V4-Pro（官方 API）', hint: '官方 API，质量优先' },
  { id: 'deepseek-chat', label: 'DeepSeek-Chat（官方 API）', hint: '官方 API，通用对话' },
  { id: 'deepseek-reasoner', label: 'DeepSeek-Reasoner（官方 API）', hint: '官方 API，深度思考' },
]
const MODEL_LOOKUP = Object.fromEntries(MODEL_PRESETS.filter((item) => item.id).map((item) => [item.id, item]))

const PERMISSION_PRESETS = [
  { id: 'read-only', label: '只读', hint: '仅查看文件与只读命令', tone: 'muted' },
  { id: 'workspace-write', label: '工作区可写', hint: '可修改工作区，敏感操作需确认', tone: 'blue' },
  { id: 'danger-full-access', label: '完全访问', hint: '不再逐次确认，请谨慎使用', tone: 'orange' },
]
const PERMISSION_LOOKUP = Object.fromEntries(PERMISSION_PRESETS.map((item) => [item.id, item]))

const CAP_ICONS = {
  files: 'folder',
  diff: 'diff',
  tasks: 'check',
  attachments: 'paperclip',
  terminal: 'terminal',
  settings: 'settings',
}

const ICONS = {
  plus: <><path d="M12 5v14M5 12h14" /></>,
  chevron: <path d="m6 9.5 6 6 6-6" />,
  shield: <path d="M12 3.2 19 5.8v5.7c0 4.3-3 7.6-7 9.3-4-1.7-7-5-7-9.3V5.8L12 3.2Z" />,
  mic: <><rect x="9.2" y="3" width="5.6" height="10.5" rx="2.8" /><path d="M5.8 11.5a6.2 6.2 0 0 0 12.4 0M12 17.7V21" /></>,
  wave: <><path d="M3.5 10v4M7 6.5v11M10.5 9v6M14 4.5v15M17.5 10v4" /><circle cx="20.5" cy="12" r="1" fill="currentColor" stroke="none" /></>,
  arrowUp: <><path d="M12 19V5" /><path d="m5.5 11.5 6.5-6.5 6.5 6.5" /></>,
  stop: <rect x="6.5" y="6.5" width="11" height="11" rx="2.5" fill="currentColor" stroke="none" />,
  laptop: <><rect x="4.5" y="5" width="15" height="10.5" rx="1.8" /><path d="M2.5 18.5h19" /></>,
  folder: <path d="M3 7.2c0-.9.7-1.6 1.6-1.6h4.3l2 2.4h7.7c.9 0 1.6.7 1.6 1.6v7.6c0 .9-.7 1.6-1.6 1.6H4.6c-.9 0-1.6-.7-1.6-1.6V7.2Z" />,
  paperclip: <path d="m20 11.2-8.1 8.1a4.8 4.8 0 0 1-6.8-6.8l8.6-8.6a3.2 3.2 0 0 1 4.5 4.5l-8.2 8.2a1.7 1.7 0 0 1-2.4-2.4l7.4-7.4" />,
  doc: <><rect x="5" y="3" width="14" height="18" rx="2" /><path d="M8.5 8h7M8.5 12h7M8.5 16h4.5" /></>,
  search: <><circle cx="11" cy="11" r="6.5" /><path d="m20 20-3.6-3.6" /></>,
  check: <path d="m5 12.5 4.5 4.5L19 7" />,
  terminal: <><path d="m5 8 4 4-4 4M12.5 16H19" /></>,
  settings: <><path d="M4 7h9M18 7h2M4 17h2M10 17h10" /><circle cx="16" cy="7" r="2" /><circle cx="8" cy="17" r="2" /></>,
  diff: <><path d="M6 5v14M12 7h6M12 12h6M12 17h6" /><circle cx="6" cy="5" r="1.5" fill="currentColor" stroke="none" /></>,
  cloud: <path d="M7 18h9.5a3.5 3.5 0 0 0 .6-6.9A5.5 5.5 0 0 0 6.3 9.8 4.1 4.1 0 0 0 7 18Z" />,
  x: <><path d="M6 6l12 12M18 6 6 18" /></>,
  pause: <><path d="M8 5v14M16 5v14" /></>,
  volume: <><path d="M4 9.5v5h3l4 3.5V6L7 9.5H4Z" /><path d="M15 9a4 4 0 0 1 0 6M17.5 6.5a7.5 7.5 0 0 1 0 11" /></>,
  volumeOff: <><path d="M4 9.5v5h3l4 3.5V6L7 9.5H4Z" /><path d="m15 9 6 6M21 9l-6 6" /></>,
}

function speechText(text) {
  return String(text || '')
    .replace(/```[\s\S]*?```/g, ' 代码块 ')
    .replace(/`([^`]+)`/g, '$1')
    .replace(/https?:\/\/\S+/g, ' 链接 ')
    .replace(/[*_>#~-]+/g, ' ')
    .replace(/\s+/g, ' ')
    .trim()
    .slice(0, 1200)
}

const VOICE_BAR_COUNT = 100
const VOICE_FLAT_BARS = Array.from({ length: VOICE_BAR_COUNT }, () => 0.06)

function voiceClock(seconds) {
  const value = Math.max(0, Number(seconds) || 0)
  return `${Math.floor(value / 60)}:${String(value % 60).padStart(2, '0')}`
}

function Icon({ name, size = 18, className = '' }) {
  return (
    <svg
      className={className}
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.7"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
    >
      {ICONS[name] || null}
    </svg>
  )
}

function Popover({ className = '', children }) {
  return <div className={`popover ${className}`} role="menu">{children}</div>
}

function PopoverItem({ icon, glyph, label, hint, badge, selected = false, iconTone = '', onClick, disabled = false }) {
  return (
    <button type="button" className={`popover-item ${selected ? 'selected' : ''}`} onClick={onClick} disabled={disabled} role="menuitem">
      <span className={`popover-icon ${iconTone}`}>{glyph ? <span className="popover-glyph">{glyph}</span> : icon ? <Icon name={icon} size={15} /> : null}</span>
      <span className="popover-text">
        <span className="popover-label">{label}{badge ? <span className="model-badge">{badge}</span> : null}</span>
        {hint ? <span className="popover-hint">{hint}</span> : null}
      </span>
      {selected ? <Icon name="check" size={14} className="popover-check" /> : null}
    </button>
  )
}

function resolveModelLabel(model, sessionModel) {
  const id = String(model || sessionModel || '').trim()
  if (!id) return { label: '默认模型', badge: '' }
  const preset = MODEL_LOOKUP[id]
  if (preset) return { label: preset.label, badge: preset.badge || '' }
  return { label: id, badge: '' }
}

function Composer({ running, onSend, onStop, onQuickAction, onError, currentId, permission, onPermission, model, onModel, workspace, workspaces = [], sessionModel = '', onVoiceDiscussion, onNewInWorkspace, onPickWorkspace, onOpenWorkspacePicker, stats, pendingRef, onRefUsed, pendingDraft, onDraftUsed }) {
  const [value, setValue] = useState('')
  const [files, setFiles] = useState([])
  const [suggest, setSuggest] = useState(null)
  const [cursor, setCursor] = useState(0)
  const [attachments, setAttachments] = useState([])
  const [uploading, setUploading] = useState(false)
  const [openMenu, setOpenMenu] = useState(null)
  const [footerMenu, setFooterMenu] = useState(null)
  const [modelDraft, setModelDraft] = useState(model || '')
  const [dragging, setDragging] = useState(false)
  const [voiceActive, setVoiceActive] = useState(false)
  const [voiceElapsed, setVoiceElapsed] = useState(0)
  const [waveBars, setWaveBars] = useState(VOICE_FLAT_BARS)
  const ref = useRef(null)
  const areaRef = useRef(null)
  const uploadRef = useRef(null)
  const rootRef = useRef(null)
  const filesLoading = useRef(false)
  const speechRef = useRef(null)
  const voiceActiveRef = useRef(false)
  const voiceBaseRef = useRef('')
  const voiceFinalRef = useRef('')
  const voiceInterimRef = useRef('')
  const sessionFinalBaseRef = useRef('')
  const voiceStartedRef = useRef(0)
  const waveTimerRef = useRef(0)
  const elapsedTimerRef = useRef(0)
  const restartTimerRef = useRef(0)
  const mediaStreamRef = useRef(null)
  const audioCtxRef = useRef(null)
  const analyserRef = useRef(null)

  useEffect(() => { ref.current = value }, [value])

  const focusEnd = () => {
    const area = areaRef.current
    if (!area) return
    area.focus()
    area.selectionStart = area.selectionEnd = area.value.length
  }

  const insert = (text) => {
    setValue((previous) => {
      if (!previous.trim()) return text
      return /[\s]$/.test(previous) ? previous + text : previous + ' ' + text
    })
    requestAnimationFrame(focusEnd)
  }

  // 每个会话一份草稿：切换会话时不丢正在输入的内容。
  const draftKey = currentId ? `claw-draft:${currentId}` : ''
  useEffect(() => {
    if (!draftKey) { setValue(''); return }
    try { setValue(sessionStorage.getItem(draftKey) || '') } catch (_) { setValue('') }
  }, [draftKey])
  useEffect(() => {
    if (!draftKey) return
    try {
      if (value) sessionStorage.setItem(draftKey, value)
      else sessionStorage.removeItem(draftKey)
    } catch (_) {}
  }, [draftKey, value])

  // 文件面板点「引用选区」后，把 `@path:12-40` 追加到输入框并把光标放到末尾。
  useEffect(() => {
    if (!pendingRef) return
    setValue((previous) => (previous.trim() ? previous.replace(/\s*$/, '') + ' ' : '') + pendingRef)
    onRefUsed?.(null)
    requestAnimationFrame(focusEnd)
  }, [pendingRef, onRefUsed])

  // 语音讨论「结束并整理」后，把整理出的 prompt 填入输入框**等待编辑**，不直接发送。
  // 语音转写难免有错字/断句错误，直接发出去用户没有任何纠正机会。
  // 已有草稿时追加而非覆盖，避免吞掉用户已输入的内容。
  useEffect(() => {
    if (!pendingDraft) return
    setValue((previous) => (previous.trim() ? `${previous.replace(/\s*$/, '')}\n\n${pendingDraft}` : pendingDraft))
    onDraftUsed?.(null)
    requestAnimationFrame(focusEnd)
  }, [pendingDraft, onDraftUsed])

  useEffect(() => { setModelDraft(model || '') }, [model])

  // 高度跟随内容：历史引用、上传、@ 选择都会改写 value，不能只依赖 onChange。
  useEffect(() => {
    const node = areaRef.current
    if (!node) return
    node.style.height = 'auto'
    node.style.height = `${Math.min(220, Math.max(72, node.scrollHeight))}px`
  }, [value])

  useEffect(() => {
    if (suggest?.kind !== 'file' || files.length || filesLoading.current) return
    filesLoading.current = true
    api('/api/workspace/files')
      .then((body) => setFiles(body.files || []))
      .catch(() => {})
      .finally(() => { filesLoading.current = false })
  }, [suggest, files.length])

  // 点击外部或按 Esc 收起下拉浮层。
  useEffect(() => {
    if (!openMenu && !footerMenu) return undefined
    const onDown = (event) => {
      if (!rootRef.current?.contains(event.target)) { setOpenMenu(null); setFooterMenu(null) }
    }
    const onKey = (event) => { if (event.key === 'Escape') { setOpenMenu(null); setFooterMenu(null) } }
    document.addEventListener('mousedown', onDown)
    document.addEventListener('keydown', onKey)
    return () => {
      document.removeEventListener('mousedown', onDown)
      document.removeEventListener('keydown', onKey)
    }
  }, [openMenu, footerMenu])

  useEffect(() => () => {
    voiceActiveRef.current = false
    if (restartTimerRef.current) clearTimeout(restartTimerRef.current)
    if (elapsedTimerRef.current) clearInterval(elapsedTimerRef.current)
    if (waveTimerRef.current) clearInterval(waveTimerRef.current)
    try { speechRef.current?.stop?.() } catch (_) {}
    if (mediaStreamRef.current) {
      mediaStreamRef.current.getTracks().forEach((track) => track.stop())
      mediaStreamRef.current = null
    }
    if (audioCtxRef.current) {
      try { audioCtxRef.current.close() } catch (_) {}
      audioCtxRef.current = null
    }
  }, [])

  const slashMatches = useMemo(() => {
    if (!value.startsWith('/') || value.includes(' ')) return []
    return QUICK_ACTIONS.filter((action) => action.cmd.startsWith(value))
  }, [value])

  const fileFragment = useMemo(() => {
    const match = /(^|\s)@([^\s]*)$/.exec(value)
    return match ? match[2] : null
  }, [value])

  const fileMatches = useMemo(() => {
    if (fileFragment === null) return null
    const query = fileFragment.toLowerCase()
    return files.filter((file) => file.path.toLowerCase().includes(query)).slice(0, 8)
  }, [fileFragment, files])

  useEffect(() => {
    if (slashMatches.length) setSuggest({ kind: 'slash', items: slashMatches })
    else if (fileMatches !== null) setSuggest({ kind: 'file', items: fileMatches })
    else setSuggest(null)
    setCursor(0)
  }, [slashMatches, fileMatches])

  const choose = (item) => {
    if (suggest?.kind === 'slash') {
      onQuickAction(item.run)
      setValue('')
    } else if (item.upload) {
      uploadRef.current?.click()
    } else if (suggest?.kind === 'file') {
      setValue((previous) => previous.replace(/@([^\s]*)$/, '@' + item.path + ' '))
    }
    setSuggest(null)
    requestAnimationFrame(focusEnd)
  }

  const submit = () => {
    if (running || voiceActiveRef.current) return
    const text = value
    if (!text.trim()) return
    setSuggest(null)
    setOpenMenu(null)
    setFooterMenu(null)
    setValue('')
    // 没有选中会话时不再拦下用户：交给上层自动在当前工作区新建会话再发送。
    onSend(text)
  }

  const submitCustomModel = () => {
    const next = modelDraft.trim()
    if (next !== (model || '')) onModel(next)
    setOpenMenu(null)
  }

  const onKeyDown = (event) => {
    if (voiceActiveRef.current) {
      if (event.key === 'Escape') { event.preventDefault(); finishVoice(true) }
      return
    }
    if (suggest) {
      const count = suggest.items.length + (suggest.kind === 'file' ? 1 : 0)
      if (event.key === 'ArrowDown') { event.preventDefault(); setCursor((index) => (index + 1) % count); return }
      if (event.key === 'ArrowUp') { event.preventDefault(); setCursor((index) => (index - 1 + count) % count); return }
      if (event.key === 'Tab' || (event.key === 'Enter' && !event.nativeEvent.isComposing)) {
        event.preventDefault()
        if (suggest.kind === 'file') {
          if (cursor === suggest.items.length) choose({ upload: true })
          else choose(suggest.items[cursor])
        } else {
          choose(suggest.items[cursor])
        }
        return
      }
      if (event.key === 'Escape') { event.preventDefault(); setSuggest(null); return }
    }
    if (event.key === 'Escape' && (openMenu || footerMenu)) { setOpenMenu(null); setFooterMenu(null); return }
    // 中文输入法回车确认候选词时不能提交。
    if (event.key === 'Enter' && !event.shiftKey && !event.ctrlKey && !event.nativeEvent.isComposing) {
      event.preventDefault()
      submit()
    }
  }

  const uploadFile = async (list) => {
    const items = Array.from(list || [])
    if (!items.length) return
    setUploading(true)
    try {
      for (const file of items) {
        const response = await fetch(withToken(withWorkspace('/api/upload?name=' + encodeURIComponent(file.name), workspace)), { method: 'POST', body: file })
        if (!response.ok) throw new Error(`HTTP ${response.status}`)
        const body = await response.json()
        setAttachments((previous) => [...previous, { name: file.name, path: body.path, size: file.size }])
        setValue((previous) => (previous.trim() ? previous.replace(/\s*$/, '') + '\n' : '') + '@' + body.path + ' ')
      }
      requestAnimationFrame(focusEnd)
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

  const onPaste = (event) => {
    const pasted = Array.from(event.clipboardData?.files || [])
    if (!pasted.length) return
    event.preventDefault()
    uploadFile(pasted)
  }

  const stopWaveform = () => {
    if (waveTimerRef.current) { clearInterval(waveTimerRef.current); waveTimerRef.current = 0 }
    if (mediaStreamRef.current) {
      mediaStreamRef.current.getTracks().forEach((track) => track.stop())
      mediaStreamRef.current = null
    }
    if (audioCtxRef.current) {
      try { audioCtxRef.current.close() } catch (_) {}
      audioCtxRef.current = null
    }
    analyserRef.current = null
    setWaveBars(VOICE_FLAT_BARS)
  }

  // 声波优先取真实麦克风音量；拿不到权限时退化为合成波形，不阻断转写。
  const startWaveform = async () => {
    let analyser = null
    let data = null
    try {
      if (!navigator.mediaDevices?.getUserMedia) throw new Error('no media devices')
      const stream = await navigator.mediaDevices.getUserMedia({ audio: true })
      if (!voiceActiveRef.current) {
        stream.getTracks().forEach((track) => track.stop())
        return
      }
      mediaStreamRef.current = stream
      const AudioCtx = window.AudioContext || window.webkitAudioContext
      if (!AudioCtx) throw new Error('no audio context')
      const context = new AudioCtx()
      audioCtxRef.current = context
      const source = context.createMediaStreamSource(stream)
      analyser = context.createAnalyser()
      analyser.fftSize = 128
      analyser.smoothingTimeConstant = 0.75
      source.connect(analyser)
      analyserRef.current = analyser
      data = new Uint8Array(analyser.frequencyBinCount)
    } catch (_) {
      analyser = null
      data = null
    }
    const startedAt = Date.now()
    waveTimerRef.current = setInterval(() => {
      if (!voiceActiveRef.current) return
      let bars
      if (analyser && data) {
        analyser.getByteFrequencyData(data)
        bars = Array.from({ length: VOICE_BAR_COUNT }, (_, index) => {
          const bin = Math.min(data.length - 1, Math.floor((index / VOICE_BAR_COUNT) * data.length * 0.72))
          const peak = Math.max(data[bin], data[Math.min(data.length - 1, bin + 2)] || 0)
          return Math.min(1, Math.max(0.06, (peak / 255) * 1.8))
        })
      } else {
        const tick = (Date.now() - startedAt) / 160
        bars = Array.from({ length: VOICE_BAR_COUNT }, (_, index) => {
          const wave = Math.sin(tick + index * 0.42) * 0.28 + Math.sin(tick * 1.9 + index * 0.13) * 0.22 + 0.38
          const spike = Math.random() < 0.06 ? Math.random() * 0.55 : 0
          return Math.min(1, Math.max(0.06, wave + spike))
        })
      }
      setWaveBars(bars)
    }, 70)
  }

  const startVoiceTimer = () => {
    voiceStartedRef.current = Date.now()
    setVoiceElapsed(0)
    if (elapsedTimerRef.current) clearInterval(elapsedTimerRef.current)
    elapsedTimerRef.current = setInterval(() => {
      if (!voiceActiveRef.current) return
      setVoiceElapsed(Math.floor((Date.now() - voiceStartedRef.current) / 1000))
    }, 250)
  }

  const stopVoiceTimer = () => {
    if (elapsedTimerRef.current) { clearInterval(elapsedTimerRef.current); elapsedTimerRef.current = 0 }
  }

  const finishVoice = (restore) => {
    if (!voiceActiveRef.current) return
    voiceActiveRef.current = false
    setVoiceActive(false)
    if (restartTimerRef.current) { clearTimeout(restartTimerRef.current); restartTimerRef.current = 0 }
    try { speechRef.current?.stop() } catch (_) {}
    speechRef.current = null
    stopVoiceTimer()
    stopWaveform()
    const base = voiceBaseRef.current
    const spoken = (voiceFinalRef.current + voiceInterimRef.current).trim()
    if (restore) {
      setValue(base)
    } else {
      setValue(base && spoken ? base.replace(/\s*$/, '') + ' ' + spoken : base || spoken)
      requestAnimationFrame(focusEnd)
    }
    voiceFinalRef.current = ''
    voiceInterimRef.current = ''
    sessionFinalBaseRef.current = ''
  }

  // 实时语音输入：转写显示在输入框，底栏切换为取消 / 声波 / 计时 / 确认。
  const startVoice = () => {
    if (voiceActiveRef.current) return
    const Recognition = window.SpeechRecognition || window.webkitSpeechRecognition
    if (!Recognition) { onError?.('当前浏览器不支持实时语音输入，请使用 Chrome / Edge'); return }
    voiceActiveRef.current = true
    voiceBaseRef.current = value
    voiceFinalRef.current = ''
    voiceInterimRef.current = ''
    sessionFinalBaseRef.current = ''
    setSuggest(null)
    setOpenMenu(null)
    setFooterMenu(null)
    setVoiceActive(true)
    startVoiceTimer()
    startWaveform()
    const recognition = new Recognition()
    recognition.lang = navigator.language || 'zh-CN'
    recognition.continuous = true
    recognition.interimResults = true
    recognition.onresult = (event) => {
      let finalText = ''
      let interimText = ''
      for (let index = 0; index < event.results.length; index += 1) {
        const result = event.results[index]
        const transcript = result?.[0]?.transcript || ''
        if (result.isFinal) finalText += transcript
        else interimText += transcript
      }
      voiceFinalRef.current = sessionFinalBaseRef.current + finalText
      voiceInterimRef.current = interimText
      const base = voiceBaseRef.current
      const spoken = (voiceFinalRef.current + voiceInterimRef.current).trim()
      setValue(base && spoken ? base.replace(/\s*$/, '') + ' ' + spoken : base || spoken)
    }
    recognition.onerror = (event) => {
      const code = event.error
      if (code === 'aborted' || code === 'no-speech') return
      const messages = {
        'not-allowed': '麦克风权限被拒绝，无法语音输入',
        'service-not-allowed': '麦克风权限被拒绝，无法语音输入',
        'audio-capture': '没有检测到可用的麦克风',
        network: '语音识别服务不可用（需要联网）',
      }
      if (messages[code]) {
        onError?.(messages[code])
        finishVoice(true)
      }
    }
    recognition.onend = () => {
      if (!voiceActiveRef.current) return
      // Chrome 静音后会结束当前识别周期；自动续接，保留已识别文本。
      if (voiceInterimRef.current) {
        voiceFinalRef.current += voiceInterimRef.current
        voiceInterimRef.current = ''
      }
      sessionFinalBaseRef.current = voiceFinalRef.current
      restartTimerRef.current = window.setTimeout(() => {
        if (!voiceActiveRef.current) return
        try { recognition.start() } catch (_) {}
      }, 250)
    }
    speechRef.current = recognition
    try {
      recognition.start()
    } catch (error) {
      onError?.(`语音输入启动失败：${error.message}`)
      finishVoice(true)
    }
  }

  const perm = PERMISSION_LOOKUP[permission] || PERMISSION_PRESETS[1]
  const shownModel = resolveModelLabel(model, sessionModel)
  const canSend = Boolean(value.trim())

  return (
    <form className="composer" ref={rootRef} onSubmit={(event) => { event.preventDefault(); submit() }}>
      {suggest && (
        <div className="menu" role="listbox">
          <div className="menu-head">
            <span>{suggest.kind === 'slash' ? '命令与技能' : '添加上下文'}</span>
            <span className="menu-hint">↑↓ 选择 · Tab 补全 · Esc 关闭</span>
          </div>
          {suggest.items.map((item, position) => {
            const index = position
            return suggest.kind === 'slash' ? (
              <button type="button" key={item.cmd} className={`menu-item ${index === cursor ? 'active' : ''}`} onMouseDown={(event) => { event.preventDefault(); choose(item) }}>
                <b>{item.cmd}</b><span>{item.label}</span>
              </button>
            ) : (
              <button type="button" key={item.path} className={`menu-item ${index === cursor ? 'active' : ''}`} onMouseDown={(event) => { event.preventDefault(); choose(item) }}>
                <b>{item.path}</b><span>{Math.max(1, Math.round((item.size || 0) / 1024))}K</span>
              </button>
            )
          })}
          {suggest.kind === 'file' && suggest.items.length === 0 && (
            <div className="menu-empty">没有匹配的文件</div>
          )}
          {suggest.kind === 'file' && (
            <button
              type="button"
              className={`menu-item ${cursor === suggest.items.length ? 'active' : ''}`}
              onMouseDown={(event) => { event.preventDefault(); choose({ upload: true }) }}
            >
              <b>上传文件…</b><span>图片 / 文档</span>
            </button>
          )}
        </div>
      )}
      {attachments.length > 0 && (
        <div className="composer-attachments">
          {attachments.map((item) => (
            <span key={item.path} className="attachment-chip" title={item.path}>
              <Icon name="paperclip" size={12} />
              <span className="attachment-name">{item.name}</span>
              <button type="button" className="attachment-remove" onClick={() => removeAttachment(item.path)} aria-label={`移除 ${item.name}`}>×</button>
            </span>
          ))}
        </div>
      )}
      <div
        className={`composer-box ${dragging ? 'dragging' : ''}`}
        onClick={(event) => { if (!event.target.closest('button, input, textarea, .popover')) areaRef.current?.focus() }}
        onDragOver={(event) => { event.preventDefault(); setDragging(true) }}
        onDragLeave={() => setDragging(false)}
        onDrop={(event) => { event.preventDefault(); setDragging(false); uploadFile(event.dataTransfer.files) }}
      >
        <textarea
          ref={areaRef}
          value={value}
          rows={1}
          readOnly={voiceActive}
          className={voiceActive ? 'voice-input' : ''}
          placeholder={voiceActive ? '正在聆听…' : currentId ? '@ 添加上下文，/ 使用命令和技能' : '直接输入即可发送，会在当前工作区新建任务'}
          onChange={(event) => setValue(event.target.value)}
          onKeyDown={onKeyDown}
          onPaste={onPaste}
        />
        {dragging && <div className="composer-drop">松开以上传文件</div>}
        {voiceActive ? (
          <div className="composer-bar voice-bar">
            <button type="button" className="voice-btn cancel" onClick={() => finishVoice(true)} title="取消语音输入" aria-label="取消语音输入">
              <Icon name="x" size={18} />
            </button>
            <div className="voice-wave" aria-hidden="true">
              {waveBars.map((level, index) => (
                <span key={index} style={{ height: `${Math.round(3 + level * 25)}px` }} />
              ))}
            </div>
            <span className="voice-time">{voiceClock(voiceElapsed)}</span>
            <button type="button" className="voice-btn confirm" onClick={() => finishVoice(false)} title="确认语音输入" aria-label="确认语音输入">
              <Icon name="check" size={18} />
            </button>
          </div>
        ) : (
        <div className="composer-bar">
          <div className="bar-left">
            <div className="bar-item">
              <button
                type="button"
                className={`icon-btn ${openMenu === 'plus' ? 'active' : ''}`}
                title="添加上下文"
                aria-label="添加上下文"
                onClick={() => setOpenMenu(openMenu === 'plus' ? null : 'plus')}
                disabled={uploading}
              >
                <Icon name="plus" />
              </button>
              {openMenu === 'plus' && (
                <Popover className="left">
                  <PopoverItem icon="paperclip" label="上传文件" hint="图片 / 文档，也可直接粘贴" onClick={() => { setOpenMenu(null); uploadRef.current?.click() }} />
                  <PopoverItem glyph="@" label="引用文件" hint="输入 @ 选择工作区文件" onClick={() => { setOpenMenu(null); insert('@') }} />
                  <PopoverItem glyph="/" label="命令与技能" hint="输入 / 打开快捷命令" onClick={() => { setOpenMenu(null); insert('/') }} />
                  <div className="popover-sep" />
                  <PopoverItem icon="paperclip" label="打开附件面板" onClick={() => { setOpenMenu(null); onQuickAction('attachments') }} />
                </Popover>
              )}
            </div>
            <div className="bar-item">
              <button
                type="button"
                className={`permission-chip tone-${perm.tone} ${openMenu === 'permission' ? 'active' : ''}`}
                title="本轮权限模式"
                onClick={() => setOpenMenu(openMenu === 'permission' ? null : 'permission')}
              >
                <Icon name="shield" size={15} />
                <span>{perm.label}</span>
                <Icon name="chevron" size={12} className="chevron" />
              </button>
              {openMenu === 'permission' && (
                <Popover className="left perm-pop">
                  <div className="popover-head">本轮权限</div>
                  {PERMISSION_PRESETS.map((option) => (
                    <PopoverItem
                      key={option.id}
                      icon="shield"
                      iconTone={`tone-${option.tone}`}
                      label={option.label}
                      hint={option.hint}
                      selected={option.id === permission}
                      onClick={() => { onPermission(option.id); setOpenMenu(null) }}
                    />
                  ))}
                </Popover>
              )}
            </div>
            <div className="bar-item">
              <button
                type="button"
                className={`caps-pill ${openMenu === 'caps' ? 'active' : ''}`}
                title="能力"
                aria-label="能力"
                onClick={() => setOpenMenu(openMenu === 'caps' ? null : 'caps')}
              >
                <span className="caps-brand">
                  <svg viewBox="0 0 24 24" width="15" height="15" aria-hidden="true">
                    <path d="M4 15.4c3-4.6 6.6-6.1 10.7-4.4l-1.3 2.7c-2.2 1.3-4.5 1.7-7 1.1L4 15.4Z" fill="#ffffff" opacity=".95" />
                    <path d="M9.7 6.8c3.9-.2 7.3 1.7 10 5.6-2.3.7-4.4.5-6.4-.5-1.9-1-3.1-2.7-3.6-5.1Z" fill="#ffffff" opacity=".8" />
                  </svg>
                </span>
                <span className="caps-mini"><Icon name="search" size={11} /></span>
                <span className="caps-mini"><Icon name="doc" size={11} /></span>
              </button>
              {openMenu === 'caps' && (
                <Popover className="left caps-pop">
                  <div className="popover-head">能力</div>
                  {QUICK_ACTIONS.map((action) => (
                    <PopoverItem
                      key={action.cmd}
                      icon={CAP_ICONS[action.run]}
                      label={action.label}
                      hint={action.cmd}
                      onClick={() => { setOpenMenu(null); onQuickAction(action.run) }}
                    />
                  ))}
                </Popover>
              )}
            </div>
          </div>
          <span className="bar-spacer" />
          <div className="bar-item model-wrap">
            <button
              type="button"
              className={`model-btn ${openMenu === 'model' ? 'active' : ''}`}
              title="本轮模型"
              onClick={() => { setModelDraft(model || ''); setOpenMenu(openMenu === 'model' ? null : 'model') }}
            >
              <span className="model-name">{shownModel.label}</span>
              {shownModel.badge ? <span className="model-badge">{shownModel.badge}</span> : null}
              <Icon name="chevron" size={12} className="chevron" />
            </button>
            {openMenu === 'model' && (
              <Popover className="right model-pop">
                <div className="popover-head">模型</div>
                {MODEL_PRESETS.map((option) => (
                  <PopoverItem
                    key={option.id || 'default'}
                    label={option.label}
                    badge={option.badge}
                    hint={option.hint}
                    selected={(model || '') === option.id}
                    onClick={() => { onModel(option.id); setOpenMenu(null) }}
                  />
                ))}
                <div className="popover-sep" />
                <div className="model-custom">
                  <input
                    value={modelDraft}
                    placeholder="自定义模型名"
                    onChange={(event) => setModelDraft(event.target.value)}
                    onKeyDown={(event) => {
                      if (event.key === 'Enter' && !event.nativeEvent.isComposing) { event.preventDefault(); submitCustomModel() }
                    }}
                  />
                  <button type="button" onClick={submitCustomModel} disabled={!modelDraft.trim()}>应用</button>
                </div>
              </Popover>
            )}
          </div>
          <button type="button" className="icon-btn" title="语音输入" aria-label="语音输入" onClick={startVoice}>
            <Icon name="mic" />
          </button>
          <button type="button" className="icon-btn" title="语音讨论" aria-label="语音讨论" onClick={onVoiceDiscussion}>
            <Icon name="wave" />
          </button>
          {running
            ? <button type="button" className="send-btn stop" onClick={onStop} title="停止" aria-label="停止"><Icon name="stop" size={16} /></button>
            : <button type="submit" className="send-btn" disabled={!canSend} title={!canSend ? '输入内容后发送（Enter）' : currentId ? '发送（Enter）' : '发送（Enter）· 会在当前工作区新建任务'} aria-label="发送"><Icon name="arrowUp" /></button>}
        </div>
        )}
        <input ref={uploadRef} type="file" multiple style={{ display: 'none' }} onChange={(event) => uploadFile(event.target.files)} />
      </div>
      <div className="composer-footer">
        <div className="footer-item">
          <button type="button" className="footer-btn" onClick={() => setFooterMenu(footerMenu === 'env' ? null : 'env')}>
            <Icon name="laptop" size={16} />
            <span>本地</span>
            <Icon name="chevron" size={12} className="chevron" />
          </button>
          {footerMenu === 'env' && (
            <Popover className="left footer-pop">
              <div className="popover-head">运行环境</div>
              <PopoverItem icon="laptop" label="本地" hint="claw serve 运行在本机" selected onClick={() => setFooterMenu(null)} />
              <PopoverItem icon="cloud" label="远程 / 云端" hint="当前构建未启用" disabled />
            </Popover>
          )}
        </div>
        <div className="footer-item">
          <button type="button" className="footer-btn" onClick={() => setFooterMenu(footerMenu === 'workspace' ? null : 'workspace')}>
            <Icon name="folder" size={16} />
            <span className="workspace-name">{basename(workspace) || '未选择工作区'}</span>
            <Icon name="chevron" size={12} className="chevron" />
          </button>
          {footerMenu === 'workspace' && (
            <Popover className="left footer-pop workspace-pop">
              <div className="popover-head">{currentId ? '工作区 · 切换会新建任务' : '工作区 · 首个消息在此新建任务'}</div>
              {workspaces.map((item) => (
                <PopoverItem
                  key={item.path}
                  icon="folder"
                  label={basename(item.path)}
                  hint={item.path}
                  selected={item.path === workspace}
                  onClick={() => {
                    setFooterMenu(null)
                    if (item.path === workspace) return
                    // 没有选中会话时切换工作区不该建空会话：先只记住目标工作区，
                    // 用户发出第一条消息时再在那个工作区里新建。
                    if (!currentId) onPickWorkspace?.(item.path)
                    else onNewInWorkspace?.(item.path)
                  }}
                />
              ))}
              {workspaces.length === 0 && <div className="popover-empty">还没有工作区</div>}
              <div className="popover-sep" />
              <PopoverItem icon="plus" label="选择其他文件夹…" onClick={() => { setFooterMenu(null); onOpenWorkspacePicker?.() }} />
            </Popover>
          )}
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

/* ---------------- 语音讨论 ---------------- */

// 语音讨论 = 纯对话，不是 agent 任务。
// 后端 /api/voice/chat 不走 LiveCli、不带工具、不写会话文件，
// 讨论历史只存在本组件内存里。这样「陪你理思路」才不会被工具调用打断，
// 也不会出现工具状态识别。
function VoiceDiscussion({ model, onClose, onError, onDraft }) {
  const [phase, setPhase] = useState('listening')
  const [log, setLog] = useState([])
  const [interim, setInterim] = useState('')
  const [elapsed, setElapsed] = useState(0)
  const [waveBars, setWaveBars] = useState(VOICE_FLAT_BARS)
  const [ttsOn, setTtsOn] = useState(true)
  const [busy, setBusy] = useState(false)

  const bodyRef = useRef(null)
  const openRef = useRef(true)
  const listeningRef = useRef(false)
  const recognitionRef = useRef(null)
  const finalRef = useRef('')
  const interimRef = useRef('')
  const sessionBaseRef = useRef('')
  const restartTimerRef = useRef(0)
  const startedRef = useRef(Date.now())
  const elapsedTimerRef = useRef(0)
  const waveTimerRef = useRef(0)
  const mediaStreamRef = useRef(null)
  const audioCtxRef = useRef(null)
  const analyserRef = useRef(null)
  const ttsOnRef = useRef(true)
  const logRef = useRef(log)
  const abortRef = useRef(null)
  logRef.current = log

  const changePhase = (next) => setPhase(next)

  const stopWaveform = () => {
    if (waveTimerRef.current) { clearInterval(waveTimerRef.current); waveTimerRef.current = 0 }
    if (mediaStreamRef.current) {
      mediaStreamRef.current.getTracks().forEach((track) => track.stop())
      mediaStreamRef.current = null
    }
    if (audioCtxRef.current) {
      try { audioCtxRef.current.close() } catch (_) {}
      audioCtxRef.current = null
    }
    analyserRef.current = null
    setWaveBars(VOICE_FLAT_BARS)
  }

  const startWaveform = async () => {
    let analyser = null
    let data = null
    try {
      if (!navigator.mediaDevices?.getUserMedia) throw new Error('no media devices')
      const stream = await navigator.mediaDevices.getUserMedia({ audio: true })
      if (!listeningRef.current) {
        stream.getTracks().forEach((track) => track.stop())
        return
      }
      mediaStreamRef.current = stream
      const AudioCtx = window.AudioContext || window.webkitAudioContext
      if (!AudioCtx) throw new Error('no audio context')
      const context = new AudioCtx()
      audioCtxRef.current = context
      const source = context.createMediaStreamSource(stream)
      analyser = context.createAnalyser()
      analyser.fftSize = 128
      analyser.smoothingTimeConstant = 0.75
      source.connect(analyser)
      analyserRef.current = analyser
      data = new Uint8Array(analyser.frequencyBinCount)
    } catch (_) {
      analyser = null
      data = null
    }
    const startedAt = Date.now()
    waveTimerRef.current = setInterval(() => {
      if (!listeningRef.current) return
      let bars
      if (analyser && data) {
        analyser.getByteFrequencyData(data)
        bars = Array.from({ length: VOICE_BAR_COUNT }, (_, index) => {
          const bin = Math.min(data.length - 1, Math.floor((index / VOICE_BAR_COUNT) * data.length * 0.72))
          const peak = Math.max(data[bin], data[Math.min(data.length - 1, bin + 2)] || 0)
          return Math.min(1, Math.max(0.06, (peak / 255) * 1.8))
        })
      } else {
        const tick = (Date.now() - startedAt) / 160
        bars = Array.from({ length: VOICE_BAR_COUNT }, (_, index) => {
          const wave = Math.sin(tick + index * 0.42) * 0.28 + Math.sin(tick * 1.9 + index * 0.13) * 0.22 + 0.38
          const spike = Math.random() < 0.06 ? Math.random() * 0.55 : 0
          return Math.min(1, Math.max(0.06, wave + spike))
        })
      }
      setWaveBars(bars)
    }, 70)
  }

  // 只停采集（麦克风 + 波形 + 朗读），不关闭面板、不中止在途请求。
  const stopCapture = () => {
    listeningRef.current = false
    clearTimeout(restartTimerRef.current)
    restartTimerRef.current = 0
    const recognition = recognitionRef.current
    recognitionRef.current = null
    if (recognition) { try { recognition.stop() } catch (_) {} }
    stopWaveform()
    try { window.speechSynthesis?.cancel() } catch (_) {}
  }

  const stopAll = () => {
    openRef.current = false
    stopCapture()
    clearInterval(elapsedTimerRef.current)
    const controller = abortRef.current
    abortRef.current = null
    if (controller) controller.abort()
  }

  const speakReply = (text, done) => {
    let finished = false
    const finish = () => { if (finished) return; finished = true; done() }
    if (!ttsOnRef.current || !window.speechSynthesis) { window.setTimeout(finish, 200); return }
    try {
      const utterance = new SpeechSynthesisUtterance(speechText(text))
      utterance.lang = navigator.language || 'zh-CN'
      utterance.rate = 1
      utterance.onend = finish
      utterance.onerror = finish
      window.speechSynthesis.cancel()
      window.speechSynthesis.speak(utterance)
    } catch (_) {
      finish()
    }
  }

  const resumeListening = () => {
    if (!openRef.current || listeningRef.current) return
    changePhase('listening')
    startListening()
  }

  // 纯对话调用：把完整讨论历史发给后端，拿回一句回复。
  // summarize=true 时后端换成总结提示词，返回结构化总结。
  const callVoiceChat = async (history, summarize) => {
    const controller = new AbortController()
    abortRef.current = controller
    try {
      const response = await fetch(withToken('/api/voice/chat'), {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          messages: history.map((item) => ({ role: item.role, text: item.text })),
          model: model || undefined,
          summarize: Boolean(summarize),
        }),
        signal: controller.signal,
      })
      if (!response.ok) {
        const text = await response.text().catch(() => '')
        throw new Error(`HTTP ${response.status}${text ? ' · ' + text.slice(0, 200) : ''}`)
      }
      const data = await response.json()
      return String(data.reply || '').trim()
    } finally {
      if (abortRef.current === controller) abortRef.current = null
    }
  }

  const submitUtterance = () => {
    if (busy) return
    const text = (finalRef.current + interimRef.current).trim()
    if (!text) return
    finalRef.current = ''
    interimRef.current = ''
    sessionBaseRef.current = ''
    sendUtterance(text)
  }

  const sendUtterance = (text) => {
    stopCapture()
    setInterim('')
    const next = [...logRef.current, { role: 'user', text }]
    setLog(next)
    changePhase('thinking')
    setBusy(true)
    ;(async () => {
      try {
        const reply = await callVoiceChat(next, false)
        if (!openRef.current) return
        setBusy(false)
        setLog((previous) => [...previous, { role: 'assistant', text: reply }])
        if (ttsOnRef.current && window.speechSynthesis) {
          changePhase('speaking')
          speakReply(reply, resumeListening)
        } else {
          resumeListening()
        }
      } catch (error) {
        if (!openRef.current || error.name === 'AbortError') return
        setBusy(false)
        onError?.(`语音讨论失败：${error.message}`)
        resumeListening()
      }
    })()
  }

  const startListening = () => {
    if (!openRef.current || listeningRef.current) return
    const Recognition = window.SpeechRecognition || window.webkitSpeechRecognition
    if (!Recognition) {
      onError?.('当前浏览器不支持语音讨论，请使用 Chrome / Edge')
      stopAll()
      onClose()
      return
    }
    listeningRef.current = true
    finalRef.current = ''
    interimRef.current = ''
    sessionBaseRef.current = ''
    setInterim('')
    startWaveform()
    const recognition = new Recognition()
    recognition.lang = navigator.language || 'zh-CN'
    recognition.continuous = true
    recognition.interimResults = true
    recognition.onresult = (event) => {
      if (!listeningRef.current) return
      let finalText = ''
      let interimText = ''
      for (let index = 0; index < event.results.length; index += 1) {
        const result = event.results[index]
        const transcript = result?.[0]?.transcript || ''
        if (result.isFinal) finalText += transcript
        else interimText += transcript
      }
      finalRef.current = sessionBaseRef.current + finalText
      interimRef.current = interimText
      const total = (finalRef.current + interimText).trim()
      setInterim(total)
      // 刻意不自动提交：停顿时长不能代表「说完了」，由用户点「发送这句」显式提交。
    }
    recognition.onerror = (event) => {
      const code = event.error
      if (code === 'aborted' || code === 'no-speech') return
      const map = {
        'not-allowed': '麦克风权限被拒绝，无法语音讨论',
        'service-not-allowed': '麦克风权限被拒绝，无法语音讨论',
        'audio-capture': '没有检测到可用的麦克风',
        network: '语音识别服务不可用（需要联网）',
      }
      if (map[code]) {
        onError?.(map[code])
        stopAll()
        onClose()
      }
    }
    recognition.onend = () => {
      if (!listeningRef.current) return
      if (interimRef.current) {
        finalRef.current += interimRef.current
        interimRef.current = ''
      }
      sessionBaseRef.current = finalRef.current
      restartTimerRef.current = window.setTimeout(() => {
        if (!listeningRef.current) return
        try { recognition.start() } catch (_) {}
      }, 200)
    }
    recognitionRef.current = recognition
    try {
      recognition.start()
    } catch (error) {
      onError?.(`语音讨论启动失败：${error.message}`)
      stopAll()
      onClose()
    }
  }

  const pauseListening = () => {
    if (!listeningRef.current) return
    stopCapture()
    setInterim('')
    changePhase('paused')
  }

  // 打断：优先中止在途的模型请求；否则只是跳过朗读继续听。
  const interruptReply = () => {
    const controller = abortRef.current
    if (controller) {
      controller.abort()
      abortRef.current = null
      setBusy(false)
      resumeListening()
      return
    }
    try { window.speechSynthesis?.cancel() } catch (_) {}
    resumeListening()
  }

  const toggleTts = () => {
    const next = !ttsOn
    setTtsOn(next)
    ttsOnRef.current = next
    if (!next) { try { window.speechSynthesis?.cancel() } catch (_) {} }
  }

  // 结束讨论：让模型出总结，只填入输入框等待用户修改后自行发送。
  // 绝不直接发送——总结与语音转写都可能失真，用户必须保留最后决定权。
  const finishDiscussion = () => {
    if (busy) return
    stopCapture()
    const items = [...logRef.current]
    const tail = interimRef.current.trim()
    if (tail) items.push({ role: 'user', text: tail })
    setInterim('')
    finalRef.current = ''
    interimRef.current = ''
    sessionBaseRef.current = ''
    if (!items.length) { onClose(); return }
    setBusy(true)
    changePhase('thinking')
    ;(async () => {
      try {
        const summary = await callVoiceChat(items, true)
        if (!openRef.current) return
        setBusy(false)
        if (summary) onDraft?.(summary)
        onClose()
      } catch (error) {
        if (!openRef.current || error.name === 'AbortError') return
        setBusy(false)
        onError?.(`总结失败：${error.message}`)
        resumeListening()
      }
    })()
  }

  const closeDiscussion = () => {
    stopAll()
    onClose()
  }

  useEffect(() => {
    openRef.current = true
    startedRef.current = Date.now()
    elapsedTimerRef.current = window.setInterval(() => {
      if (!openRef.current) return
      setElapsed(Math.floor((Date.now() - startedRef.current) / 1000))
    }, 500)
    const timer = window.setTimeout(() => { if (openRef.current) startListening() }, 200)
    return () => {
      clearTimeout(timer)
      stopAll()
    }
  }, [])

  useEffect(() => {
    const node = bodyRef.current
    if (node) node.scrollTop = node.scrollHeight
  }, [log, interim, phase])

  useEffect(() => {
    const onKey = (event) => {
      if (event.key !== 'Escape') return
      event.preventDefault()
      stopAll()
      onClose()
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [])

  const statusText = {
    listening: '正在聆听…',
    paused: '已暂停',
    thinking: '思考中…',
    speaking: '朗读中…',
  }[phase] || ''

  return (
    <div className="vd-mask" role="dialog" aria-modal="true">
      <div className="vd-panel">
        <div className="vd-head">
          <div>
            <div className="vd-title">语音讨论</div>
            <div className="vd-sub">AI 只陪你聊清思路，不调用任何工具；结束后生成总结填进输入框，由你改完再发</div>
          </div>
          <div className="vd-head-actions">
            <button
              type="button"
              className={`icon-btn ${ttsOn ? 'active' : ''}`}
              onClick={toggleTts}
              title={ttsOn ? '关闭语音播报' : '开启语音播报'}
              aria-label="语音播报"
            >
              <Icon name={ttsOn ? 'volume' : 'volumeOff'} size={16} />
            </button>
            <button type="button" className="icon-btn" onClick={closeDiscussion} title="关闭" aria-label="关闭语音讨论">
              <Icon name="x" size={17} />
            </button>
          </div>
        </div>
        <div className="vd-body" ref={bodyRef}>
          {log.length === 0 && !interim && (
            <div className="vd-empty">
              <div className="vd-empty-icon"><Icon name="wave" size={24} /></div>
              想到什么就说什么，说完点「发送这句」。AI 会追问、帮你补全没想到的角度；聊够了点「结束并整理」。
            </div>
          )}
          {log.map((item, index) => (
            <div key={`${item.role}-${index}`} className={`vd-msg ${item.role}`}>
              <span className="vd-who">{item.role === 'user' ? '你' : 'AI'}</span>
              <div className="vd-text">{item.text}</div>
            </div>
          ))}
          {interim && (
            <div className="vd-msg user interim">
              <span className="vd-who">你</span>
              <div className="vd-text">{interim}</div>
            </div>
          )}
        </div>
        <div className="vd-foot">
          {phase === 'listening' ? (
            <button type="button" className="voice-btn cancel" onClick={pauseListening} title="暂停聆听" aria-label="暂停聆听">
              <Icon name="pause" size={18} />
            </button>
          ) : phase === 'paused' ? (
            <button type="button" className="voice-btn cancel" onClick={resumeListening} title="继续聆听" aria-label="继续聆听">
              <Icon name="mic" size={18} />
            </button>
          ) : (
            <button type="button" className="voice-btn cancel" onClick={interruptReply} title="打断" aria-label="打断">
              <Icon name="stop" size={16} />
            </button>
          )}
          <div className="vd-status">
            <span className={`vd-dot ${phase}`} />
            <span>{statusText}</span>
          </div>
          <div className={`voice-wave vd-wave ${phase === 'listening' ? '' : 'idle'}`} aria-hidden="true">
            {waveBars.map((level, index) => (
              <span key={index} style={{ height: `${Math.round(3 + level * 23)}px` }} />
            ))}
          </div>
          <span className="voice-time">{voiceClock(elapsed)}</span>
          <button
            type="button"
            className="vd-send"
            disabled={!interim.trim() || busy || phase === 'thinking' || phase === 'speaking'}
            onClick={submitUtterance}
            title="把这句话发给 AI"
          >
            发送这句
          </button>
          <button
            type="button"
            className="vd-end"
            disabled={busy}
            onClick={finishDiscussion}
          >
            {busy ? '整理中…' : '结束并整理'}
          </button>
        </div>
      </div>
    </div>
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

function Panel({ tab, onTab, onClose, onDock, width, currentId, workspace, meta, onError, onToast, onReference }) {
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
        <div className={`pane-wrap ${tab === 'files' ? 'active' : ''}`}><FilesPane workspace={workspace} onError={onError} onReference={onReference} /></div>
        <div className={`pane-wrap ${tab === 'diff' ? 'active' : ''}`}><DiffPane workspace={workspace} onError={onError} /></div>
        <div className={`pane-wrap ${tab === 'tasks' ? 'active' : ''}`}><TasksPane currentId={currentId} workspace={workspace} onError={onError} /></div>
        <div className={`pane-wrap ${tab === 'attachments' ? 'active' : ''}`}><AttachmentsPane workspace={workspace} onError={onError} /></div>
        <div className={`pane-wrap ${tab === 'terminal' ? 'active' : ''}`}>{terminalMounted && <TerminalPane />}</div>
      </div>
    </section>
  )
}

/// 带行号的只读预览：点击行首建立选区（第一次起点、第二次终点），供「引用选区」生成 `@path:12-40`。
function FileLines({ content, selStart, selEnd, onPick }) {
  const lines = String(content || '').split('\n')
  // 选区可能反向拖（先点后面再点前面），统一成 lo..hi。
  const lo = selStart === null ? null : Math.min(selStart, selEnd ?? selStart)
  const hi = selStart === null ? null : Math.max(selStart, selEnd ?? selStart)
  return (
    <div className="file-lines">
      {lines.map((line, index) => {
        const number = index + 1
        const selected = lo !== null && number >= lo && number <= hi
        return (
          <div
            key={index}
            className={`file-line ${selected ? 'sel' : ''}`}
            onClick={() => onPick(number)}
            title={`第 ${number} 行`}
          >
            <span className="ln">{number}</span>
            <span className="lc">{line || ' '}</span>
          </div>
        )
      })}
    </div>
  )
}

function FilesPane({ workspace, onError, onReference }) {
  const [files, setFiles] = useState([])
  const [filter, setFilter] = useState('')
  const [preview, setPreview] = useState(null)
  const [editing, setEditing] = useState(false)
  const [draft, setDraft] = useState('')
  const [loading, setLoading] = useState(false)
  // 行选区：首次点击设起点，再次点击设终点（成区间），第三次点击重新开始。
  const [selStart, setSelStart] = useState(null)
  const [selEnd, setSelEnd] = useState(null)

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
      // 换文件时旧行号无意义，必须清掉，否则会引用到新文件的错误位置。
      setSelStart(null)
      setSelEnd(null)
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
          {!editing && selStart !== null && (
            <button
              title="把选中行作为 @path:起始-结束 引用插入输入框"
              onClick={() => {
                const lo = Math.min(selStart, selEnd ?? selStart)
                const hi = Math.max(selStart, selEnd ?? selStart)
                const ref = lo === hi ? `@${preview.path}:${lo} ` : `@${preview.path}:${lo}-${hi} `
                onReference?.(ref)
                setSelStart(null)
                setSelEnd(null)
              }}
            >引用第 {Math.min(selStart, selEnd ?? selStart)}-{Math.max(selStart, selEnd ?? selStart)} 行</button>
          )}
          {(selStart !== null) && <button onClick={() => { setSelStart(null); setSelEnd(null) }}>清除选区</button>}
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
                    : <FileLines
                        content={preview.content || ''}
                        selStart={selStart}
                        selEnd={selEnd}
                        onPick={(line) => {
                          if (selStart === null || selEnd !== null) { setSelStart(line); setSelEnd(null) }
                          else setSelEnd(line)
                        }}
                      />}
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
