import React, { useCallback, useEffect, useState } from 'react'
import { api, postJson, rawUrl, withWorkspace } from '../api'
import { Markdown } from '../markdown'

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

export function FilesPane({ workspace, onError, onReference }) {
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
