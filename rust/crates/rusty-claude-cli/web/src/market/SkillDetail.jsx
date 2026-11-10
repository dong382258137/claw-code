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

export function SkillDetail({ name }) {
  const [detail, setDetail] = useState(null)
  const [raw, setRaw] = useState('')
  const [error, setError] = useState('')
  const [loading, setLoading] = useState(false)
  const [mode, setMode] = useState('rendered')

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
