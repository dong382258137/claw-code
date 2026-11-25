import React from 'react'

export function TopBar({ title, chips, connected, running, onSettings, onTogglePanel, panelOpen }) {
  return (
    <div className="topbar">
      <span className="title">{title}</span>
      <div className="chips">
        {chips.map((chip, index) => (
          <span
            key={index}
            className={`chip${/条$/.test(String(chip)) ? ' chip-count' : ''}`}
            title={String(chip)}
          >{chip}</span>
        ))}
        {!connected && <span className="chip warn">离线</span>}
        {running && <span className="chip running">运行中</span>}
      </div>
      <button onClick={onSettings} title="设置">⚙</button>
      <button onClick={onTogglePanel}>{panelOpen ? '隐藏面板' : '显示面板'}</button>
    </div>
  )
}
