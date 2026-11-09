import { useCallback, useState } from 'react'

export function useToasts() {
  const [toasts, setToasts] = useState([])
  const push = useCallback((text, kind = 'info', action = null) => {
    if (!text) return
    const id = `${Date.now()}-${Math.random()}`
    setToasts((list) => {
      const last = list[list.length - 1]
      // 同类错误 2 秒内只提示一次，避免失败重试时刷屏。
      if (last && last.text === text && last.kind === kind && Date.now() - last.at < 2000) return list
      // 错误提示常驻且只保留最新一条：断流/失败的原因需要用户读到，
      // 不能像普通提示那样 5 秒后自己消失。
      const base = kind === 'error' ? list.filter((toast) => toast.kind !== 'error') : list
      return [...base, { id, text, kind, action, at: Date.now() }]
    })
    if (kind !== 'error') {
      setTimeout(() => setToasts((list) => list.filter((toast) => toast.id !== id)), 5000)
    }
  }, [])
  return { toasts, push }
}
