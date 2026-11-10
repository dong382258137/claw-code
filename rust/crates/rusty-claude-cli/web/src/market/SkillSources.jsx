import React, { useCallback, useEffect, useState } from 'react'
import { api, postJson } from '../api.js'
import { SkillInstallList } from './SkillInstallList.jsx'

const STATUS_LABELS = {
  ok: '可用',
  unreadable: '不可读',
  unsupported: '不支持的索引版本',
  git_not_found: '未找到 git',
  clone_failed: '拉取失败',
  timeout: '超时',
}

// 来源市场：本地/远程索引来源的增删查，以及选中来源的安装列表。
export function SkillSources() {
  const [sources, setSources] = useState([])
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState('')
  const [notice, setNotice] = useState('')
  const [selected, setSelected] = useState('')
  const [form, setForm] = useState({ type: 'local', value: '', name: '' })
  const [adding, setAdding] = useState(false)

  const load = useCallback(async () => {
    setLoading(true)
    setError('')
    try {
      const payload = await api('/api/skill-sources')
      setSources(payload.sources || [])
    } catch (err) {
      setError(err.message || String(err))
    } finally {
      setLoading(false)
    }
  }, [])

  useEffect(() => {
    load()
  }, [load])

  const addSource = async (event) => {
    event.preventDefault()
    setAdding(true)
    setError('')
    setNotice('')
    try {
      const created = await postJson('/api/skill-sources', {
        type: form.type,
        value: form.value,
        name: form.name.trim() || null,
      })
      setNotice(`已添加来源 ${created.name || created.id}`)
      setSelected(created.id)
      setForm({ type: 'local', value: '', name: '' })
      await load()
    } catch (err) {
      setError(err.message || String(err))
    } finally {
      setAdding(false)
    }
  }

  const refresh = async (id) => {
    setError('')
    setNotice('')
    try {
      const updated = await api(`/api/skill-sources/${encodeURIComponent(id)}/refresh`, {
        method: 'POST',
      })
      setNotice(updated.status === 'ok' ? '来源已刷新' : `刷新失败：${updated.last_error || updated.status}`)
      await load()
    } catch (err) {
      setError(err.message || String(err))
    }
  }

  const remove = async (source) => {
    if (!window.confirm(`删除来源「${source.name || source.id}」？已安装的技能不会被卸载。`)) return
    setError('')
    setNotice('')
    try {
      await api(`/api/skill-sources/${encodeURIComponent(source.id)}`, { method: 'DELETE' })
      if (selected === source.id) setSelected('')
      setNotice('来源已删除')
      await load()
    } catch (err) {
      setError(err.message || String(err))
    }
  }

  return (
    <div className="skill-sources">
      <form className="skill-source-form" onSubmit={addSource}>
        <select
          value={form.type}
          onChange={(event) => setForm((prev) => ({ ...prev, type: event.target.value }))}
        >
          <option value="local">本地目录 / marketplace.json</option>
          <option value="git" disabled>git 仓库（P3 支持）</option>
        </select>
        <input
          value={form.value}
          onChange={(event) => setForm((prev) => ({ ...prev, value: event.target.value }))}
          placeholder="D:\\tmp\\market 或 ...\\marketplace.json"
          required
        />
        <input
          value={form.name}
          onChange={(event) => setForm((prev) => ({ ...prev, name: event.target.value }))}
          placeholder="来源名称（可选）"
        />
        <button type="submit" className="skill-refresh" disabled={adding}>
          {adding ? '添加中…' : '添加来源'}
        </button>
      </form>

      {notice ? <div className="skill-notice">{notice}</div> : null}
      {error ? (
        <div className="skill-error">
          <span>{error}</span>
          <button type="button" onClick={load}>重试</button>
        </div>
      ) : null}

      {loading ? <div className="skill-list-empty">加载中…</div> : null}
      {!loading && sources.length === 0 ? (
        <div className="skill-list-empty">还没有来源。添加一个包含 marketplace.json 的本地目录。</div>
      ) : null}

      <div className="skill-source-list">
        {sources.map((source) => (
          <div
            key={source.id}
            className={selected === source.id ? 'skill-source-row active' : 'skill-source-row'}
            onClick={() => setSelected(source.id)}
          >
            <div className="skill-item-top">
              <span className="skill-name">{source.name || source.id}</span>
              <span className={`skill-source-status ${source.status}`}>
                {STATUS_LABELS[source.status] || source.status}
              </span>
            </div>
            <div className="skill-desc">{source.value}</div>
            <div className="skill-source-meta">
              <span>{source.skill_count} 个技能</span>
              {source.last_error ? <span className="skill-source-error">{source.last_error}</span> : null}
            </div>
            <div className="skill-source-actions">
              <span
                role="button"
                tabIndex={0}
                className="skill-link"
                onClick={(event) => {
                  event.stopPropagation()
                  refresh(source.id)
                }}
              >
                刷新
              </span>
              <span
                role="button"
                tabIndex={0}
                className="skill-link danger"
                onClick={(event) => {
                  event.stopPropagation()
                  remove(source)
                }}
              >
                删除
              </span>
            </div>
          </div>
        ))}
      </div>

      {selected ? <SkillInstallList sourceId={selected} onChanged={load} /> : null}
    </div>
  )
}
