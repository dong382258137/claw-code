import React from 'react'

// 市场骨架页：本轮只占位，不发起任何请求、不定义 API 契约。
// 后续接入时，列表/安装/启停都落在这个组件里，不再回到 App.jsx。
const COPY = {
  plugins: {
    title: '插件市场',
    subtitle: '浏览、安装与管理插件，为工作区补充工具与能力。',
    emptyTitle: '插件市场骨架已就位',
    emptyText: '列表、搜索、安装与启停将在后续版本接入；本轮不定义后端接口。',
  },
  skills: {
    title: '技能市场',
    subtitle: '浏览与安装技能，让助手按需获得领域能力。',
    emptyTitle: '技能市场骨架已就位',
    emptyText: '技能列表、来源校验与安装流程将在后续版本接入；本轮不定义后端接口。',
  },
}

export function MarketPage({ kind = 'plugins' }) {
  const copy = COPY[kind] || COPY.plugins
  return (
    <div className="market-page">
      <div className="market-head">
        <div>
          <h1>{copy.title}</h1>
          <p>{copy.subtitle}</p>
        </div>
        <span className="chip">骨架页</span>
      </div>
      <div className="market-body">
        <div className="market-empty">
          <div className="market-empty-title">{copy.emptyTitle}</div>
          <p>{copy.emptyText}</p>
          <div className="market-slots">
            <span className="market-slot">列表区</span>
            <span className="market-slot">详情区</span>
            <span className="market-slot">安装 / 启停</span>
          </div>
        </div>
      </div>
    </div>
  )
}
