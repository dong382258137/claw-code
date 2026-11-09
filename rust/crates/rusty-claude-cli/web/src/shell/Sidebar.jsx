import React, { useMemo, useState } from 'react'
import { basename, relTime } from '../shared/utils.js'

export function Sidebar({ sessions, filter, onFilter, currentId, collapsed, onToggle, onSelect, onNew, onNewIn, onAddWorkspace, onOpenPicker, workspaces, activeWorkspace, onRename, onDelete, total, onLoadMore, onSettings, onTheme, theme, route = 'chat', onNavigate }) {
  // 侧栏项目组默认全部收起：首次打开只列出项目本身，便于先选工作区再看会话。
  // 这里只记录用户手动「展开」过的项目（空集 = 全收起），比记录收起集合少一次
  // 依赖 sessions 异步加载的初始化时序。搜索时忽略收起状态，否则命中的会话会被
  // 藏在收起的分组里、搜索看起来像失效。
  const [expandedGroups, setExpandedGroups] = useState(() => new Set())
  const query = filter.trim().toLowerCase()
  const filtered = sessions.filter((session) =>
    !query ||
    (session.title || '').toLowerCase().includes(query) ||
    session.id.toLowerCase().includes(query) ||
    (session.workspace || '').toLowerCase().includes(query))

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
            <input id="session-search" value={filter} onChange={(event) => onFilter(event.target.value)} placeholder="搜索会话 / Ctrl+K" />
          </div>
          <div className="projects-head">
            <span>项目</span>
            <button
              title="选择工作区文件夹"
              onClick={onOpenPicker}
            >＋</button>
          </div>
          <div className="session-list">
            {projects.length === 0 && <div className="empty">没有匹配的会话</div>}
            {projects.map((project) => {
              // 搜索时强制展开：否则命中的会话藏在收起的项目里，搜索看起来像失效。
              const isExpanded = expandedGroups.has(project.path) || Boolean(query)
              const active = project.path === activeWorkspace
              return (
                <div className={`project ${active ? 'active-workspace' : ''}`} key={project.path}>
                  <div className="project-head" title={project.path}>
                    <button className="project-toggle" onClick={() => toggleGroup(project.path)}>
                      {isExpanded ? '▾' : '▸'} 📁 {project.label}
                      <span className="project-count">{project.count}</span>
                    </button>
                    <button
                      className="project-new"
                      title={`在 ${project.label} 新建任务`}
                      onClick={() => onNewIn(project.path)}
                    >＋</button>
                  </div>
                  {isExpanded && project.sessions.map((session) => (
                    <div key={session.id} className={`session-row nested ${session.id === currentId ? 'active' : ''}`}>
                      <button className="session" onClick={() => onSelect(session)} title={session.workspace || ''}>
                        <span className="title">{session.title || session.id}</span>
                        <span className="meta">{relTime(session.modified)}</span>
                      </button>
                      <span className="session-actions">
                        <button title="重命名" onClick={(event) => { event.stopPropagation(); onRename(session) }}>✎</button>
                        <button title="删除" onClick={(event) => { event.stopPropagation(); onDelete(session) }}>×</button>
                      </span>
                    </div>
                  ))}
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
