import React, { useCallback, useEffect, useState } from 'react'
import { api, postJson } from '../api.js'

const REASON_LABELS = {
  missing_name: '缺少 name',
  missing_path: '缺少 path',
  path_not_found: '路径不存在',
  path_escape: '路径越出来源目录',
  missing_skill_md: '没有 SKILL.md',
  name_mismatch: '名称不一致',
  incompatible: '版本不兼容',
  too_large: '目录过大',
  checksum_mismatch: '校验失败',
  missing_integrity_anchor: '没有可锚定的 commit',
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

// 某个来源的技能索引：逐条安装/升级。
export function SkillInstallList({ sourceId, onChanged }) {
  const [market, setMarket] = useState(null)
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState('')
  const [busy, setBusy] = useState('')
  const [notice, setNotice] = useState('')

  const load = useCallback(async () => {
    setLoading(true)
    setError('')
    try {
      setMarket(await api(`/api/skill-market?source=${encodeURIComponent(sourceId)}`))
    } catch (err) {
      setError(err.message || String(err))
    } finally {
      setLoading(false)
    }
  }, [sourceId])

  useEffect(() => {
    load()
  }, [load])

  const install = async (skill) => {
    const upgrade = Boolean(skill.installed)
    if (
      upgrade &&
      !window.confirm(
        `已安装 ${skill.installed.version}，将覆盖为 ${skill.version}。继续升级安装？`,
      )
    ) {
      return
    }
    setBusy(skill.name)
    setError('')
    setNotice('')
    try {
      const result = await postJson('/api/skills/install', {
        source_id: sourceId,
        name: skill.name,
        version: skill.version,
        upgrade,
      })
      setNotice(
        result.shadowed_by
          ? `已安装 ${result.name}，但它被同名技能遮蔽，当前实际生效的不是这份副本`
          : `已安装 ${result.name} ${result.version}`,
      )
      await load()
      onChanged?.()
    } catch (err) {
      setError(err.message || String(err))
    } finally {
      setBusy('')
    }
  }

  if (loading) return <div className="skill-list-empty">加载索引中…</div>
  if (error) {
    return (
      <div className="skill-error">
        <span>{error}</span>
        <button type="button" onClick={load}>重试</button>
      </div>
    )
  }
  if (!market) return null

  return (
    <div className="skill-install-list">
      <div className="skill-install-head">
        <h3>{market.source.name || market.source.id}</h3>
        <div className="skill-install-badges">
          {market.source.index_format ? (
            <span className="chip">
              {FORMAT_LABELS[market.source.index_format] || market.source.index_format}
            </span>
          ) : null}
          {market.source.resolved_ref ? (
            <span className="chip">commit {market.source.resolved_ref}</span>
          ) : null}
          <span className="chip">{market.skills.length} 个条目</span>
        </div>
      </div>
      {market.cached ? (
        <div className="skill-notice warn">
          {market.live
            ? `来源刷新失败${market.source.last_error ? `：${market.source.last_error}` : ''}。以下内容来自本地镜像`
            : market.skills.length
              ? `索引当前不可用${market.source.last_error ? `：${market.source.last_error}` : ''}。以下为上次缓存内容，只能查看，无法安装`
              : `索引当前不可用${market.source.last_error ? `：${market.source.last_error}` : ''}。暂无可用缓存`}
          {market.cached_at_unix_ms ? `（最后更新于 ${formatTime(market.cached_at_unix_ms)}）` : ''}
        </div>
      ) : market.source.status !== 'ok' ? (
        <div className="skill-notice warn">{market.source.last_error || market.source.status}</div>
      ) : null}
      {notice ? <div className="skill-notice">{notice}</div> : null}

      {market.skills.length === 0 ? (
        <div className="skill-list-empty">
          {market.live ? '这个来源没有可安装的技能。' : '索引不可用，也没有可用的缓存内容。'}
        </div>
      ) : null}

      {market.skills.map((skill) => (
        <div key={skill.name} className="skill-install-row">
          <div className="skill-install-main">
            <div className="skill-item-top">
              <span className="skill-name">{skill.name}</span>
              <span className="skill-version">{skill.version}</span>
              {skill.installed ? (
                <span className="skill-source-status ok">已安装 {skill.installed.version}</span>
              ) : null}
            </div>
            <div className="skill-desc">{skill.description || '（无描述）'}</div>
            {!skill.installable ? (
              <div className="skill-source-error">
                不可安装：{REASON_LABELS[skill.reason] || skill.reason}
                {skill.reason_detail ? ` · ${skill.reason_detail}` : ''}
              </div>
            ) : null}
          </div>
          <button
            type="button"
            className="skill-refresh"
            disabled={!skill.installable || !market.live || busy === skill.name}
            onClick={() => install(skill)}
          >
            {busy === skill.name ? '安装中…' : skill.installed ? '升级安装' : '安装'}
          </button>
        </div>
      ))}
    </div>
  )
}
