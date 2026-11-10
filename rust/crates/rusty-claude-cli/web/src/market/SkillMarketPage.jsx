import React, { useCallback, useEffect, useMemo, useState } from 'react'
import { api } from '../api.js'
import { parseSkillSub } from '../shared/router.js'
import { SkillDetail } from './SkillDetail.jsx'

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
]

function sourceLabel(source) {
  return SOURCE_LABELS[source] || source || '未知'
}

// 技能市场（P0）：只读目录 + 详情 + SKILL.md 预览，不写任何文件。
export function SkillMarketPage() {
  const [data, setData] = useState(null)
  const [error, setError] = useState('')
  const [loading, setLoading] = useState(true)
  const [filter, setFilter] = useState('all')
  const [keyword, setKeyword] = useState('')
  const [selected, setSelected] = useState(() => parseSkillSub(location.hash).name || '')

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
    const onHashChange = () => setSelected(parseSkillSub(location.hash).name || '')
    window.addEventListener('hashchange', onHashChange)
    return () => window.removeEventListener('hashchange', onHashChange)
  }, [])

  const skills = data?.skills || []
  const visible = useMemo(() => {
    const text = keyword.trim().toLowerCase()
    return skills.filter((skill) => {
      if (filter === 'active' && !skill.active) return false
      if (filter === 'shadowed' && skill.active) return false
      if (!text) return true
      return (
        skill.name.toLowerCase().includes(text) ||
        (skill.description || '').toLowerCase().includes(text)
      )
    })
  }, [skills, filter, keyword])

  const pick = (name) => {
    if (selected === name) return
    setSelected(name)
    history.replaceState(null, '', `#/skills/${encodeURIComponent(name)}`)
  }

  const summary = data?.summary || {}

  return (
    <div className="market-page">
      <div className="market-head">
        <div>
          <h1>技能市场</h1>
          <p>浏览本机已发现的技能，查看 SKILL.md 原文；P0 为只读视图，不安装、不改动文件。</p>
        </div>
        <span className="chip">{summary.active ?? 0} / {summary.total ?? 0} 生效</span>
      </div>

      {summary.install_root ? (
        <div className="skill-install-root">安装目录：{summary.install_root}</div>
      ) : null}

      <div className="skill-toolbar">
        <input
          className="skill-search"
          value={keyword}
          onChange={(event) => setKeyword(event.target.value)}
          placeholder="搜索名称或描述"
        />
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
            <div className="skill-list-empty">没有匹配的技能</div>
          ) : null}
          {visible.map((skill) => (
            <button
              key={`${skill.path}:${skill.name}`}
              type="button"
              className={selected === skill.name ? 'skill-item active' : 'skill-item'}
              onClick={() => pick(skill.name)}
            >
              <div className="skill-item-top">
                <span className="skill-name">{skill.name}</span>
                <span className={`skill-source ${skill.source}`}>{sourceLabel(skill.source)}</span>
              </div>
              <div className="skill-desc">{skill.description || '（无描述）'}</div>
              {!skill.active && skill.shadowed_by ? (
                <div className="skill-shadowed">被{sourceLabel(skill.shadowed_by)}同名技能遮蔽</div>
              ) : null}
            </button>
          ))}
        </div>
        <div className="skill-detail-pane">
          {selected ? (
            <SkillDetail name={selected} />
          ) : (
            <div className="skill-detail-empty">从左侧选择一个技能查看详情</div>
          )}
        </div>
      </div>
    </div>
  )
}
