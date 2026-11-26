import React, { useEffect, useState } from 'react'
import { api } from '../api.js'
import { Markdown } from '../markdown.jsx'

const SOURCE_LABELS = {
  project: '项目',
  user_config: '用户配置',
  user: '用户目录',
  builtin: '内置',
}

const ORIGIN_LABELS = {
  skills_dir: 'skills 目录',
  legacy_commands: 'legacy /commands',
}

function formatSize(bytes) {
  if (!bytes) return '0 B'
  if (bytes < 1024) return `${bytes} B`
  return `${(bytes / 1024).toFixed(1)} KB`
}

export function SkillDetail({ name, onRemoved }) {
  const [detail, setDetail] = useState(null)
  const [raw, setRaw] = useState('')
  const [error, setError] = useState('')
  const [loading, setLoading] = useState(false)
  const [mode, setMode] = useState('rendered')
  const [removing, setRemoving] = useState(false)

  useEffect(() => {
    if (!name) return undefined
    let cancelled = false
    setLoading(true)
    setError('')
    setDetail(null)
    setRaw('')
    const encoded = encodeURIComponent(name)
    Promise.all([
      api(`/api/skills/${encoded}`),
      api(`/api/skills/${encoded}/raw?path=SKILL.md`),
    ])
      .then(([info, text]) => {
        if (cancelled) return
        setDetail(info)
        setRaw(text)
      })
      .catch((err) => {
        if (!cancelled) setError(err.message || String(err))
      })
      .finally(() => {
        if (!cancelled) setLoading(false)
      })
    return () => {
      cancelled = true
    }
  }, [name])

  const remove = async () => {
    if (
      !window.confirm(
        `卸载 ${detail.name}？\n\n将删除 ${detail.path}，且不可恢复。项目里的同名技能不受影响。`,
      )
    ) {
      return
    }
    setRemoving(true)
    setError('')
    try {
      await api(`/api/skills/${encodeURIComponent(name)}`, { method: 'DELETE' })
      onRemoved?.()
    } catch (err) {
      setError(err.message || String(err))
    } finally {
      setRemoving(false)
    }
  }

  if (loading) return <div className="skill-detail-empty">加载中…</div>
  if (error) return <div className="skill-error"><span>{error}</span></div>
  if (!detail) return null

  return (
    <div className="skill-detail">
      <div className="skill-detail-head">
        <div>
          <h2>{detail.name}</h2>
          <p>{detail.description || '（无描述）'}</p>
        </div>
        <div className="skill-detail-actions">
          <button
            type="button"
            className={mode === 'rendered' ? 'skill-filter active' : 'skill-filter'}
            onClick={() => setMode('rendered')}
          >
            渲染
          </button>
          <button
            type="button"
            className={mode === 'raw' ? 'skill-filter active' : 'skill-filter'}
            onClick={() => setMode('raw')}
          >
            原文
          </button>
          {detail.uninstallable ? (
            <button
              type="button"
              className="skill-danger"
              onClick={remove}
              disabled={removing}
            >
              {removing ? '卸载中…' : '卸载'}
            </button>
          ) : null}
        </div>
      </div>

      <dl className="skill-meta">
        <dt>调用名</dt>
        <dd><code>{detail.invoke_as}</code></dd>
        <dt>来源</dt>
        <dd>{SOURCE_LABELS[detail.source] || detail.source} · {ORIGIN_LABELS[detail.origin] || detail.origin}</dd>
        <dt>状态</dt>
        <dd>
          {detail.active ? '生效中' : `被${SOURCE_LABELS[detail.shadowed_by] || detail.shadowed_by || '同名技能'}遮蔽`}
        </dd>
        <dt>路径</dt>
        <dd className="skill-path">{detail.path}</dd>
        {detail.installed?.managed ? (
          <>
            <dt>版本</dt>
            <dd>v{detail.installed.version || '未知'}</dd>
            <dt>市场</dt>
            <dd>{detail.installed.marketplace || '未知'}</dd>
            <dt>校验和</dt>
            <dd className="skill-path">{detail.installed.checksum || '未提供'}</dd>
          </>
        ) : null}
      </dl>

      {detail.files?.length ? (
        <div className="skill-files">
          <div className="skill-files-title">文件（{detail.files.length}）</div>
          <div className="skill-file-list">
            {detail.files.map((file) => (
              <div key={file.path} className="skill-file">
                <span>{file.path}</span>
                <span className="skill-file-size">{formatSize(file.size)}</span>
              </div>
            ))}
          </div>
        </div>
      ) : null}

      <div className="skill-preview">
        {mode === 'rendered' ? (
          <Markdown text={raw} />
        ) : (
          <pre className="skill-raw">{raw}</pre>
        )}
      </div>
    </div>
  )
}
