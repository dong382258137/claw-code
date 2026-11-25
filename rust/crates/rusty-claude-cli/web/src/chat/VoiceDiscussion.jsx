import React, { useEffect, useRef, useState } from 'react'
import { withToken } from '../api'
import { Icon } from '../shared/icons.jsx'
import { VOICE_BAR_COUNT, VOICE_FLAT_BARS } from '../shared/constants.js'
import { createDictation, speechText, voiceClock } from '../shared/utils.js'

// 语音讨论 = 纯对话，不是 agent 任务。
// 后端 /api/voice/chat 不走 LiveCli、不带工具、不写会话文件，
// 讨论历史只存在本组件内存里。这样「陪你理思路」才不会被工具调用打断，
// 也不会出现工具状态识别。
export function VoiceDiscussion({ model, onClose, onError, onDraft }) {
  const [phase, setPhase] = useState('listening')
  const [log, setLog] = useState([])
  const [interim, setInterim] = useState('')
  const [elapsed, setElapsed] = useState(0)
  const [waveBars, setWaveBars] = useState(VOICE_FLAT_BARS)
  const [ttsOn, setTtsOn] = useState(true)
  const [busy, setBusy] = useState(false)

  const bodyRef = useRef(null)
  const openRef = useRef(true)
  const listeningRef = useRef(false)
  const dictationRef = useRef(null)
  const startedRef = useRef(Date.now())
  const elapsedTimerRef = useRef(0)
  const waveTimerRef = useRef(0)
  const mediaStreamRef = useRef(null)
  const audioCtxRef = useRef(null)
  const analyserRef = useRef(null)
  const ttsOnRef = useRef(true)
  const logRef = useRef(log)
  const abortRef = useRef(null)
  logRef.current = log

  const changePhase = (next) => setPhase(next)

  const stopWaveform = () => {
    if (waveTimerRef.current) { clearInterval(waveTimerRef.current); waveTimerRef.current = 0 }
    if (mediaStreamRef.current) {
      mediaStreamRef.current.getTracks().forEach((track) => track.stop())
      mediaStreamRef.current = null
    }
    if (audioCtxRef.current) {
      try { audioCtxRef.current.close() } catch (_) {}
      audioCtxRef.current = null
    }
    analyserRef.current = null
    setWaveBars(VOICE_FLAT_BARS)
  }

  const startWaveform = async () => {
    let analyser = null
    let data = null
    try {
      if (!navigator.mediaDevices?.getUserMedia) throw new Error('no media devices')
      const stream = await navigator.mediaDevices.getUserMedia({ audio: true })
      if (!listeningRef.current) {
        stream.getTracks().forEach((track) => track.stop())
        return
      }
      mediaStreamRef.current = stream
      const AudioCtx = window.AudioContext || window.webkitAudioContext
      if (!AudioCtx) throw new Error('no audio context')
      const context = new AudioCtx()
      audioCtxRef.current = context
      const source = context.createMediaStreamSource(stream)
      analyser = context.createAnalyser()
      analyser.fftSize = 128
      analyser.smoothingTimeConstant = 0.75
      source.connect(analyser)
      analyserRef.current = analyser
      data = new Uint8Array(analyser.frequencyBinCount)
    } catch (_) {
      analyser = null
      data = null
    }
    const startedAt = Date.now()
    waveTimerRef.current = setInterval(() => {
      if (!listeningRef.current) return
      let bars
      if (analyser && data) {
        analyser.getByteFrequencyData(data)
        bars = Array.from({ length: VOICE_BAR_COUNT }, (_, index) => {
          const bin = Math.min(data.length - 1, Math.floor((index / VOICE_BAR_COUNT) * data.length * 0.72))
          const peak = Math.max(data[bin], data[Math.min(data.length - 1, bin + 2)] || 0)
          return Math.min(1, Math.max(0.06, (peak / 255) * 1.8))
        })
      } else {
        const tick = (Date.now() - startedAt) / 160
        bars = Array.from({ length: VOICE_BAR_COUNT }, (_, index) => {
          const wave = Math.sin(tick + index * 0.42) * 0.28 + Math.sin(tick * 1.9 + index * 0.13) * 0.22 + 0.38
          const spike = Math.random() < 0.06 ? Math.random() * 0.55 : 0
          return Math.min(1, Math.max(0.06, wave + spike))
        })
      }
      setWaveBars(bars)
    }, 70)
  }

  // 只停采集（麦克风 + 波形 + 朗读），不关闭面板、不中止在途请求。
  const stopCapture = () => {
    listeningRef.current = false
    const dictation = dictationRef.current
    dictationRef.current = null
    dictation?.stop()
    stopWaveform()
    try { window.speechSynthesis?.cancel() } catch (_) {}
  }

  const stopAll = () => {
    openRef.current = false
    stopCapture()
    clearInterval(elapsedTimerRef.current)
    const controller = abortRef.current
    abortRef.current = null
    if (controller) controller.abort()
  }

  const speakReply = (text, done) => {
    let finished = false
    const finish = () => { if (finished) return; finished = true; done() }
    if (!ttsOnRef.current || !window.speechSynthesis) { window.setTimeout(finish, 200); return }
    try {
      const utterance = new SpeechSynthesisUtterance(speechText(text))
      utterance.lang = navigator.language || 'zh-CN'
      utterance.rate = 1
      utterance.onend = finish
      utterance.onerror = finish
      window.speechSynthesis.cancel()
      window.speechSynthesis.speak(utterance)
    } catch (_) {
      finish()
    }
  }

  const resumeListening = () => {
    if (!openRef.current || listeningRef.current) return
    changePhase('listening')
    startListening()
  }

  // 纯对话调用：把完整讨论历史发给后端，拿回一句回复。
  // summarize=true 时后端换成总结提示词，返回结构化总结。
  const callVoiceChat = async (history, summarize) => {
    const controller = new AbortController()
    abortRef.current = controller
    try {
      const response = await fetch(withToken('/api/voice/chat'), {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          messages: history.map((item) => ({ role: item.role, text: item.text })),
          model: model || undefined,
          summarize: Boolean(summarize),
        }),
        signal: controller.signal,
      })
      if (!response.ok) {
        const text = await response.text().catch(() => '')
        throw new Error(`HTTP ${response.status}${text ? ' · ' + text.slice(0, 200) : ''}`)
      }
      const data = await response.json()
      return String(data.reply || '').trim()
    } finally {
      if (abortRef.current === controller) abortRef.current = null
    }
  }

  const submitUtterance = () => {
    if (busy) return
    const text = (dictationRef.current?.text() || '').trim()
    if (!text) return
    sendUtterance(text)
  }

  const sendUtterance = (text) => {
    stopCapture()
    setInterim('')
    const next = [...logRef.current, { role: 'user', text }]
    setLog(next)
    changePhase('thinking')
    setBusy(true)
    ;(async () => {
      try {
        const reply = await callVoiceChat(next, false)
        if (!openRef.current) return
        setBusy(false)
        setLog((previous) => [...previous, { role: 'assistant', text: reply }])
        if (ttsOnRef.current && window.speechSynthesis) {
          changePhase('speaking')
          speakReply(reply, resumeListening)
        } else {
          resumeListening()
        }
      } catch (error) {
        if (!openRef.current || error.name === 'AbortError') return
        setBusy(false)
        onError?.(`语音讨论失败：${error.message}`)
        resumeListening()
      }
    })()
  }

  const startListening = () => {
    if (!openRef.current || listeningRef.current) return
    // 识别会话由 createDictation 自动续接：浏览器单次会话约 60 秒会断，
    // 长讨论靠断了就换新实例重开，不再依赖单次 start() 无限延续。
    const dictation = createDictation({
      lang: navigator.language || 'zh-CN',
      onText: (text) => {
        // 刻意不自动提交：停顿时长不能代表「说完了」，由用户点「发送这句」显式提交。
        if (listeningRef.current) setInterim(text)
      },
      onFail: (message) => {
        onError?.(message)
        stopAll()
        onClose()
      },
    })
    if (!dictation) {
      onError?.('当前浏览器不支持语音讨论，请使用 Chrome / Edge')
      stopAll()
      onClose()
      return
    }
    listeningRef.current = true
    dictationRef.current = dictation
    setInterim('')
    startWaveform()
    dictation.start()
  }

  const pauseListening = () => {
    if (!listeningRef.current) return
    stopCapture()
    setInterim('')
    changePhase('paused')
  }

  // 打断：优先中止在途的模型请求；否则只是跳过朗读继续听。
  const interruptReply = () => {
    const controller = abortRef.current
    if (controller) {
      controller.abort()
      abortRef.current = null
      setBusy(false)
      resumeListening()
      return
    }
    try { window.speechSynthesis?.cancel() } catch (_) {}
    resumeListening()
  }

  const toggleTts = () => {
    const next = !ttsOn
    setTtsOn(next)
    ttsOnRef.current = next
    if (!next) { try { window.speechSynthesis?.cancel() } catch (_) {} }
  }

  // 结束讨论：让模型出总结，只填入输入框等待用户修改后自行发送。
  // 绝不直接发送——总结与语音转写都可能失真，用户必须保留最后决定权。
  const finishDiscussion = () => {
    if (busy) return
    // 先把还没定稿的转写收下来，再停采集（stop 之后 dictation 内文本仍可取，但这里更直观）。
    const tail = (dictationRef.current?.text() || '').trim()
    stopCapture()
    const items = [...logRef.current]
    if (tail) items.push({ role: 'user', text: tail })
    setInterim('')
    if (!items.length) { onClose(); return }
    setBusy(true)
    changePhase('thinking')
    ;(async () => {
      try {
        const summary = await callVoiceChat(items, true)
        if (!openRef.current) return
        setBusy(false)
        if (summary) onDraft?.(summary)
        onClose()
      } catch (error) {
        if (!openRef.current || error.name === 'AbortError') return
        setBusy(false)
        onError?.(`总结失败：${error.message}`)
        resumeListening()
      }
    })()
  }

  const closeDiscussion = () => {
    stopAll()
    onClose()
  }

  useEffect(() => {
    openRef.current = true
    startedRef.current = Date.now()
    elapsedTimerRef.current = window.setInterval(() => {
      if (!openRef.current) return
      setElapsed(Math.floor((Date.now() - startedRef.current) / 1000))
    }, 500)
    const timer = window.setTimeout(() => { if (openRef.current) startListening() }, 200)
    return () => {
      clearTimeout(timer)
      stopAll()
    }
  }, [])

  useEffect(() => {
    const node = bodyRef.current
    if (node) node.scrollTop = node.scrollHeight
  }, [log, interim, phase])

  useEffect(() => {
    const onKey = (event) => {
      if (event.key !== 'Escape') return
      event.preventDefault()
      stopAll()
      onClose()
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [])

  const statusText = {
    listening: '正在聆听…',
    paused: '已暂停',
    thinking: '思考中…',
    speaking: '朗读中…',
  }[phase] || ''

  return (
    <div className="vd-mask" role="dialog" aria-modal="true">
      <div className="vd-panel">
        <div className="vd-head">
          <div>
            <div className="vd-title">语音讨论</div>
            <div className="vd-sub">AI 只陪你聊清思路，不调用任何工具；结束后生成总结填进输入框，由你改完再发</div>
          </div>
          <div className="vd-head-actions">
            <button
              type="button"
              className={`icon-btn ${ttsOn ? 'active' : ''}`}
              onClick={toggleTts}
              title={ttsOn ? '关闭语音播报' : '开启语音播报'}
              aria-label="语音播报"
            >
              <Icon name={ttsOn ? 'volume' : 'volumeOff'} size={16} />
            </button>
            <button type="button" className="icon-btn" onClick={closeDiscussion} title="关闭" aria-label="关闭语音讨论">
              <Icon name="x" size={17} />
            </button>
          </div>
        </div>
        <div className="vd-body" ref={bodyRef}>
          {log.length === 0 && !interim && (
            <div className="vd-empty">
              <div className="vd-empty-icon"><Icon name="wave" size={24} /></div>
              想到什么就说什么，说完点「发送这句」。AI 会追问、帮你补全没想到的角度；聊够了点「结束并整理」。
            </div>
          )}
          {log.map((item, index) => (
            <div key={`${item.role}-${index}`} className={`vd-msg ${item.role}`}>
              <span className="vd-who">{item.role === 'user' ? '你' : 'AI'}</span>
              <div className="vd-text">{item.text}</div>
            </div>
          ))}
          {interim && (
            <div className="vd-msg user interim">
              <span className="vd-who">你</span>
              <div className="vd-text">{interim}</div>
            </div>
          )}
        </div>
        <div className="vd-foot">
          {phase === 'listening' ? (
            <button type="button" className="voice-btn cancel" onClick={pauseListening} title="暂停聆听" aria-label="暂停聆听">
              <Icon name="pause" size={18} />
            </button>
          ) : phase === 'paused' ? (
            <button type="button" className="voice-btn cancel" onClick={resumeListening} title="继续聆听" aria-label="继续聆听">
              <Icon name="mic" size={18} />
            </button>
          ) : (
            <button type="button" className="voice-btn cancel" onClick={interruptReply} title="打断" aria-label="打断">
              <Icon name="stop" size={16} />
            </button>
          )}
          <div className="vd-status">
            <span className={`vd-dot ${phase}`} />
            <span>{statusText}</span>
          </div>
          <div className={`voice-wave vd-wave ${phase === 'listening' ? '' : 'idle'}`} aria-hidden="true">
            {waveBars.map((level, index) => (
              <span key={index} style={{ height: `${Math.round(3 + level * 23)}px` }} />
            ))}
          </div>
          <span className="voice-time">{voiceClock(elapsed)}</span>
          <button
            type="button"
            className="vd-send"
            disabled={!interim.trim() || busy || phase === 'thinking' || phase === 'speaking'}
            onClick={submitUtterance}
            title="把这句话发给 AI"
          >
            发送这句
          </button>
          <button
            type="button"
            className="vd-end"
            disabled={busy}
            onClick={finishDiscussion}
          >
            {busy ? '整理中…' : '结束并整理'}
          </button>
        </div>
      </div>
    </div>
  )
}
