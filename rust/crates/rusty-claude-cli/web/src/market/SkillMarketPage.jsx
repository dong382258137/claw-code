import React, { useCallback, useEffect, useMemo, useState } from 'react'
import { api } from '../api.js'
import { parseSkillSub } from '../shared/router.js'
import { SkillDetail } from './SkillDetail.jsx'
import { SkillSources } from './SkillSources.jsx'

const SOURCE_LABELS = {
  project: '项目',
  user_config: '用户配置',
  user: '用户目录',
  builtin: '内置',
}

const FILTERS = [
  { key: 'all', label: '全部' },
  { key: 'active', label: '生效中' },
  { key: 'shadowed', label: '被遮蔽' },
  { key: 'disabled', label: '已禁用' },
]

const TABS = [
  { key: 'list', label: '本地技能', hash: '#/skills' },
  { key: 'installed', label: '已安装', hash: '#/skills/installed' },
  { key: 'sources', label: '来源市场', hash: '#/skills/sources' },
]

function sourceLabel(source) {
  return SOURCE_LABELS[source] || source || '未知'
}

// 技能市场（P2）：本地目录 + 来源管理 + 安装/卸载 + 启停。
export function SkillMarketPage() {
  const [data, setData] = useState(null)
  const [error, setError] = useState('')
  const [loading, setLoading] = useState(true)
  const [filter, setFilter] = useState('all')
  const [keyword, setKeyword] = useState('')
  const [sub, setSub] = useState(() => parseSkillSub(location.hash))
  const [notice, setNotice] = useState('')
  const [toggling, setToggling] = useState('')

  const load = useCallback(async () => {
    setLoading(true)
    setError('')
    try {
      setData(await api('/api/skills'))
    } catch (err) {
      setError(err.message || String(err))
    } finally {
      setLoading(false)
    }
  }, [])

  useEffect(() => {
    load()
  }, [load])

  useEffect(() => {
    const onHashChange = () => setSub(parseSkillSub(location.hash))
    window.addEventListener('hashchange', onHashChange)
    return () => window.removeEventListener('hashchange', onHashChange)
  }, [])

  const view = sub.view === 'sources' ? 'sources' : sub.view === 'installed' ? 'installed' : 'list'
  const selected = sub.view === 'detail' ? sub.name : ''

  const skills = data?.skills || []
  const visible = useMemo(() => {
    const text = keyword.trim().toLowerCase()
    return skills.filter((skill) => {
      if (view === 'installed' && !skill.managed) return false
      if (view === 'list') {
        if (filter === 'active' && !skill.active) return false
        if (filter === 'shadowed' && skill.active) return false
        if (filter === 'disabled' && skill.enabled) return false
      }
      if (!text) return true
      return (
        skill.name.toLowerCase().includes(text) ||
        (skill.description || '').toLowerCase().includes(text)
      )
    })
  }, [skills, view, filter, keyword])

  const pick = (name) => {
    if (selected === name) return
    history.replaceState(null, '', `#/skills/${encodeURIComponent(name)}`)
    setSub(parseSkillSub(location.hash))
  }

  const goTab = (hash) => {
    history.replaceState(null, '', hash)
    setSub(parseSkillSub(hash))
  }

  const afterRemove = () => {
    setNotice('已卸载')
    history.replaceState(null, '', '#/skills/installed')
    setSub(parseSkillSub(location.hash))
    load()
  }

  const toggle = async (skill) => {
    if (toggling) return
    const next = !skill.enabled
    if (!next && !window.confirm(`禁用 ${skill.name} 后，它不会出现在技能列表和系统提示里。继续？`)) {
      return
    }
    setToggling(skill.name)
    setNotice('')
    try {
      const result = await api(`/api/skills/${encodeURIComponent(skill.name)}`, {
        method: 'PATCH',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ enabled: next }),
      })
      setNotice(result.enabled ? `已启用 ${skill.name}` : `已禁用 ${skill.name}`)
      await load()
    } catch (err) {
      setError(err.message || String(err))
    } finally {
      setToggling('')
    }
  }

  const summary = data?.summary || {}

  return (
    <div className="market-page">
      <div className="market-head">
        <div>
          <h1>技能市场</h1>
          <p>浏览本机技能，从本地索引来源安装或卸载；卸载只会删除市场安装的副本。</p>
        </div>
        <span className="chip">
          {summary.active ?? 0} / {summary.total ?? 0} 生效 · {summary.disabled ?? 0} 已禁用 ·{' '}
          {summary.managed ?? 0} 已安装
        </span>
      </div>

      <div className="skill-tabs">
        {TABS.map((tab) => (
          <button
            key={tab.key}
            type="button"
            className={view === tab.key || (tab.key === 'list' && sub.view === 'detail') ? 'skill-tab active' : 'skill-tab'}
            onClick={() => goTab(tab.hash)}
          >
            {tab.label}
          </button>
        ))}
      </div>

      {summary.install_root ? (
        <div className="skill-install-root">安装目录：{summary.install_root}</div>
      ) : null}

      {notice ? <div className="skill-notice">{notice}</div> : null}

      {view === 'sources' ? <SkillSources /> : null}

      {view !== 'sources' ? (
        <>
          <div className="skill-toolbar">
            <input
              className="skill-search"
              value={keyword}
              onChange={(event) => setKeyword(event.target.value)}
              placeholder="搜索名称或描述"
            />
            {view === 'list' ? (
              <div className="skill-filters">
                {FILTERS.map((item) => (
                  <button
                    key={item.key}
                    type="button"
                    className={filter === item.key ? 'skill-filter active' : 'skill-filter'}
                    onClick={() => setFilter(item.key)}
                  >
                    {item.label}
                  </button>
                ))}
              </div>
            ) : null}
            <button type="button" className="skill-refresh" onClick={load} disabled={loading}>
              {loading ? '加载中…' : '刷新'}
            </button>
          </div>

          {error ? (
            <div className="skill-error">
              <span>{error}</span>
              <button type="button" onClick={load}>重试</button>
            </div>
          ) : null}

          <div className="skill-layout">
            <div className="skill-list">
              {!loading && !error && visible.length === 0 ? (
                <div className="skill-list-empty">
                  {view === 'installed' ? '还没有从市场安装的技能' : '没有匹配的技能'}
                </div>
              ) : null}
              {visible.map((skill) => (
                <div
                  key={`${skill.path}:${skill.name}`}
                  className={
                    selected === skill.name
                      ? 'skill-card active'
                      : skill.enabled
                        ? 'skill-card'
                        : 'skill-card disabled'
                  }
                >
                  <button
                    type="button"
                    className="skill-item"
                    onClick={() => pick(skill.name)}
                  >
                    <div className="skill-item-top">
                      <span className="skill-name">{skill.name}</span>
                      <span className={`skill-source ${skill.source}`}>{sourceLabel(skill.source)}</span>
                    </div>
                    <div className="skill-desc">{skill.description || '（无描述）'}</div>
                    {skill.managed ? (
                      <div className="skill-managed">
                        市场安装 · {skill.marketplace || '未知来源'} · v{skill.version || '未知'}
                      </div>
                    ) : null}
                    {!skill.enabled ? <div className="skill-disabled">已禁用</div> : null}
                    {!skill.active && skill.shadowed_by ? (
                      <div className="skill-shadowed">被{sourceLabel(skill.shadowed_by)}同名技能遮蔽</div>
                    ) : null}
                  </button>
                  <button
                    type="button"
                    className={skill.enabled ? 'skill-toggle on' : 'skill-toggle'}
                    title={skill.enabled ? '禁用' : '启用'}
                    aria-label={skill.enabled ? `禁用 ${skill.name}` : `启用 ${skill.name}`}
                    disabled={toggling === skill.name}
                    onClick={() => toggle(skill)}
                  >
                    {skill.enabled ? '禁用' : '启用'}
                  </button>
                </div>
              ))}
            </div>
            <div className="skill-detail-pane">
              {selected ? (
                <SkillDetail name={selected} onRemoved={afterRemove} />
              ) : (
                <div className="skill-detail-empty">从左侧选择一个技能查看详情</div>
              )}
            </div>
          </div>
        </>
      ) : null}
    </div>
  )
}
