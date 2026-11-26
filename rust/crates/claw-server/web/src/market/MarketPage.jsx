import React from 'react'
import { PluginMarketPage } from './PluginMarketPage.jsx'
import { SkillMarketPage } from './SkillMarketPage.jsx'

// 市场页入口：插件与技能各自有真实页面。
export function MarketPage({ kind = 'plugins' }) {
  if (kind === 'skills') return <SkillMarketPage />
  return <PluginMarketPage />
}
