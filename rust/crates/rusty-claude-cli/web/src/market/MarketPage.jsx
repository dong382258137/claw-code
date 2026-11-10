import React from 'react'
import { SkillMarketPage } from './SkillMarketPage.jsx'

// 市场页入口：插件仍为占位；技能已接入只读目录（P0）。
const COPY = {
  plugins: {
    title: '插件市场',
    subtitle: '浏览、安装与管理插件，为工作区补充工具与能力。',
    emptyTitle: '插件市场骨架已就位',
    emptyText: '列表、搜索、安装与启停将在后续版本接入；本轮不定义后端接口。',
  },
}

export function MarketPage({ kind = 'plugins' }) {
  if (kind === 'skills') return <SkillMarketPage />
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
