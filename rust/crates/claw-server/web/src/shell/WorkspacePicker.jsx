import React, { useEffect, useRef, useState } from 'react'

export function WorkspacePicker({ workspaces, onChoose, onNativePick, onClose }) {
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
        <div className="picker-head">
          <div className="picker-heading">
            <b>选择工作区</b>
            <small>{filtered.length} 个文件夹 · 新会话将在此创建</small>
          </div>
          <button type="button" className="picker-close" onClick={onClose} aria-label="关闭">×</button>
        </div>
        <div className="picker-search">
          <span>⌕</span>
          <input ref={inputRef} value={query} onChange={(event) => setQuery(event.target.value)} placeholder="搜索文件夹" />
        </div>
        <div className="picker-list">
          {filtered.map((item) => (
            <button key={item.path} className="picker-row" onClick={() => onChoose(item.path)} title={item.path}>
              <span className="picker-icon" aria-hidden="true">▸</span>
              <span className="picker-text">
                <b>{item.name}</b>
                <small>{item.path}</small>
              </span>
              {item.session_count > 0 && <span className="picker-count">{item.session_count} 会话</span>}
            </button>
          ))}
          {filtered.length === 0 && <div className="empty">没有匹配的文件夹</div>}
        </div>
        <div className="picker-foot">
          <button
            className="picker-native"
            disabled={busy}
            onClick={async () => {
              setBusy(true)
              try { await onNativePick() } finally { setBusy(false) }
            }}
          >
            <span>＋</span> {busy ? '正在选择…' : '选择其他文件夹…'}
          </button>
          <span className="picker-hint">Esc 关闭</span>
        </div>
      </div>
    </div>
  )
}
