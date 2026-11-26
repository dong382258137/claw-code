import React, { useCallback, useEffect, useMemo, useState } from 'react'
import { api } from '../api.js'
import { parsePluginSub } from '../shared/router.js'
import { PluginComponents } from './PluginComponents.jsx'
import { PluginDetail } from './PluginDetail.jsx'
import { PluginInstallDialog } from './PluginInstallDialog.jsx'
import { PluginSources } from './PluginSources.jsx'
import { errorText, formatTime, REASON_LABELS } from './pluginUtils.js'

// 每页固定 50 条：2283 条索引只过滤不渲染，避免一次性挂载全部卡片。
const PAGE_SIZE = 50

const TABS = [
  { key: 'list', label: '市场', hash: '#/plugins' },
  { key: 'installed', label: '已安装', hash: '#/plugins/installed' },
  { key: 'sources', label: '来源', hash: '#/plugins/sources' },
]

const FILTERS = [
  { key: 'all', label: '全部' },
  { key: 'installable', label: '可安装' },
  { key: 'blocked', label: '不可安装' },
  { key: 'installed', label: '已安装' },
]

// 插件市场：真实索引 + 搜索/分页 + 详情 + 安装/启停/卸载 + 来源管理。
export function PluginMarketPage() {
  const [sources, setSources] = useState([])
  const [sourceId, setSourceId] = useState('')
  const [index, setIndex] = useState(null)
  const [marketLoading, setMarketLoading] = useState(false)
  const [installed, setInstalled] = useState(null)
  const [installedLoading, setInstalledLoading] = useState(false)
  const [error, setError] = useState(null)
  const [notice, setNotice] = useState('')
  const [keyword, setKeyword] = useState('')
  const [filter, setFilter] = useState('all')
  const [page, setPage] = useState(0)
  const [refreshing, setRefreshing] = useState(false)
  const [busyId, setBusyId] = useState('')
  const [revision, setRevision] = useState(0)
  const [installTarget, setInstallTarget] = useState(null)
  const [selectedInstalledId, setSelectedInstalledId] = useState('')
  const [sub, setSub] = useState(() => parsePluginSub(location.hash))

  const loadSources = useCallback(async () => {
    try {
      const payload = await api('/api/plugin-sources')
      const list = payload.sources || []
      setSources(list)
      setSourceId((prev) =>
        list.some((source) => source.id === prev) ? prev : list[0]?.id || '',
      )
    } catch (err) {
      setError(err)
    }
  }, [])

  const loadMarket = useCallback(async (id) => {
    if (!id) {
      setIndex(null)
      return
    }
    setMarketLoading(true)
    setError(null)
    try {
      setIndex(await api(`/api/plugin-market?source=${encodeURIComponent(id)}`))
    } catch (err) {
      setIndex(null)
      setError(err)
    } finally {
      setMarketLoading(false)
    }
  }, [])

  const loadInstalled = useCallback(async () => {
    setInstalledLoading(true)
    try {
      setInstalled(await api('/api/plugins'))
    } catch (err) {
      setError(err)
    } finally {
      setInstalledLoading(false)
    }
  }, [])

  useEffect(() => {
    loadSources()
  }, [loadSources])

  useEffect(() => {
    loadMarket(sourceId)
  }, [loadMarket, sourceId])

  useEffect(() => {
    if (sub.view === 'installed') loadInstalled()
  }, [loadInstalled, sub.view])

  useEffect(() => {
    const onHashChange = () => setSub(parsePluginSub(location.hash))
    window.addEventListener('hashchange', onHashChange)
    return () => window.removeEventListener('hashchange', onHashChange)
  }, [])

  useEffect(() => {
    setPage(0)
  }, [keyword, filter, sourceId])

  const view =
    sub.view === 'sources'
      ? 'sources'
      : sub.view === 'installed'
        ? 'installed'
        : sub.view === 'detail'
          ? 'detail'
          : 'list'
  const selectedName = sub.view === 'detail' ? sub.name : ''
  const source = sources.find((item) => item.id === sourceId) || null
  const cached = Boolean(index?.cached)

  const plugins = index?.plugins || []
  const installableCount = useMemo(
    () => plugins.filter((plugin) => plugin.installable).length,
    [plugins],
  )

  const filtered = useMemo(() => {
    const text = keyword.trim().toLowerCase()
    return plugins.filter((plugin) => {
      if (filter === 'installable' && !plugin.installable) return false
      if (filter === 'blocked' && plugin.installable) return false
      if (filter === 'installed' && !plugin.installed) return false
      if (!text) return true
      return (
        plugin.name.toLowerCase().includes(text) ||
        (plugin.description || '').toLowerCase().includes(text) ||
        (plugin.category || '').toLowerCase().includes(text)
      )
    })
  }, [plugins, filter, keyword])

  const pages = Math.max(1, Math.ceil(filtered.length / PAGE_SIZE))
  const currentPage = Math.min(page, pages - 1)
  const pageItems = filtered.slice(currentPage * PAGE_SIZE, (currentPage + 1) * PAGE_SIZE)

  const installedPlugins = installed?.plugins || []
  const installedFiltered = useMemo(() => {
    const text = keyword.trim().toLowerCase()
    if (!text) return installedPlugins
    return installedPlugins.filter(
      (plugin) =>
        plugin.name.toLowerCase().includes(text) ||
        (plugin.description || '').toLowerCase().includes(text) ||
        plugin.id.toLowerCase().includes(text),
    )
  }, [installedPlugins, keyword])
  const selectedInstalled =
    installedPlugins.find((plugin) => plugin.id === selectedInstalledId) || null

  const reloadAfterMutation = async () => {
    setRevision((value) => value + 1)
    await Promise.all([loadMarket(sourceId), loadInstalled()])
    await loadSources()
  }

  const goTab = (hash) => {
    history.replaceState(null, '', hash)
    setSub(parsePluginSub(hash))
  }

  const pick = (name) => {
    if (selectedName === name) return
    history.replaceState(null, '', `#/plugins/${encodeURIComponent(name)}`)
    setSub(parsePluginSub(location.hash))
  }

  const refreshSource = async () => {
    if (!sourceId) return
    setRefreshing(true)
    setError(null)
    setNotice('')
    try {
      const updated = await api(
        `/api/plugin-sources/${encodeURIComponent(sourceId)}/refresh`,
        { method: 'POST' },
      )
      setNotice(
        updated.status === 'ok'
          ? `来源已刷新（${updated.plugin_count} 个插件）`
          : `刷新失败：${updated.last_error || updated.status}`,
      )
    } catch (err) {
      setError(err)
    } finally {
      setRefreshing(false)
      await loadSources()
      await loadMarket(sourceId)
    }
  }

  const togglePlugin = async (pluginId, enabled, label) => {
    setBusyId(pluginId)
    setError(null)
    setNotice('')
    try {
      await api(`/api/plugins/${encodeURIComponent(pluginId)}`, {
        method: 'PATCH',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ enabled }),
      })
      setNotice(`${enabled ? '已启用' : '已停用'} ${label}`)
      await reloadAfterMutation()
    } catch (err) {
      setError(err)
    } finally {
      setBusyId('')
    }
  }

  const uninstallPlugin = async (pluginId, label) => {
    if (
      !window.confirm(
        `卸载插件「${label}」？会删除安装目录及其技能副本，且不可恢复。`,
      )
    ) {
      return
    }
    setBusyId(pluginId)
    setError(null)
    setNotice('')
    try {
      await api(`/api/plugins/${encodeURIComponent(pluginId)}`, { method: 'DELETE' })
      if (selectedInstalledId === pluginId) setSelectedInstalledId('')
      setNotice(`已卸载 ${label}`)
      await reloadAfterMutation()
    } catch (err) {
      setError(err)
    } finally {
      setBusyId('')
    }
  }

  const handleInstalled = async (report) => {
    setNotice(
      `已安装 ${report.name} v${report.version}：导入 ${report.components.skill_count} 个技能、` +
        `${report.components.mcp_server_count} 个 MCP 服务` +
        (report.components.unsupported.length
          ? `，${report.components.unsupported.length} 项组件暂不支持`
          : ''),
    )
    await reloadAfterMutation()
  }

  const headChip = index
    ? `${index.source.name || index.source.id} · ${plugins.length} 条 · ${installableCount} 可安装`
    : '未加载'

  return (
    <div className="market-page">
      <div className="market-head">
        <div>
          <h1>插件市场</h1>
          <p>
            从公开或自建来源浏览插件；安装会把插件声明的技能、Agent、命令、Hook 与 MCP
            服务接入当前环境。
          </p>
        </div>
        <span className="chip">{headChip}</span>
      </div>

      <div className="skill-tabs">
        {TABS.map((tab) => (
          <button
            key={tab.key}
            type="button"
            className={
              view === tab.key || (tab.key === 'list' && view === 'detail')
                ? 'skill-tab active'
                : 'skill-tab'
            }
            onClick={() => goTab(tab.hash)}
          >
            {tab.label}
          </button>
        ))}
      </div>

      {notice ? <div className="skill-notice">{notice}</div> : null}
      {error ? (
        <div className="skill-error">
          <span>{errorText(error)}</span>
          <button type="button" onClick={() => (view === 'installed' ? loadInstalled() : loadMarket(sourceId))}>
            重试
          </button>
        </div>
      ) : null}

      {view === 'sources' ? <PluginSources onChanged={loadSources} /> : null}

      {view !== 'sources' ? (
        <>
          <div className="skill-toolbar">
            <select
              className="plugin-source-select"
              value={sourceId}
              onChange={(event) => setSourceId(event.target.value)}
            >
              {sources.length === 0 ? <option value="">没有可用来源</option> : null}
              {sources.map((item) => (
                <option key={item.id} value={item.id}>
                  {item.name || item.id}
                  {item.trusted ? '' : '（未受信任）'}
                </option>
              ))}
            </select>
            <input
              className="skill-search"
              value={keyword}
              onChange={(event) => setKeyword(event.target.value)}
              placeholder={view === 'installed' ? '搜索已安装插件' : '搜索名称、描述或分类'}
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
            <button
              type="button"
              className="skill-refresh"
              onClick={refreshSource}
              disabled={refreshing || !sourceId}
            >
              {refreshing ? '刷新中…' : '刷新来源'}
            </button>
          </div>

          {cached ? (
            <div className="plugin-cache-banner">
              <strong>当前显示的是缓存索引，最后更新于 {formatTime(index?.cached_at_unix_ms) || '未知时间'}，不是实时数据。</strong>
              <span>安装与卸载前的校验以实时来源为准，缓存状态下安装已停用。</span>
              <button type="button" className="skill-refresh" onClick={refreshSource} disabled={refreshing}>
                立即刷新
              </button>
            </div>
          ) : index && index.source.status !== 'ok' ? (
            <div className="skill-notice warn">
              来源状态：{index.source.status}
              {index.source.last_error ? ` · ${index.source.last_error}` : ''}
            </div>
          ) : null}

          {view === 'installed' ? (
            <div className="skill-layout">
              <div className="plugin-column">
                <div className="skill-list">
                  {installedLoading ? <div className="skill-list-empty">加载中…</div> : null}
                  {!installedLoading && installedFiltered.length === 0 ? (
                    <div className="skill-list-empty">还没有安装插件</div>
                  ) : null}
                  {installedFiltered.map((plugin) => (
                    <div
                      key={plugin.id}
                      className={
                        selectedInstalledId === plugin.id
                          ? 'skill-card active'
                          : plugin.enabled
                            ? 'skill-card'
                            : 'skill-card disabled'
                      }
                    >
                      <button
                        type="button"
                        className="skill-item"
                        onClick={() => setSelectedInstalledId(plugin.id)}
                      >
                        <div className="skill-item-top">
                          <span className="skill-name">{plugin.name}</span>
                          <span className="skill-source">{plugin.enabled ? '已启用' : '已停用'}</span>
                        </div>
                        <div className="skill-desc">{plugin.description || '（无描述）'}</div>
                        <div className="skill-managed">
                          v{plugin.version} · {plugin.kind} · {plugin.components.skill_count} 技能 ·{' '}
                          {plugin.components.mcp_server_count} MCP
                        </div>
                      </button>
                      <button
                        type="button"
                        className={plugin.enabled ? 'skill-toggle on' : 'skill-toggle'}
                        disabled={busyId === plugin.id}
                        onClick={() =>
                          togglePlugin(plugin.id, !plugin.enabled, plugin.name)
                        }
                      >
                        {plugin.enabled ? '停用' : '启用'}
                      </button>
                    </div>
                  ))}
                </div>
                {installed?.failures?.length ? (
                  <div className="skill-notice warn">
                    有 {installed.failures.length} 个插件加载失败：{installed.failures.join('；')}
                  </div>
                ) : null}
              </div>
              <div className="skill-detail-pane">
                {selectedInstalled ? (
                  <div className="skill-detail">
                    <div className="skill-detail-head">
                      <div>
                        <h2>{selectedInstalled.name}</h2>
                        <p>{selectedInstalled.description || '（无描述）'}</p>
                      </div>
                      <div className="skill-detail-actions">
                        <button
                          type="button"
                          className="skill-refresh"
                          disabled={busyId === selectedInstalled.id}
                          onClick={() =>
                            togglePlugin(
                              selectedInstalled.id,
                              !selectedInstalled.enabled,
                              selectedInstalled.name,
                            )
                          }
                        >
                          {selectedInstalled.enabled ? '停用' : '启用'}
                        </button>
                        <button
                          type="button"
                          className="skill-danger"
                          disabled={busyId === selectedInstalled.id}
                          onClick={() =>
                            uninstallPlugin(selectedInstalled.id, selectedInstalled.name)
                          }
                        >
                          卸载
                        </button>
                      </div>
                    </div>
                    <dl className="skill-meta">
                      <dt>插件 ID</dt>
                      <dd className="skill-path">{selectedInstalled.id}</dd>
                      <dt>版本</dt>
                      <dd>v{selectedInstalled.version}</dd>
                      <dt>类型</dt>
                      <dd>{selectedInstalled.kind}</dd>
                      <dt>来源</dt>
                      <dd>{selectedInstalled.source || '未知'}</dd>
                      <dt>根目录</dt>
                      <dd className="skill-path">{selectedInstalled.root || '未知'}</dd>
                      <dt>状态</dt>
                      <dd>{selectedInstalled.enabled ? '已启用' : '已停用'}</dd>
                    </dl>
                    <PluginComponents components={selectedInstalled.components} />
                  </div>
                ) : (
                  <div className="skill-detail-empty">从左侧选择一个已安装插件查看组件</div>
                )}
              </div>
            </div>
          ) : (
            <div className="skill-layout">
              <div className="plugin-column">
                <div className="skill-list">
                  {marketLoading ? <div className="skill-list-empty">加载索引中…</div> : null}
                  {!marketLoading && !error && pageItems.length === 0 ? (
                    <div className="skill-list-empty">
                      {plugins.length === 0 ? '这个来源没有插件' : '没有匹配的插件'}
                    </div>
                  ) : null}
                  {pageItems.map((plugin) => (
                    <div
                      key={plugin.name}
                      className={
                        selectedName === plugin.name ? 'skill-card active' : 'skill-card'
                      }
                    >
                      <button
                        type="button"
                        className="skill-item"
                        onClick={() => pick(plugin.name)}
                      >
                        <div className="skill-item-top">
                          <span className="skill-name">{plugin.name}</span>
                          {plugin.installed ? (
                            <span className="skill-source">
                              {plugin.installed.enabled ? '已安装' : '已停用'}
                            </span>
                          ) : null}
                          {!plugin.installable ? (
                            <span className="skill-source-status error">不可安装</span>
                          ) : null}
                        </div>
                        <div className="skill-desc">{plugin.description || '（无描述）'}</div>
                        <div className="skill-managed">
                          {[plugin.version ? `v${plugin.version}` : null, plugin.category, plugin.source?.kind]
                            .filter(Boolean)
                            .join(' · ')}
                        </div>
                        {!plugin.installable ? (
                          <div className="skill-source-error">
                            原因 {REASON_LABELS[plugin.reason] || plugin.reason}（{plugin.reason}）
                          </div>
                        ) : null}
                      </button>
                    </div>
                  ))}
                </div>
                <div className="plugin-pager">
                  <span>
                    共 {filtered.length} 条 · 第 {currentPage + 1} / {pages} 页
                  </span>
                  <button
                    type="button"
                    className="skill-filter"
                    disabled={currentPage === 0}
                    onClick={() => setPage(currentPage - 1)}
                  >
                    上一页
                  </button>
                  <button
                    type="button"
                    className="skill-filter"
                    disabled={currentPage >= pages - 1}
                    onClick={() => setPage(currentPage + 1)}
                  >
                    下一页
                  </button>
                </div>
              </div>
              <div className="skill-detail-pane">
                {selectedName ? (
                  <PluginDetail
                    sourceId={sourceId}
                    name={selectedName}
                    revision={revision}
                    cached={cached}
                    cachedAt={index?.cached_at_unix_ms}
                    busyId={busyId}
                    onInstall={setInstallTarget}
                    onToggle={togglePlugin}
                    onUninstall={uninstallPlugin}
                  />
                ) : (
                  <div className="skill-detail-empty">从左侧选择一个插件查看详情</div>
                )}
              </div>
            </div>
          )}
        </>
      ) : null}

      {installTarget ? (
        <PluginInstallDialog
          source={source || { id: sourceId, trusted: false, name: sourceId }}
          entry={installTarget}
          onClose={() => setInstallTarget(null)}
          onInstalled={handleInstalled}
        />
      ) : null}
    </div>
  )
}
