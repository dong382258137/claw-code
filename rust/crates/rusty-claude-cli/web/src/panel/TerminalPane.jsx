import React, { useCallback, useEffect, useRef, useState } from 'react'
import { Terminal } from '@xterm/xterm'
import { FitAddon } from '@xterm/addon-fit'
import '@xterm/xterm/css/xterm.css'
import { wsUrl } from '../api'

export function TerminalPane() {
  const [status, setStatus] = useState('未连接')
  const [fullscreen, setFullscreen] = useState(false)
  const hostRef = useRef(null)
  const termRef = useRef(null)
  const fitRef = useRef(null)
  const socketRef = useRef(null)

  const sendJson = useCallback((payload) => {
    const socket = socketRef.current
    if (socket?.readyState === 1) socket.send(JSON.stringify(payload))
  }, [])

  // 尺寸以 xterm 拟合出的 cols/rows 为准（字符网格），不再按像素估算。
  const pushResize = useCallback(() => {
    const fit = fitRef.current
    const term = termRef.current
    if (!fit || !term) return
    try {
      fit.fit()
    } catch (_) {
      return
    }
    sendJson({ type: 'resize', cols: term.cols, rows: term.rows })
  }, [sendJson])

  // 终端只初始化一次：xterm 实例随渲染重建会丢掉滚屏缓冲区与光标状态。
  useEffect(() => {
    const host = hostRef.current
    if (!host) return
    const term = new Terminal({
      cursorBlink: true,
      fontFamily: 'Consolas, "Cascadia Mono", monospace',
      fontSize: 13,
      scrollback: 5000,
    })
    const fit = new FitAddon()
    term.loadAddon(fit)
    term.open(host)
    termRef.current = term
    fitRef.current = fit
    try {
      fit.fit()
    } catch (_) {}

    const socket = new WebSocket(wsUrl('/ws/terminal'))
    socket.binaryType = 'arraybuffer'
    socket.onopen = () => {
      setStatus('已连接')
      setTimeout(pushResize, 120)
    }
    // 原始字节直通 xterm：由它解析 ANSI/颜色/光标/清屏，不再丢掉控制序列，
    // 这样 vim/htop/top 这类全屏程序才能正常工作。
    socket.onmessage = (event) => {
      if (typeof event.data === 'string') term.write(event.data)
      else term.write(new Uint8Array(event.data))
    }
    socket.onclose = () => setStatus('已断开')
    socket.onerror = () => setStatus('连接失败')
    socketRef.current = socket

    // 键盘输入直通 PTY：方向键、Tab 补全、Ctrl+C 都走 xterm 的按键编码。
    const disposable = term.onData((data) => sendJson({ type: 'input', data }))

    const observer = new ResizeObserver(() => pushResize())
    observer.observe(host)
    window.addEventListener('resize', pushResize)
    return () => {
      disposable.dispose()
      observer.disconnect()
      window.removeEventListener('resize', pushResize)
      socket.close()
      term.dispose()
      termRef.current = null
      fitRef.current = null
    }
  }, [pushResize])

  useEffect(() => {
    if (fullscreen) setTimeout(pushResize, 80)
  }, [fullscreen, pushResize])

  useEffect(() => {
    const onKey = (event) => { if (event.key === 'Escape') setFullscreen(false) }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [])

  const send = (data) => sendJson({ type: 'input', data })
  return (
    <div className={`pane ${fullscreen ? 'term-fullscreen' : ''}`}>
      <div className="pane-toolbar">
        <span className="hint">{status}</span>
        <span className="spacer" />
        <button onClick={() => termRef.current?.clear()}>清屏</button>
        <button onClick={() => send('\u0003')}>Ctrl+C</button>
        <button onClick={() => setFullscreen((value) => !value)}>{fullscreen ? '退出全屏' : '全屏'}</button>
      </div>
      <div className="term-host" ref={hostRef} />
    </div>
  )
}
