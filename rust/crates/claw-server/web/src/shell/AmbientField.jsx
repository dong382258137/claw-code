import { useEffect, useRef } from 'react'

// 环境粒子流：种子化随机 + 低幅流场，铺在内容层之下做深空氛围。
// 约束：DPR ≤ 1.5、约 20fps、页面隐藏时暂停；prefers-reduced-motion 时只画静态一帧。
export function AmbientField() {
  const canvasRef = useRef(null)

  useEffect(() => {
    const canvas = canvasRef.current
    const host = canvas?.parentElement
    const ctx = canvas?.getContext('2d')
    if (!canvas || !host || !ctx) return undefined

    const reduceMotion = window.matchMedia('(prefers-reduced-motion: reduce)').matches
    let width = 1
    let height = 1
    let points = []
    let raf = 0
    let lastFrame = 0
    let pageVisible = !document.hidden
    let seed = 0x2f6e2b1

    const random = () => {
      seed = (seed * 1664525 + 1013904223) >>> 0
      return seed / 4294967296
    }

    const resize = () => {
      const rect = host.getBoundingClientRect()
      width = Math.max(1, rect.width)
      height = Math.max(1, rect.height)
      const dpr = Math.min(window.devicePixelRatio || 1, 1.5)
      canvas.width = Math.round(width * dpr)
      canvas.height = Math.round(height * dpr)
      canvas.style.width = `${width}px`
      canvas.style.height = `${height}px`
      ctx.setTransform(dpr, 0, 0, dpr, 0, 0)
      const count = Math.min(56, Math.max(20, Math.round((width * height) / 38000)))
      seed = 0x2f6e2b1
      points = Array.from({ length: count }, () => ({
        x: random() * width,
        y: random() * height,
        speed: 0.16 + random() * 0.22,
        radius: 0.6 + random() * 1.1,
        phase: random() * Math.PI * 2,
      }))
    }

    const render = (time, animate) => {
      ctx.clearRect(0, 0, width, height)
      const linkDistance = 120
      for (let i = 0; i < points.length; i += 1) {
        const point = points[i]
        if (animate) {
          const angle =
            Math.sin((point.x + point.phase * 40) * 0.004 + time * 0.00012) * Math.PI +
            Math.cos((point.y - point.phase * 25) * 0.005 - time * 0.00009) * Math.PI
          point.x += Math.cos(angle) * point.speed
          point.y += Math.sin(angle) * point.speed
          if (point.x < -24) point.x = width + 24
          if (point.x > width + 24) point.x = -24
          if (point.y < -24) point.y = height + 24
          if (point.y > height + 24) point.y = -24
        }
        for (let j = i + 1; j < points.length; j += 1) {
          const other = points[j]
          const dx = point.x - other.x
          const dy = point.y - other.y
          const distance = Math.hypot(dx, dy)
          if (distance > linkDistance) continue
          const alpha = (0.14 * (1 - distance / linkDistance)).toFixed(3)
          ctx.strokeStyle = `rgba(53, 230, 255, ${alpha})`
          ctx.lineWidth = 0.6
          ctx.beginPath()
          ctx.moveTo(point.x, point.y)
          ctx.lineTo(other.x, other.y)
          ctx.stroke()
        }
      }
      for (const point of points) {
        ctx.fillStyle = 'rgba(53, 230, 255, 0.05)'
        ctx.beginPath()
        ctx.arc(point.x, point.y, point.radius * 3.2, 0, Math.PI * 2)
        ctx.fill()
        ctx.fillStyle = 'rgba(150, 246, 255, 0.55)'
        ctx.beginPath()
        ctx.arc(point.x, point.y, point.radius, 0, Math.PI * 2)
        ctx.fill()
      }
    }

    const tick = (time) => {
      raf = window.requestAnimationFrame(tick)
      if (!pageVisible || time - lastFrame < 50) return
      lastFrame = time
      render(time, true)
    }

    resize()
    if (reduceMotion) {
      render(0, false)
    } else {
      raf = window.requestAnimationFrame(tick)
    }

    const observer = new ResizeObserver(() => {
      resize()
      if (reduceMotion) render(0, false)
    })
    observer.observe(host)
    const onVisibility = () => {
      pageVisible = !document.hidden
    }
    document.addEventListener('visibilitychange', onVisibility)
    return () => {
      window.cancelAnimationFrame(raf)
      observer.disconnect()
      document.removeEventListener('visibilitychange', onVisibility)
    }
  }, [])

  return <canvas ref={canvasRef} className="ambient-field" aria-hidden="true" />
}
