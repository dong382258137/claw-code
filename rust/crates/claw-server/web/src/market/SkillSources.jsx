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
  http_error: '索引下载失败',
}

const KIND_LABELS = {
  local: '本地',
  git: 'git',
  http: 'HTTP 索引',
}

const FORMAT_LABELS = {
  native: '自家索引',
  claude: 'Claude marketplace',
  scan: '扫描仓库',
  http: '远程索引',
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
  const [recommended, setRecommended] = useState([])
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState('')
  const [notice, setNotice] = useState('')
  const [selected, setSelected] = useState('')
  const [form, setForm] = useState({ type: 'local', value: '', name: '', subdir: '' })
  const [adding, setAdding] = useState('')

  const load = useCallback(async () => {
    setLoading(true)
    setError('')
    try {
      const payload = await api('/api/skill-sources')
      setSources(payload.sources || [])
      setRecommended(payload.recommended || [])
    } catch (err) {
      setError(err.message || String(err))
    } finally {
      setLoading(false)
    }
  }, [])

  useEffect(() => {
    load()
  }, [load])

  // 推荐源与手填表单共用一条添加路径：后端返回的 status 决定提示文案。
  const createSource = async (payload) => {
    setError('')
    setNotice('')
    try {
      const created = await postJson('/api/skill-sources', payload)
      setNotice(
        created.status === 'ok'
          ? `已添加来源 ${created.name || created.id}`
          : `来源已添加，但拉取失败：${created.last_error || created.status}`,
      )
      setSelected(created.id)
      await load()
      return true
    } catch (err) {
      setError(err.message || String(err))
      return false
    }
  }

  const addSource = async (event) => {
    event.preventDefault()
    setAdding('form')
    const ok = await createSource({
      type: form.type,
      value: form.value,
      name: form.name.trim() || null,
      subdir: form.type === 'git' ? form.subdir.trim() || null : null,
    })
    if (ok) setForm({ type: 'local', value: '', name: '', subdir: '' })
    setAdding('')
  }

  const addRecommended = async (item) => {
    setAdding(item.id)
    await createSource({ type: item.kind, value: item.value, name: item.name || null })
    setAdding('')
  }

  // git 源常带/不带 .git 后缀，比较时统一去掉尾部和大小写差异。
  const normalizeValue = (value) => String(value || '').trim().replace(/\.git$/i, '').replace(/\/+$/, '')
  // 接口里来源类型字段名是 type（与插件来源保持一致），推荐源用的是 kind。
  const isAdded = (item) =>
    sources.some(
      (source) =>
        source.type === item.kind && normalizeValue(source.value) === normalizeValue(item.value),
    )

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
      {recommended.length ? (
        <div className="skill-recommend">
          <div className="skill-recommend-head">
            <h3>推荐来源</h3>
            <span className="chip">{recommended.length} 个</span>
          </div>
          <div className="skill-recommend-list">
            {recommended.map((item) => {
              const added = isAdded(item)
              return (
                <div key={item.id} className="skill-recommend-card">
                  <div className="skill-item-top">
                    <span className="skill-name">{item.name || item.id}</span>
                    <span className="skill-source-status">{KIND_LABELS[item.kind] || item.kind}</span>
                  </div>
                  <div className="skill-desc">{item.description || item.value}</div>
                  <div className="skill-source-meta">
                    <span className="skill-recommend-value">{item.value}</span>
                  </div>
                  <div className="skill-source-actions">
                    <button
                      type="button"
                      className="skill-refresh"
                      disabled={added || adding === item.id}
                      onClick={() => addRecommended(item)}
                    >
                      {added ? '已添加' : adding === item.id ? '添加中…' : '一键添加'}
                    </button>
                  </div>
                </div>
              )
            })}
          </div>
        </div>
      ) : null}

      <form className="skill-source-form" onSubmit={addSource}>
        <select
          value={form.type}
          onChange={(event) => setForm((prev) => ({ ...prev, type: event.target.value }))}
        >
          <option value="local">本地目录 / marketplace.json</option>
          <option value="git">git 仓库（clone 到本地镜像）</option>
          <option value="http">HTTP JSON 索引（自动缓存，断网回退）</option>
        </select>
        <input
          value={form.value}
          onChange={(event) => setForm((prev) => ({ ...prev, value: event.target.value }))}
          placeholder={
            form.type === 'http'
              ? 'https://raw.githubusercontent.com/<owner>/<repo>/main/.claude-plugin/marketplace.json'
              : form.type === 'git'
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
        <button type="submit" className="skill-refresh" disabled={Boolean(adding)}>
          {adding === 'form' ? '添加中…' : '添加来源'}
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
        <div className="skill-list-empty">
          还没有来源。可以一键添加上方推荐来源，或手动填写包含 marketplace.json 的本地目录、git
          仓库、HTTP 索引地址。
        </div>
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
              {source.builtin ? <span className="skill-source-status">内置</span> : null}
              {source.index_format ? (
                <span className="skill-source-status">
                  {FORMAT_LABELS[source.index_format] || source.index_format}
                </span>
              ) : null}
              <span className={`skill-source-status ${source.status}`}>
                {STATUS_LABELS[source.status] || source.status}
              </span>
            </div>
            <div className="skill-desc">{source.value}</div>
            <div className="skill-source-meta">
              <span>{KIND_LABELS[source.type] || source.type}</span>
              <span>{source.skill_count} 个技能</span>
              {source.git_ref ? <span>commit {source.git_ref}</span> : null}
              {source.type === 'http' ? (
                <span>
                  {source.cached_at_unix_ms
                    ? `索引快照：${formatTime(source.cached_at_unix_ms)}`
                    : '尚无索引快照'}
                </span>
              ) : null}
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
              {source.builtin ? null : (
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
              )}
            </div>
          </div>
        ))}
      </div>

      {selected ? <SkillInstallList sourceId={selected} onChanged={load} /> : null}
    </div>
  )
}
