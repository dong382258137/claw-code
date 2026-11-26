import React, { useEffect, useState } from 'react'
import { api } from '../api.js'
import { errorText, formatTime, REASON_LABELS } from './pluginUtils.js'

// 市场条目详情：字段来自 /api/plugin-market/detail；不可安装时展示原因码与细节，
// 已安装条目提供启用/停用与卸载。
export function PluginDetail({
  sourceId,
  name,
  revision,
  cached,
  cachedAt,
  busyId,
  onInstall,
  onToggle,
  onUninstall,
}) {
  const [detail, setDetail] = useState(null)
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState(null)

  useEffect(() => {
    if (!sourceId || !name) return undefined
    let cancelled = false
    setLoading(true)
    setError(null)
    api(
      `/api/plugin-market/detail?source=${encodeURIComponent(sourceId)}&name=${encodeURIComponent(name)}`,
    )
      .then((payload) => {
        if (!cancelled) setDetail(payload)
      })
      .catch((err) => {
        if (!cancelled) setError(err)
      })
      .finally(() => {
        if (!cancelled) setLoading(false)
      })
    return () => {
      cancelled = true
    }
  }, [sourceId, name, revision])

  if (loading) return <div className="skill-detail-empty">加载中…</div>
  if (error) return <div className="skill-error"><span>{errorText(error)}</span></div>
  if (!detail) return null

  const plugin = detail.plugin
  const installed = plugin.installed
  const busy = installed ? busyId === installed.plugin_id : false

  return (
    <div className="skill-detail">
      <div className="skill-detail-head">
        <div>
          <h2>{plugin.name}</h2>
          <p>{plugin.description || '（无描述）'}</p>
        </div>
        <div className="skill-detail-actions">
          {installed ? (
            <>
              <button
                type="button"
                className="skill-refresh"
                disabled={busy}
                onClick={() => onToggle?.(installed.plugin_id, !installed.enabled, plugin.name)}
              >
                {installed.enabled ? '停用' : '启用'}
              </button>
              <button
                type="button"
                className="skill-danger"
                disabled={busy}
                onClick={() => onUninstall?.(installed.plugin_id, plugin.name)}
              >
                {busy ? '处理中…' : '卸载'}
              </button>
            </>
          ) : (
            <button
              type="button"
              className="skill-refresh"
              disabled={!plugin.installable || cached}
              title={cached ? '索引来自缓存，刷新来源后才能安装' : undefined}
              onClick={() => onInstall?.(plugin)}
            >
              安装
            </button>
          )}
        </div>
      </div>

      <dl className="skill-meta">
        <dt>版本</dt>
        <dd>{plugin.version || '未声明'}</dd>
        <dt>市场</dt>
        <dd>
          {detail.marketplace || '未知'}
          {detail.owner ? ` · ${detail.owner}` : ''}
        </dd>
        <dt>分类</dt>
        <dd>{plugin.category || '未分类'}</dd>
        <dt>来源形态</dt>
        <dd>{plugin.source?.kind || '未知'}</dd>
        {plugin.source?.url ? (
          <>
            <dt>地址</dt>
            <dd className="skill-path">{plugin.source.url}</dd>
          </>
        ) : null}
        {plugin.source?.path ? (
          <>
            <dt>仓库路径</dt>
            <dd className="skill-path">{plugin.source.path}</dd>
          </>
        ) : null}
        {plugin.source?.ref ? (
          <>
            <dt>ref</dt>
            <dd className="skill-path">{plugin.source.ref}</dd>
          </>
        ) : null}
        {plugin.source?.sha ? (
          <>
            <dt>sha</dt>
            <dd className="skill-path">{plugin.source.sha}</dd>
          </>
        ) : null}
        {plugin.source?.sha256 ? (
          <>
            <dt>sha256</dt>
            <dd className="skill-path">{plugin.source.sha256}</dd>
          </>
        ) : null}
        {plugin.homepage ? (
          <>
            <dt>主页</dt>
            <dd className="skill-path">{plugin.homepage}</dd>
          </>
        ) : null}
        {plugin.declared_skills?.length ? (
          <>
            <dt>声明技能</dt>
            <dd>{plugin.declared_skills.join('、')}</dd>
          </>
        ) : null}
        <dt>状态</dt>
        <dd>
          {installed
            ? `已安装 v${installed.version} · ${installed.enabled ? '已启用' : '已停用'}`
            : plugin.installable
              ? '可安装'
              : '不可安装'}
        </dd>
      </dl>

      {!plugin.installable ? (
        <div className="plugin-reason">
          不可安装：{REASON_LABELS[plugin.reason] || '未通过校验'}（<code>{plugin.reason}</code>）
          {plugin.reason_detail ? <div className="skill-desc">{plugin.reason_detail}</div> : null}
        </div>
      ) : null}

      {cached ? (
        <div className="skill-notice warn">
          当前索引来自缓存{cachedAt ? `（最后更新于 ${formatTime(cachedAt)}）` : ''}
          ，安装已停用；请先刷新来源。
        </div>
      ) : null}
    </div>
  )
}
