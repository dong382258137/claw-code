import React, { useCallback, useEffect, useRef, useState } from 'react'
import { api, postJson, withToken } from './api'
import { useToasts } from './shared/useToasts.js'
import { basename, clampWidth, relTime } from './shared/utils.js'
import { navigate, useRoute } from './shared/router.js'
import { Sidebar } from './shell/Sidebar.jsx'
import { WorkspacePicker } from './shell/WorkspacePicker.jsx'
import { TopBar } from './shell/TopBar.jsx'
import { Toasts } from './shell/Toasts.jsx'
import { Conversation } from './chat/Conversation.jsx'
import { Composer } from './chat/Composer.jsx'
import { VoiceDiscussion } from './chat/VoiceDiscussion.jsx'
import { useSessionEvents } from './chat/useSessionEvents.js'
import { Panel } from './panel/Panel.jsx'
import { SettingsPage } from './settings/SettingsPage.jsx'
import { MarketPage } from './market/MarketPage.jsx'

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

  // hash 路由：#/plugins、#/skills 是市场骨架页，其余回到对话页。
  const route = useRoute()

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

  /* ---------- 会话订阅（单一 /events 事件流） ---------- */
  useSessionEvents({
    currentId,
    push,
    setCurrent,
    setMessages,
    setLive,
    setLiveThinking,
    setLiveTools,
    setOptimistic,
    setRunning,
    setApprovals,
    setUpgrading,
    setLoadingSession,
    setConnected,
    setPendingDraft,
    lastPromptRef,
    runningRef,
    sendRef,
  })

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

  const deleteSessions = useCallback(async (ids) => {
    if (!ids.length) return
    try {
      const body = await postJson('/api/sessions/delete', { ids })
      const deleted = body.deleted || []
      const skipped = body.skipped || []
      if (deleted.length) {
        setSessions((list) => list.filter((item) => !deleted.includes(item.id)))
        setSessionTotal((total) => Math.max(0, total - deleted.length))
        // 删掉当前正在看的会话：回到「未选择会话」，与单条删除一致。
        if (currentId && deleted.includes(currentId)) {
          setCurrent(null)
          setMessages([])
          setLive('')
          setLiveThinking('')
          setLiveTools([])
          setApprovals([])
        }
      }
      if (skipped.length) {
        push(`已删除 ${deleted.length} 个会话，${skipped.length} 个未删除（运行中或已被删除）`, 'error')
      } else {
        push(`已删除 ${deleted.length} 个会话`)
      }
    } catch (error) {
      push(`批量删除失败：${error.message}`, 'error')
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
      // baseline 记录发送前的用户消息条数：快照里的用户消息变多就说明这条已落盘，
      // 乐观气泡随之清除。不按文本比对，因为服务端会展开 @引用再落盘。
      const baseline = sessionId === currentId
        ? (messages || []).filter((message) => message.role === 'user').length
        : 0
      // 带上 sessionId：新会话的订阅 effect 会立刻跑一次，不能把刚输入的内容清掉。
      setOptimistic({ text: trimmed, at: Date.now(), sessionId, baseline })
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
  }, [activeWorkspace, createSession, currentId, messages, push, webSettings.model, webSettings.permission])
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

  // 从市场页回到对话页时，顺手把会话/新建动作一并完成，避免切换后停在旧页面。
  const showSession = useCallback((session) => {
    navigate('chat')
    selectSession(session)
  }, [selectSession])
  const newSession = useCallback((workspace, options) => {
    navigate('chat')
    return createSession(workspace, options)
  }, [createSession])

  const settings = settingsOpen && (
    <SettingsPage
      settings={webSettings}
      meta={meta}
      onClose={() => setSettingsOpen(false)}
      onSave={async (patch) => { await saveSettings(patch, { silent: false }); setSettingsOpen(false) }}
    />
  )

  const sidebar = (
    <Sidebar
      sessions={sessions}
      filter={filter}
      onFilter={setFilter}
      currentId={currentId}
      collapsed={sidebarCollapsed}
      onToggle={toggleSidebar}
      onSelect={showSession}
      onNew={newSession}
      onNewIn={(workspace) => newSession(workspace)}
      onAddWorkspace={addWorkspace}
      onOpenPicker={() => setWorkspacePickerOpen(true)}
      workspaces={workspaces}
      activeWorkspace={activeWorkspace}
      onRename={renameSession}
      onDelete={deleteSession}
      onDeleteMany={deleteSessions}
      total={sessionTotal}
      onLoadMore={loadMoreSessions}
      onSettings={() => setSettingsOpen(true)}
      onTheme={() => saveSettings({ theme: webSettings.theme === 'dark' ? 'light' : 'dark' })}
      theme={webSettings.theme}
      route={route}
      onNavigate={navigate}
    />
  )

  const chatMain = (
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
      {settings}
    </main>
  )

  const marketMain = (
    <main>
      <MarketPage kind={route} />
      {settings}
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

  const chatRoute = route === 'chat'

  return (
    <div className={`app ${dock === 'left' ? 'dock-left' : ''}`}>
      {sidebar}
      {chatRoute && dock === 'left' && panelOpen && <div className="splitter" onMouseDown={startResize} />}
      {chatRoute && dock === 'left' && panel}
      {chatRoute ? chatMain : marketMain}
      {chatRoute && dock === 'right' && panelOpen && <div className="splitter" onMouseDown={startResize} />}
      {chatRoute && dock === 'right' && panel}
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
