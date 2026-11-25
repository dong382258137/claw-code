import React, { useEffect, useMemo, useState } from 'react'
import { basename, relTime } from '../shared/utils.js'

export function Sidebar({ sessions, filter, onFilter, currentId, collapsed, onToggle, onSelect, onNew, onNewIn, onAddWorkspace, onOpenPicker, workspaces, activeWorkspace, onRename, onDelete, onDeleteMany, total, onLoadMore, onSettings, onTheme, theme, route = 'chat', onNavigate }) {
  // 侧栏项目组默认全部收起：首次打开只列出项目本身，便于先选工作区再看会话。
  // 这里只记录用户手动「展开」过的项目（空集 = 全收起），比记录收起集合少一次
  // 依赖 sessions 异步加载的初始化时序。搜索时忽略收起状态，否则命中的会话会被
  // 藏在收起的分组里、搜索看起来像失效。
  const [expandedGroups, setExpandedGroups] = useState(() => new Set())
  // 多选删除状态只活在侧栏：进入多选后点行 = 勾选而不是切换会话，
  // 因此仍保留了「切到某条会话」的语义（不勾选直接看内容需要先退出多选）。
  const [selecting, setSelecting] = useState(false)
  const [selected, setSelected] = useState(() => new Set())
  const query = filter.trim().toLowerCase()
  const filtered = sessions.filter((session) =>
    !query ||
    (session.title || '').toLowerCase().includes(query) ||
    session.id.toLowerCase().includes(query) ||
    (session.workspace || '').toLowerCase().includes(query))

  // 列表刷新（10s 轮询）或删除后，丢弃已不存在的选中项，避免残留幽灵选中。
  useEffect(() => {
    const alive = new Set(sessions.map((session) => session.id))
    setSelected((current) => {
      if (current.size === 0) return current
      const next = new Set([...current].filter((id) => alive.has(id)))
      return next.size === current.size ? current : next
    })
  }, [sessions])

  // 搜索词变化后丢弃被过滤掉的选中项：否则「全选」改搜索再删，数量会和所见不一致。
  useEffect(() => {
    const allowed = new Set(filtered.map((session) => session.id))
    setSelected((current) => {
      if (current.size === 0) return current
      const next = new Set([...current].filter((id) => allowed.has(id)))
      return next.size === current.size ? current : next
    })
  }, [query]) // 只在搜索词变化时裁剪，filtered 随 query 一起变

  // 切到别的会话 = 用户离开这批选择；清空列表也自动退出多选，避免空列表里挂着删除按钮。
  useEffect(() => {
    setSelecting(false)
    setSelected(new Set())
  }, [currentId])

  useEffect(() => {
    if (sessions.length === 0) setSelecting(false)
  }, [sessions.length])

  const projects = useMemo(() => {
    const order = []
    for (const item of workspaces) {
      if (item.path && !order.includes(item.path)) order.push(item.path)
    }
    for (const session of sessions) {
      if (session.workspace && !order.includes(session.workspace)) order.push(session.workspace)
    }
    return order.map((path) => ({
      path,
      label: basename(path) || path,
      sessions: filtered.filter((session) => session.workspace === path),
      count: sessions.filter((session) => session.workspace === path).length,
    })).filter((project) => !query || project.sessions.length > 0)
  }, [workspaces, sessions, filtered, query])

  const toggleGroup = (path) => {
    setExpandedGroups((current) => {
      const next = new Set(current)
      if (next.has(path)) next.delete(path)
      else next.add(path)
      return next
    })
  }

  const toggleSelecting = () => {
    if (selecting) {
      setSelecting(false)
      setSelected(new Set())
      return
    }
    // 进入多选先展开全部项目，保证待勾选的会话都可见；之后仍可手动收起。
    setExpandedGroups((current) => new Set([...current, ...projects.map((project) => project.path)]))
    setSelecting(true)
  }

  const toggleOne = (id) => {
    setSelected((current) => {
      const next = new Set(current)
      if (next.has(id)) next.delete(id)
      else next.add(id)
      return next
    })
  }

  const allSelected = filtered.length > 0 && filtered.every((session) => selected.has(session.id))
  const toggleAll = () => {
    setSelected(allSelected ? new Set() : new Set(filtered.map((session) => session.id)))
  }

  const deleteSelected = () => {
    const ids = sessions.filter((session) => selected.has(session.id)).map((session) => session.id)
    if (ids.length === 0) return
    if (!window.confirm(`确定删除选中的 ${ids.length} 个会话？此操作不可恢复。`)) return
    onDeleteMany?.(ids)
    setSelecting(false)
    setSelected(new Set())
  }

  return (
    <aside className={collapsed ? 'collapsed' : ''}>
      <div className="brand">
        <span><b>CLAW</b></span>
        {!collapsed && (
          <div className="brand-actions">
            <button onClick={() => onNew(activeWorkspace)} title="在当前工作区新建任务">＋</button>
            <button onClick={onTheme} title="切换主题">{theme === 'dark' ? '◐' : '◑'}</button>
            <button onClick={onSettings} title="设置">⚙</button>
            <button onClick={onToggle} title="收起侧栏 (Ctrl+B)">‹</button>
          </div>
        )}
      </div>
      {collapsed && (
        <div className="rail">
          <button onClick={() => onNew(activeWorkspace)} title="新建任务">＋</button>
          <button onClick={onToggle} title="展开侧栏 (Ctrl+B)">☰</button>
          <button onClick={onSettings} title="设置">⚙</button>
          <button onClick={onTheme} title="切换主题">◐</button>
        </div>
      )}
      {!collapsed && (
        <>
          <div className="nav-tabs">
            <button className={route === 'chat' ? 'active' : ''} onClick={() => onNavigate?.('chat')}>对话</button>
            <button className={route === 'plugins' ? 'active' : ''} onClick={() => onNavigate?.('plugins')}>插件市场</button>
            <button className={route === 'skills' ? 'active' : ''} onClick={() => onNavigate?.('skills')}>技能市场</button>
          </div>
          <button className="new-session" onClick={() => onNew(activeWorkspace)}>＋ 新建任务</button>
          <div className="search">
            <span className="search-icon" aria-hidden="true">⌕</span>
            <input id="session-search" value={filter} onChange={(event) => onFilter(event.target.value)} placeholder="搜索会话" />
            <kbd className="search-kbd">Ctrl K</kbd>
          </div>
          <div className="projects-head">
            <span className="projects-label">项目 <em>{projects.length}</em></span>
            <span className="projects-head-actions">
              <button
                className={`batch-toggle ${selecting ? 'active' : ''}`}
                title={selecting ? '退出多选' : '多选会话，批量删除'}
                onClick={toggleSelecting}
              >{selecting ? '取消' : '多选'}</button>
              <button
                title="选择工作区文件夹"
                onClick={onOpenPicker}
              >＋</button>
            </span>
          </div>
          {selecting && (
            <div className="batch-bar">
              <button onClick={toggleAll}>{allSelected ? '取消全选' : '全选'}</button>
              <span className="batch-count">已选 {selected.size} 项</span>
              <button className="danger" disabled={selected.size === 0} onClick={deleteSelected}>删除</button>
            </div>
          )}
          <div className="session-list">
            {sessions.length === 0 && (
              <div className="empty">
                还没有会话
                <div className="empty-hint">点上方「＋ 新建任务」开始</div>
              </div>
            )}
            {sessions.length > 0 && projects.length === 0 && <div className="empty">没有匹配的会话</div>}
            {sessions.length > 0 && projects.map((project) => {
              // 搜索时强制展开：否则命中的会话藏在收起的项目里，搜索看起来像失效。
              const isExpanded = expandedGroups.has(project.path) || Boolean(query)
              const active = project.path === activeWorkspace
              return (
                <div className={`project ${active ? 'active-workspace' : ''}`} key={project.path}>
                  <div className="project-head" title={project.path}>
                    <button className="project-toggle" onClick={() => toggleGroup(project.path)}>
                      <span className="project-caret" aria-hidden="true">{isExpanded ? '▾' : '▸'}</span>
                      <span className="project-folder" aria-hidden="true">📁</span>
                      <span className="project-name">{project.label}</span>
                      <span className="project-count">{project.count}</span>
                    </button>
                    <button
                      className="project-new"
                      title={`在 ${project.label} 新建任务`}
                      onClick={() => onNewIn(project.path)}
                    >＋</button>
                  </div>
                  {isExpanded && project.sessions.map((session) => {
                    const checked = selected.has(session.id)
                    return (
                      <div key={session.id} className={`session-row nested ${session.id === currentId ? 'active' : ''} ${checked ? 'selected' : ''}`}>
                        {selecting && (
                          <label className="session-check">
                            <input type="checkbox" checked={checked} onChange={() => toggleOne(session.id)} />
                          </label>
                        )}
                        <button
                          className="session"
                          onClick={() => (selecting ? toggleOne(session.id) : onSelect(session))}
                          title={session.workspace || ''}
                        >
                          <span className="title">{session.title || session.id}</span>
                          <span className="meta">
                            {/* 并行任务可见：不打开会话也能看出它在跑 / 在排队。 */}
                            {session.state === 'running' && (
                              <span className="session-state running">运行中{session.queued > 0 ? ` · 队列 ${session.queued}` : ''}</span>
                            )}
                            {session.state === 'queued' && (
                              <span className="session-state queued">排队中{session.position > 0 ? `（第 ${session.position} 位）` : ''}</span>
                            )}
                            <span>{relTime(session.modified)}</span>
                          </span>
                        </button>
                        {!selecting && (
                          <span className="session-actions">
                            <button title="重命名" onClick={(event) => { event.stopPropagation(); onRename(session) }}>✎</button>
                            <button title="删除" onClick={(event) => { event.stopPropagation(); onDelete(session) }}>×</button>
                          </span>
                        )}
                      </div>
                    )
                  })}
                </div>
              )
            })}
            {total > sessions.length && (
              <div className="session-more">
                <button onClick={onLoadMore}>加载更多（{sessions.length}/{total}）</button>
              </div>
            )}
          </div>
        </>
      )}
    </aside>
  )
}
