import React, { useCallback, useEffect, useRef, useState } from 'react'
import { api, rawUrl, withToken, withWorkspace } from '../api'
import { basename } from '../shared/utils.js'

export function AttachmentsPane({ workspace, onError }) {
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
