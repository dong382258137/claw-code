import React, { useEffect, useState } from 'react'
import { TABS } from '../shared/constants.js'
import { FilesPane } from './FilesPane.jsx'
import { DiffPane } from './DiffPane.jsx'
import { TasksPane } from './TasksPane.jsx'
import { AttachmentsPane } from './AttachmentsPane.jsx'
import { TerminalPane } from './TerminalPane.jsx'

export function Panel({ tab, onTab, onClose, onDock, width, currentId, workspace, meta, taskEvent, connected, onError, onToast, onReference }) {
  // 终端只在第一次打开时挂载，之后保持连接；避免每次加载页面都起一个 PTY。
  const [terminalMounted, setTerminalMounted] = useState(tab === 'terminal')
  const [fullscreen, setFullscreen] = useState(false)
  useEffect(() => { if (tab === 'terminal') setTerminalMounted(true) }, [tab])
  useEffect(() => {
    if (!fullscreen) return undefined
    const onKey = (event) => { if (event.key === 'Escape') setFullscreen(false) }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [fullscreen])
  return (
    <section className={`panel ${fullscreen ? 'fullscreen' : ''}`} style={fullscreen ? undefined : { width }}>
      <div className="panel-head">
        <div className="panel-tabs">
          {TABS.map((item) => (
            <button key={item.id} className={tab === item.id ? 'tab active' : 'tab'} onClick={() => onTab(item.id)}>{item.label}</button>
          ))}
        </div>
        <span className="spacer" />
        <div className="panel-actions">
          <button className="panel-dock" onClick={onDock} title="切换停靠侧">⇄</button>
          <button onClick={() => setFullscreen((value) => !value)} title={fullscreen ? '退出全屏 (Esc)' : '全屏'}>{fullscreen ? '⤢' : '⛶'}</button>
          <button onClick={onClose} title="收起">×</button>
        </div>
      </div>
      <div className="panel-body">
        {/* 五个面板常驻，切换只隐藏：终端连接、文件浏览状态不会因切标签丢失。 */}
        <div className={`pane-wrap ${tab === 'files' ? 'active' : ''}`}><FilesPane workspace={workspace} onError={onError} onReference={onReference} /></div>
        <div className={`pane-wrap ${tab === 'diff' ? 'active' : ''}`}><DiffPane workspace={workspace} currentId={currentId} onError={onError} onReference={onReference} /></div>
        <div className={`pane-wrap ${tab === 'tasks' ? 'active' : ''}`}><TasksPane currentId={currentId} workspace={workspace} taskEvent={taskEvent} connected={connected} onError={onError} onToast={onToast} /></div>
        <div className={`pane-wrap ${tab === 'attachments' ? 'active' : ''}`}><AttachmentsPane workspace={workspace} onError={onError} /></div>
        <div className={`pane-wrap ${tab === 'terminal' ? 'active' : ''}`}>{terminalMounted && <TerminalPane workspace={workspace} />}</div>
      </div>
    </section>
  )
}
