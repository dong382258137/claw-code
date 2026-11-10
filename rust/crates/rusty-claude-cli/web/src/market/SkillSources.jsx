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

// 后端时间戳是 unix 毫秒；缺失时返回空串，由调用方决定文案。
function formatTime(value) {
  if (!value) return ''
  const date = new Date(value)
  if (Number.isNaN(date.getTime())) return ''
  return date.toLocaleString('zh-CN', { hour12: false })
}

// 来源市场：本地/远程索引来源的增删查，以及选中来源的安装列表。
export function SkillSources() {
  const [sources, setSources] = useState([])
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState('')
  const [notice, setNotice] = useState('')
  const [selected, setSelected] = useState('')
  const [form, setForm] = useState({ type: 'local', value: '', name: '', subdir: '' })
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
        subdir: form.type === 'git' ? form.subdir.trim() || null : null,
      })
      setNotice(
        created.status === 'ok'
          ? `已添加来源 ${created.name || created.id}`
          : `来源已添加，但拉取失败：${created.last_error || created.status}`,
      )
      setSelected(created.id)
      setForm({ type: 'local', value: '', name: '', subdir: '' })
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
          <option value="git">git 仓库（clone 到本地镜像）</option>
        </select>
        <input
          value={form.value}
          onChange={(event) => setForm((prev) => ({ ...prev, value: event.target.value }))}
          placeholder={
            form.type === 'git'
              ? 'https://example.com/skills.git'
              : 'D:\\tmp\\market 或 ...\\marketplace.json'
          }
          required
        />
        {form.type === 'git' ? (
          <input
            value={form.subdir}
            onChange={(event) => setForm((prev) => ({ ...prev, subdir: event.target.value }))}
            placeholder="仓库内子目录（可选）"
          />
        ) : null}
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
              <span>{source.kind === 'git' ? 'git' : '本地'}</span>
              <span>{source.skill_count} 个技能</span>
              {source.git_ref ? <span>commit {source.git_ref}</span> : null}
              <span>
                {source.last_success_at_unix_ms
                  ? `上次成功：${formatTime(source.last_success_at_unix_ms)}`
                  : '从未成功'}
              </span>
            </div>
            {source.status !== 'ok' ? (
              <div className="skill-notice warn">
                {STATUS_LABELS[source.status] || source.status}
                {source.last_error ? ` · ${source.last_error}` : ''}
                {source.last_success_at_unix_ms
                  ? `（上次成功：${formatTime(source.last_success_at_unix_ms)}）`
                  : '（从未成功）'}
              </div>
            ) : null}
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
