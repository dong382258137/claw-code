import React, { useEffect, useMemo, useRef, useState } from 'react'
import { Markdown } from '../markdown'
import { groupTurns, processTitle } from '../turns'
import { fmtClock } from '../shared/utils.js'
import { api } from '../api'
import { ToolCard, FullTextButton } from './ToolCard'

export function Conversation({ messages, optimistic, live, liveThinking, liveTools, loading }) {
  const ref = useRef(null)
  const stick = useRef(true)
  const [atBottom, setAtBottom] = useState(true)
  const [visibleCount, setVisibleCount] = useState(120)
  // 被截断的块：ref → 磁盘原文。只在用户点「查看全文」时回读，不破坏列表体积。
  const [expanded, setExpanded] = useState({})
  const [expanding, setExpanding] = useState('')
  const [expandError, setExpandError] = useState('')

  // 以「屏幕上那串截断文本」为 key 把元数据传给子组件，
  // 这样不必改动 turns.js 的行结构也能定位到具体块。
  const truncations = useMemo(() => {
    const map = new Map()
    for (const message of messages || []) {
      for (const block of message.blocks || []) {
        const meta = block.truncated
        if (!meta?.ref) continue
        const value = block.kind === 'tool_use' ? block.input : block.kind === 'tool_result' ? block.output : block.text
        if (typeof value === 'string' && value) map.set(value, { ...meta, kind: block.kind })
      }
    }
    return map
  }, [messages])

  const patched = useMemo(() => {
    if (!Object.keys(expanded).length) return messages || []
    return (messages || []).map((message) => {
      if (!(message.blocks || []).some((block) => block.truncated?.ref && expanded[block.truncated.ref])) {
        return message
      }
      return {
        ...message,
        blocks: message.blocks.map((block) => {
          const full = block.truncated?.ref ? expanded[block.truncated.ref] : ''
          if (!full) return block
          const field = block.kind === 'tool_use' ? 'input' : block.kind === 'tool_result' ? 'output' : 'text'
          return { ...block, [field]: full }
        }),
      }
    })
  }, [messages, expanded])

  const expand = async (meta) => {
    if (!meta?.ref || expanding) return
    setExpanding(meta.ref)
    setExpandError('')
    try {
      const data = await api('/api/block?r=' + encodeURIComponent(meta.ref))
      const text = data.text ?? data.input ?? data.output ?? ''
      setExpanded((prev) => ({ ...prev, [meta.ref]: text }))
    } catch (error) {
      setExpandError(String(error?.message || error))
    } finally {
      setExpanding('')
    }
  }

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
    () => patched.filter((message) => (message.blocks || []).length > 0),
    [patched],
  )
  const shown = visible.length > visibleCount ? visible.slice(-visibleCount) : visible
  const turns = useMemo(() => groupTurns(shown), [shown])
  // 流式过程不一定伴随正文增量：工具型 turn 往往是先 thinking/tool_use，
  // 直到最后才吐一段 delta。只看 live（正文）会让整段执行过程不可见。
  const streaming = Boolean(live) || Boolean(liveThinking) || (liveTools || []).length > 0
  return (
    <>
      <div className="messages" ref={ref} onScroll={onScroll}>
        <div className="chat-column">
        {loading && visible.length === 0 && (
          <div className="skeleton-list">
            <div className="skeleton" /><div className="skeleton short" /><div className="skeleton" />
          </div>
        )}
        {!loading && visible.length === 0 && !optimistic && !streaming && (
          <div className="chat-empty">
            <div className="chat-empty-mark" aria-hidden="true"><span /></div>
            <div className="chat-empty-title">直接输入即可发送</div>
            <div className="chat-empty-sub">未选会话时，会在当前工作区新建任务</div>
            <div className="chat-empty-keys">
              <span><kbd>Enter</kbd>发送</span>
              <span><kbd>Shift</kbd><kbd>Enter</kbd>换行</span>
              <span><kbd>/</kbd>快捷操作</span>
              <span><kbd>@</kbd>引用文件</span>
            </div>
          </div>
        )}
        {visible.length > shown.length && (
          <div className="load-earlier">
            <button onClick={() => setVisibleCount((count) => count + 120)}>
              加载更早（还有 {visible.length - shown.length} 条）
            </button>
          </div>
        )}
        {turns.map((turn, index) => (
          <TurnView
            key={index}
            turn={turn}
            truncations={truncations}
            onExpand={expand}
            expanding={expanding}
          />
        ))}
        {expandError && <div className="truncation-error">读取全文失败：{expandError}</div>}
        {optimistic && (
          <UserMessage
            pending
            message={{ role: 'user', blocks: [{ kind: 'text', text: optimistic.text }], ts: optimistic.at }}
          />
        )}
        {streaming && <LiveTurn thinking={liveThinking} tools={liveTools || []} text={live} />}
        </div>
        {!atBottom && (
          <div className="jump-row">
            <button className="jump" onClick={jumpToLatest}>回到最新 ↓</button>
          </div>
        )}
      </div>
    </>
  )
}

export function UserMessage({ message, pending }) {
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

// 截断块专用：按 ref 回读磁盘原文，展开后原位替换（刷新前后都可用）。
// 组件本体在 ToolCard.jsx，AnswerMessage 与工具卡共用。

export function AnswerMessage({ text, ts, meta, onExpand, expanding }) {
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
        <FullTextButton meta={meta} onExpand={onExpand} expanding={expanding} />
      </div>
      <div className="msg-body"><Markdown text={text} /></div>
    </div>
  )
}

export function ProcessRow({ row, truncations, onExpand, expanding }) {
  if (row.kind === 'thinking') {
    return (
      <details className="process-thinking">
        <summary>思考</summary>
        <pre>{row.text}</pre>
        <FullTextButton meta={truncations?.get(row.text)} onExpand={onExpand} expanding={expanding} />
      </details>
    )
  }
  if (row.kind === 'tool') {
    return (
      <ToolCard
        row={row}
        truncations={truncations}
        onExpand={onExpand}
        expanding={expanding}
      />
    )
  }
  return <div className="process-text"><Markdown text={row.text} /></div>
}

export function ProcessGroup({ rows, live = false, durationMs = 0, truncations, onExpand, expanding }) {
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
      {open && (
        <div className="process-body">
          {rows.map((row, index) => (
            <ProcessRow
              key={index}
              row={row}
              truncations={truncations}
              onExpand={onExpand}
              expanding={expanding}
            />
          ))}
        </div>
      )}
    </div>
  )
}

export function TurnView({ turn, truncations, onExpand, expanding }) {
  const durationMs = turn.ts && turn.end ? Math.max(0, turn.end - turn.ts) : 0
  return (
    <div className="turn">
      {turn.user && <UserMessage message={turn.user} />}
      {turn.rows.length > 0 && (
        <ProcessGroup
          rows={turn.rows}
          durationMs={durationMs}
          truncations={truncations}
          onExpand={onExpand}
          expanding={expanding}
        />
      )}
      {turn.answer && (
        <AnswerMessage
          text={turn.answer}
          ts={turn.end}
          meta={truncations?.get(turn.answer)}
          onExpand={onExpand}
          expanding={expanding}
        />
      )}
    </div>
  )
}

export function LiveTurn({ thinking, tools, text }) {
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
