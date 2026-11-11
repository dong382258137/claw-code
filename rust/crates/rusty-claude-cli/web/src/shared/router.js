import { useEffect, useState } from 'react'

// 极简 hash 路由：不引入路由库，也不要求后端为 SPA 深链接做 fallback。
// #/plugins、#/skills 是市场骨架页；其余一律回到对话页。
const ROUTES = ['chat', 'plugins', 'skills']
const ROUTE_EVENT = 'claw-route-change'

export function parseRoute(hash = '') {
  const value = String(hash || '').replace(/^#\/?/, '').split('?')[0]
  // 技能/插件详情与来源管理都走子路径；一级路由仍返回 skills / plugins，
  // App.jsx / Sidebar 无需感知子路径。
  if (value === 'skills' || value.startsWith('skills/')) return 'skills'
  if (value === 'plugins' || value.startsWith('plugins/')) return 'plugins'
  return ROUTES.includes(value) ? value : 'chat'
}

// 解析技能市场子路径：{ view: 'list' | 'installed' | 'sources' | 'detail', name? }
export function parseSkillSub(hash = '') {
  const value = String(hash || '').replace(/^#\/?/, '').split('?')[0]
  if (!value.startsWith('skills/')) return { view: 'list' }
  const rest = value.slice('skills/'.length)
  if (!rest) return { view: 'list' }
  if (rest === 'installed') return { view: 'installed' }
  if (rest === 'sources') return { view: 'sources' }
  return { view: 'detail', name: decodeURIComponent(rest) }
}

// 解析插件市场子路径：{ view: 'list' | 'installed' | 'sources' | 'detail', name? }
export function parsePluginSub(hash = '') {
  const value = String(hash || '').replace(/^#\/?/, '').split('?')[0]
  if (!value.startsWith('plugins/')) return { view: 'list' }
  const rest = value.slice('plugins/'.length)
  if (!rest) return { view: 'list' }
  if (rest === 'installed') return { view: 'installed' }
  if (rest === 'sources') return { view: 'sources' }
  return { view: 'detail', name: decodeURIComponent(rest) }
}

export function navigate(route) {
  const next = ROUTES.includes(route) ? route : 'chat'
  if (next === 'chat') {
    // replaceState 不会触发 hashchange，必须再派发一次自定义事件同步路由状态。
    if (location.hash) history.replaceState(null, '', location.pathname + location.search)
  } else {
    location.hash = `#/${next}`
  }
  window.dispatchEvent(new Event(ROUTE_EVENT))
}

export function useRoute() {
  const [route, setRoute] = useState(() => parseRoute(location.hash))
  useEffect(() => {
    const onChange = () => setRoute(parseRoute(location.hash))
    window.addEventListener('hashchange', onChange)
    window.addEventListener(ROUTE_EVENT, onChange)
    return () => {
      window.removeEventListener('hashchange', onChange)
      window.removeEventListener(ROUTE_EVENT, onChange)
    }
  }, [])
  return route
}
