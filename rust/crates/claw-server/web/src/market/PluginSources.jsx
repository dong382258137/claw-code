import React, { useCallback, useEffect, useState } from 'react'
import { api, postJson } from '../api.js'
import {
  errorText,
  formatTime,
  SOURCE_STATUS_LABELS,
  SOURCE_TYPE_LABELS,
} from './pluginUtils.js'

// 来源管理：接 /api/plugin-sources，支持添加、刷新、删除（内置源不可删除）。
export function PluginSources({ onChanged }) {
  const [sources, setSources] = useState([])
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState(null)
  const [notice, setNotice] = useState('')
  const [busy, setBusy] = useState('')
  const [form, setForm] = useState({ type: 'http', value: '', name: '', subdir: '' })
  const [adding, setAdding] = useState(false)

  const load = useCallback(async () => {
    setLoading(true)
    setError(null)
    try {
      const payload = await api('/api/plugin-sources')
      setSources(payload.sources || [])
    } catch (err) {
      setError(err)
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
    setError(null)
    setNotice('')
    try {
      const created = await postJson('/api/plugin-sources', {
        type: form.type,
        value: form.value.trim(),
        name: form.name.trim() || null,
        subdir: form.type === 'git' ? form.subdir.trim() || null : null,
      })
      setNotice(
        created.status === 'ok'
          ? `已添加来源 ${created.name || created.id}`
          : `来源已添加，但拉取失败：${created.last_error || created.status}`,
      )
      setForm({ type: 'http', value: '', name: '', subdir: '' })
      await load()
      onChanged?.()
    } catch (err) {
      setError(err)
    } finally {
      setAdding(false)
    }
  }

  const refresh = async (source) => {
    setBusy(source.id)
    setError(null)
    setNotice('')
    try {
      const updated = await api(
        `/api/plugin-sources/${encodeURIComponent(source.id)}/refresh`,
        { method: 'POST' },
      )
      setNotice(
        updated.status === 'ok'
          ? `来源 ${updated.name || updated.id} 已刷新`
          : `刷新失败：${updated.last_error || updated.status}`,
      )
      await load()
      onChanged?.()
    } catch (err) {
      setError(err)
    } finally {
      setBusy('')
    }
  }

  const remove = async (source) => {
    if (!window.confirm(`删除来源「${source.name || source.id}」？已安装的插件不会被卸载。`)) {
      return
    }
    setBusy(source.id)
    setError(null)
    setNotice('')
    try {
      await api(`/api/plugin-sources/${encodeURIComponent(source.id)}`, { method: 'DELETE' })
      setNotice('来源已删除')
      await load()
      onChanged?.()
    } catch (err) {
      setError(err)
    } finally {
      setBusy('')
    }
  }

  return (
    <div className="skill-sources">
      <form className="skill-source-form" onSubmit={addSource}>
        <select
          value={form.type}
          onChange={(event) => setForm((prev) => ({ ...prev, type: event.target.value }))}
        >
          <option value="http">HTTP 索引（marketplace.json）</option>
          <option value="git">git 仓库（clone 到本地镜像）</option>
          <option value="local">本地目录 / marketplace.json</option>
        </select>
        <input
          value={form.value}
          onChange={(event) => setForm((prev) => ({ ...prev, value: event.target.value }))}
          placeholder={
            form.type === 'http'
              ? 'anthropics/claude-plugins-community 或 https://…/marketplace.json'
              : form.type === 'git'
                ? 'https://github.com/anthropics/skills.git'
                : 'D:\\tmp\\market 或 …\\marketplace.json'
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

      <div className="skill-notice">
        手动添加的来源一律按未受信任处理，安装前需要在确认弹窗里再勾选一次风险确认。
      </div>
      {notice ? <div className="skill-notice">{notice}</div> : null}
      {error ? (
        <div className="skill-error">
          <span>{errorText(error)}</span>
          <button type="button" onClick={load}>重试</button>
        </div>
      ) : null}

      {loading ? <div className="skill-list-empty">加载中…</div> : null}
      {!loading && sources.length === 0 ? (
        <div className="skill-list-empty">还没有来源。添加一个包含 marketplace.json 的来源。</div>
      ) : null}

      <div className="skill-source-list">
        {sources.map((source) => (
          <div key={source.id} className="skill-source-row">
            <div className="skill-item-top">
              <span className="skill-name">{source.name || source.id}</span>
              <span className={`skill-source-status ${source.status}`}>
                {SOURCE_STATUS_LABELS[source.status] || source.status}
              </span>
              <span className="skill-source">
                {source.trusted ? '受信任' : '未受信任'}
              </span>
              {source.builtin ? <span className="skill-source">内置</span> : null}
            </div>
            <div className="skill-desc">{source.value}</div>
            <div className="skill-source-meta">
              <span>{SOURCE_TYPE_LABELS[source.type] || source.type}</span>
              <span>{source.plugin_count} 个插件</span>
              {source.subdir ? <span>子目录 {source.subdir}</span> : null}
              {source.git_ref ? <span>commit {source.git_ref}</span> : null}
              <span>{source.checked_at_unix_ms ? `检查于：${formatTime(source.checked_at_unix_ms)}` : '从未检查'}</span>
              {source.cached_at_unix_ms ? (
                <span>缓存更新于：{formatTime(source.cached_at_unix_ms)}</span>
              ) : null}
            </div>
            {source.status !== 'ok' ? (
              <div className="skill-notice warn">
                {SOURCE_STATUS_LABELS[source.status] || source.status}
                {source.last_error ? ` · ${source.last_error}` : ''}
              </div>
            ) : null}
            <div className="skill-source-actions">
              <span
                role="button"
                tabIndex={0}
                className="skill-link"
                onClick={() => refresh(source)}
              >
                {busy === source.id ? '刷新中…' : '刷新'}
              </span>
              {!source.builtin ? (
                <span
                  role="button"
                  tabIndex={0}
                  className="skill-link danger"
                  onClick={() => remove(source)}
                >
                  删除
                </span>
              ) : null}
            </div>
          </div>
        ))}
      </div>
    </div>
  )
}
