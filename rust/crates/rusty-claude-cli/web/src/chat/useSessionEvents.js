import { useEffect } from 'react'
import { api, withToken } from '../api'

// 任务面板关心的增量事件：与对话事件共用同一条 EventSource，
// 由这里转发给 App，再由 App 传给 TasksPane（不再另开一条连接）。
const TASK_EVENT_KINDS = new Set(['todos', 'plan', 'plan_mode', 'tasks', 'plans'])

// 会话事件订阅：唯一一条 /api/session/:id/events EventSource。
// 快照不再轮询 /stream —— 连接建立（含重连）、收到 done、收到 reset 时，
// 主动通过 GET /api/session/:id 拉取一次完整快照做状态对齐，其余时间只吃增量。
export function useSessionEvents({
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
  setTaskEvent,
  lastPromptRef,
  runningRef,
  sendRef,
}) {
  useEffect(() => {
    if (!currentId) return undefined
    let closed = false
    // 切会话必须把所有「本轮实时状态」清零：live/liveThinking/liveTools/running
    // 在 App 里都是全局单份，不清理就会把上一个会话的工具卡、思考过程、
    // 「运行中」标记带进新会话界面——看起来像两个会话在串台。
    // 无会话时输入会自动新建会话并立刻发消息：这条待发消息带着目标 sessionId，
    // 不能因为 currentId 刚切换到它就被清掉，否则用户刚打的字会闪一下消失。
    setOptimistic((pending) => (pending && pending.sessionId === currentId ? pending : null))
    setLive('')
    setLiveThinking('')
    setLiveTools([])
    setApprovals([])
    setMessages([])
    setLoadingSession(true)
    // 任务面板与对话共用这一条事件流：切会话时先清掉上一个会话的尾事件，
    // 避免新面板挂载时把旧会话的待办/计划又应用一次。
    setTaskEvent(null)

    // 「运行中」属于哪个会话只有服务端知道：立刻对齐一次，否则切到空闲会话后
    // 仍会显示上一个会话在执行（状态轮询要 1.2s 才纠正）。
    // 若这次对齐返回前已经收到本会话的流式事件，就以事件为准，避免旧状态覆盖新状态。
    let sawStreamEvent = false
    api(`/api/session/${encodeURIComponent(currentId)}/status`)
      .then((status) => {
        if (closed || sawStreamEvent) return
        setRunning(Boolean(status?.running))
      })
      .catch(() => {
        if (!closed && !sawStreamEvent) setRunning(false)
      })

    // 与旧 /stream snapshot 完全一致的落地方式，保证终态渲染行为不变。
    const applySnapshot = (view) => {
      if (closed || !view) return
      setCurrent((current) => ({ ...view, title: current?.title || view.id }))
      setMessages(view.messages || [])
      setLoadingSession(false)
      const last = view.messages?.[view.messages.length - 1]
      if (last && last.role === 'assistant') { setLive(''); setLiveThinking(''); setLiveTools([]) }
      // 乐观消息的清除条件只认「服务端多了一条用户消息」：
      // 之前是「最后一条是 user 才清」，而 done 时快照最后一条必然是 assistant，
      // 于是气泡连同「发送中…」永久留在底部，形成重复的用户消息。
      // 也不能按文本比对：服务端落盘前会展开 @引用/追加 Web 待办，文本已被改写。
      setOptimistic((pending) => {
        if (!pending) return null
        if (pending.sessionId && pending.sessionId !== currentId) return pending
        const userCount = (view.messages || []).filter((message) => message.role === 'user').length
        return userCount > (pending.baseline || 0) ? null : pending
      })
    }

    const loadSnapshot = () => api(`/api/session/${encodeURIComponent(currentId)}`)
      .then((view) => applySnapshot(view))
      .catch(() => {})

    // 首次加载同样走 applySnapshot：effect 因依赖变化重跑时，服务端快照里
    // 可能已经包含刚发出的消息，必须用同一套条件清掉乐观气泡。
    api(`/api/session/${encodeURIComponent(currentId)}`)
      .then((view) => applySnapshot(view))
      .catch((error) => {
        if (closed) return
        setLoadingSession(false)
        push(`读取会话失败：${error.message}`, 'error')
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

    const events = new EventSource(withToken(`/api/session/${encodeURIComponent(currentId)}/events`))
    let openedOnce = false
    events.onopen = () => {
      setConnected(true)
      // 重连后先拿完整快照，补齐断线期间漏掉的终态；首次连接由上面的初始 GET 负责，
      // 避免同一时刻重复请求。
      if (openedOnce) loadSnapshot()
      openedOnce = true
    }
    events.addEventListener('stream', (event) => {
      if (closed) return
      // 收到本会话的任何流式事件后，以事件为准，不再用切换时那次状态对齐覆盖。
      sawStreamEvent = true
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
        else if (TASK_EVENT_KINDS.has(data.kind)) {
          // 任务面板的增量事件：随会话 id 一起转发，面板切会话后不会误用旧事件。
          setTaskEvent({ sessionId: currentId, payload: data })
        }
        else if (data.kind === 'done') {
          setRunning(false)
          // 终态落地：turn 结束时 Rust 侧已把完整消息写盘，拉一次快照把
          // 流式内容替换成持久化消息（等价旧 /stream 的终态 snapshot）。
          loadSnapshot()
        }
        else if (data.kind === 'reset') { loadSnapshot() }
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
    return () => { closed = true; events.close() }
  }, [currentId, push, setTaskEvent])
}
