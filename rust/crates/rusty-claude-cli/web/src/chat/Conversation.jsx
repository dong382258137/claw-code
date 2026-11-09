import React, { useEffect, useMemo, useRef, useState } from 'react'
import { Markdown } from '../markdown'
import { groupTurns, processTitle } from '../turns'
import { fmtClock } from '../shared/utils.js'

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

export function Conversation({ messages, optimistic, live, liveThinking, liveTools, loading }) {
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

export function AnswerMessage({ text, ts }) {
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

export function ProcessRow({ row }) {
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

export function ProcessGroup({ rows, live = false, durationMs = 0 }) {
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

export function TurnView({ turn }) {
  const durationMs = turn.ts && turn.end ? Math.max(0, turn.end - turn.ts) : 0
  return (
    <div className="turn">
      {turn.user && <UserMessage message={turn.user} />}
      {turn.rows.length > 0 && <ProcessGroup rows={turn.rows} durationMs={durationMs} />}
      {turn.answer && <AnswerMessage text={turn.answer} ts={turn.end} />}
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
