import { MODEL_LOOKUP } from './constants.js'

export function clampWidth(desired, dock, collapsed) {
  const sidebar = collapsed ? 56 : 280
  const available = window.innerWidth - sidebar - 400 - 4
  const viewportCap = Math.floor(window.innerWidth * 0.7)
  const upper = Math.max(300, Math.min(viewportCap, available))
  return Math.max(300, Math.min(Math.round(desired || window.innerWidth * 0.45), upper))
}

export function basename(path) {
  return String(path).split(/[\\/]/).filter(Boolean).pop() || ''
}

export function relTime(seconds) {
  if (!seconds) return ''
  const diff = Math.max(0, Date.now() / 1000 - seconds)
  if (diff < 60) return '刚刚'
  if (diff < 3600) return `${Math.floor(diff / 60)} 分钟前`
  if (diff < 86400) return `${Math.floor(diff / 3600)} 小时前`
  return `${Math.floor(diff / 86400)} 天前`
}

export function fmtClock(ms) {
  if (!ms) return ''
  const date = new Date(ms)
  const pad = (value) => String(value).padStart(2, '0')
  return `${pad(date.getHours())}:${pad(date.getMinutes())}`
}

export function fmtTime(ms) {
  if (!ms) return ''
  const date = new Date(ms)
  const pad = (value) => String(value).padStart(2, '0')
  return `${date.getMonth() + 1}-${pad(date.getDate())} ${pad(date.getHours())}:${pad(date.getMinutes())}`
}

export function formatTokens(value) {
  const count = Number(value) || 0
  if (count >= 1e9) return `${(count / 1e9).toFixed(1)}B`
  if (count >= 1e6) return `${(count / 1e6).toFixed(1)}M`
  if (count >= 1e3) return `${(count / 1e3).toFixed(1)}k`
  return String(count)
}

export function speechText(text) {
  return String(text || '')
    .replace(/```[\s\S]*?```/g, ' 代码块 ')
    .replace(/`([^`]+)`/g, '$1')
    .replace(/https?:\/\/\S+/g, ' 链接 ')
    .replace(/[*_>#~-]+/g, ' ')
    .replace(/\s+/g, ' ')
    .trim()
    .slice(0, 1200)
}

export function voiceClock(seconds) {
  const value = Math.max(0, Number(seconds) || 0)
  return `${Math.floor(value / 60)}:${String(value % 60).padStart(2, '0')}`
}

// 浏览器（Chrome/Edge）的 Web Speech 识别单次会话有硬上限：连续识别约 60 秒、
// 静音数秒都会自动结束，长语音输入必须靠“结束后立即重开”续接。
// 这里统一封装续接循环，避免各组件各写一套时踩到同类坑：
// - 每轮换新实例：复用旧实例再 start() 常抛 InvalidStateError，异常被吞后
//   就再也不会有 onend，表现为界面还在“聆听”但识别已经死掉；
// - 瞬时断连（network / audio-capture）按退避重试，连续失败才上报；
// - 看门狗兜底识别服务卡死（长时间会话偶发，既不回调也不结束）。
export function createDictation({ lang, onText, onFail } = {}) {
  const Recognition = window.SpeechRecognition || window.webkitSpeechRecognition
  if (!Recognition) return null

  const baseLang = lang || navigator.language || 'zh-CN'
  // 连续快速失败（启动即断且无任何结果）的上限，超过就提示用户而不是无限热循环。
  const MAX_ATTEMPTS = 4
  // 完全没有回调事件的静默卡死判定时长。
  const STALL_MS = 20000
  let active = false
  let recognition = null
  let restartTimer = 0
  let watchdogTimer = 0
  let attempts = 0
  let lastError = ''
  let lastEventAt = 0
  let committed = ''
  let sessionFinal = ''
  let sessionInterim = ''
  let cycleStartedAt = 0
  let cycleGotResult = false

  const text = () => (committed + sessionFinal + sessionInterim).trim()

  const release = () => {
    const current = recognition
    recognition = null
    if (!current) return
    current.onresult = null
    current.onerror = null
    current.onend = null
    try { current.stop() } catch (_) {}
  }

  const stop = () => {
    active = false
    if (restartTimer) { clearTimeout(restartTimer); restartTimer = 0 }
    if (watchdogTimer) { clearInterval(watchdogTimer); watchdogTimer = 0 }
    release()
  }

  const fail = (message) => {
    stop()
    onFail?.(message)
  }

  const scheduleRestart = () => {
    if (restartTimer) return
    const delay = Math.min(2000, 300 * 2 ** Math.min(attempts, 3))
    restartTimer = window.setTimeout(() => { restartTimer = 0; startCycle() }, delay)
  }

  const startCycle = () => {
    if (!active) return
    const instance = new Recognition()
    recognition = instance
    cycleStartedAt = Date.now()
    lastEventAt = cycleStartedAt
    cycleGotResult = false
    instance.lang = baseLang
    instance.continuous = true
    instance.interimResults = true
    instance.onstart = () => { lastEventAt = Date.now() }
    instance.onresult = (event) => {
      if (!active || recognition !== instance) return
      lastEventAt = Date.now()
      cycleGotResult = true
      attempts = 0
      lastError = ''
      let finalText = ''
      let interimText = ''
      for (let index = 0; index < event.results.length; index += 1) {
        const result = event.results[index]
        const transcript = result?.[0]?.transcript || ''
        if (result.isFinal) finalText += transcript
        else interimText += transcript
      }
      sessionFinal = finalText
      sessionInterim = interimText
      onText?.(text())
    }
    instance.onerror = (event) => {
      if (!active || recognition !== instance) return
      lastEventAt = Date.now()
      const code = event.error
      if (code === 'aborted' || code === 'no-speech') return
      if (code === 'not-allowed' || code === 'service-not-allowed') {
        fail('麦克风权限被拒绝，无法使用语音')
        return
      }
      // 长会话里 network / audio-capture 多为瞬时故障，先交给 onend 重开重试。
      lastError = {
        network: '语音识别服务不可用（需要联网）',
        'audio-capture': '没有检测到可用的麦克风',
      }[code] || `语音识别出错：${code}`
    }
    instance.onend = () => {
      if (!active || recognition !== instance) return
      lastEventAt = Date.now()
      recognition = null
      // 浏览器可能在断会话前不给最终结果，把未定稿文本一并收下，避免丢话。
      committed += sessionFinal + sessionInterim
      sessionFinal = ''
      sessionInterim = ''
      if (cycleGotResult) {
        attempts = 0
      } else if (Date.now() - cycleStartedAt < 1000) {
        attempts += 1
      }
      if (attempts >= MAX_ATTEMPTS) {
        fail(lastError || '语音识别连续中断，已停止；请检查网络与麦克风后重试')
        return
      }
      scheduleRestart()
    }
    try {
      instance.start()
    } catch (_) {
      recognition = null
      attempts += 1
      if (attempts >= MAX_ATTEMPTS) {
        fail(lastError || '语音识别启动失败，请检查麦克风与网络后重试')
        return
      }
      scheduleRestart()
    }
  }

  return {
    start() {
      if (active) return
      active = true
      attempts = 0
      lastError = ''
      committed = ''
      sessionFinal = ''
      sessionInterim = ''
      watchdogTimer = window.setInterval(() => {
        if (!active || !recognition) return
        if (Date.now() - lastEventAt < STALL_MS) return
        // 识别服务静默卡死：收下已有文本，换新实例重开。
        committed += sessionFinal + sessionInterim
        sessionFinal = ''
        sessionInterim = ''
        release()
        attempts += 1
        if (attempts >= MAX_ATTEMPTS) {
          fail('语音识别服务无响应，已停止；请检查网络与麦克风后重试')
          return
        }
        scheduleRestart()
      }, 5000)
      startCycle()
    },
    stop,
    text,
  }
}

export function resolveModelLabel(model, sessionModel) {
  const id = String(model || sessionModel || '').trim()
  if (!id) return { label: '默认模型', badge: '' }
  const preset = MODEL_LOOKUP[id]
  if (preset) return { label: preset.label, badge: preset.badge || '' }
  return { label: id, badge: '' }
}
