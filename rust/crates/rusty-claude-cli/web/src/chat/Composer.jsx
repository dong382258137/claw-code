import React, { useEffect, useMemo, useRef, useState } from 'react'
import { api, withToken, withWorkspace } from '../api'
import { Icon, Popover, PopoverItem } from '../shared/icons.jsx'
import {
  CAP_ICONS,
  MODEL_PRESETS,
  PERMISSION_LOOKUP,
  PERMISSION_PRESETS,
  QUICK_ACTIONS,
  VOICE_BAR_COUNT,
  VOICE_FLAT_BARS,
} from '../shared/constants.js'
import { basename, createDictation, formatTokens, resolveModelLabel, voiceClock } from '../shared/utils.js'

export function Composer({ running, queue, onSend, onStop, onQuickAction, onError, currentId, permission, onPermission, model, onModel, workspace, workspaces = [], sessionModel = '', onVoiceDiscussion, onNewInWorkspace, onPickWorkspace, onOpenWorkspacePicker, stats, pendingRef, onRefUsed, pendingDraft, onDraftUsed }) {
  const [value, setValue] = useState('')
  const [files, setFiles] = useState([])
  const [suggest, setSuggest] = useState(null)
  const [cursor, setCursor] = useState(0)
  const [attachments, setAttachments] = useState([])
  const [uploading, setUploading] = useState(false)
  const [openMenu, setOpenMenu] = useState(null)
  const [footerMenu, setFooterMenu] = useState(null)
  const [modelDraft, setModelDraft] = useState(model || '')
  const [dragging, setDragging] = useState(false)
  const [voiceActive, setVoiceActive] = useState(false)
  const [voiceElapsed, setVoiceElapsed] = useState(0)
  const [waveBars, setWaveBars] = useState(VOICE_FLAT_BARS)
  const ref = useRef(null)
  const areaRef = useRef(null)
  const uploadRef = useRef(null)
  const rootRef = useRef(null)
  const filesLoading = useRef(false)
  const dictationRef = useRef(null)
  const voiceActiveRef = useRef(false)
  const voiceBaseRef = useRef('')
  const voiceStartedRef = useRef(0)
  const waveTimerRef = useRef(0)
  const elapsedTimerRef = useRef(0)
  const mediaStreamRef = useRef(null)
  const audioCtxRef = useRef(null)
  const analyserRef = useRef(null)

  useEffect(() => { ref.current = value }, [value])

  const focusEnd = () => {
    const area = areaRef.current
    if (!area) return
    area.focus()
    area.selectionStart = area.selectionEnd = area.value.length
  }

  const insert = (text) => {
    setValue((previous) => {
      if (!previous.trim()) return text
      return /[\s]$/.test(previous) ? previous + text : previous + ' ' + text
    })
    requestAnimationFrame(focusEnd)
  }

  // 每个会话一份草稿：切换会话时不丢正在输入的内容。
  const draftKey = currentId ? `claw-draft:${currentId}` : ''
  useEffect(() => {
    if (!draftKey) { setValue(''); return }
    try { setValue(sessionStorage.getItem(draftKey) || '') } catch (_) { setValue('') }
  }, [draftKey])
  useEffect(() => {
    if (!draftKey) return
    try {
      if (value) sessionStorage.setItem(draftKey, value)
      else sessionStorage.removeItem(draftKey)
    } catch (_) {}
  }, [draftKey, value])

  // 文件面板点「引用选区」后，把 `@path:12-40` 追加到输入框并把光标放到末尾。
  useEffect(() => {
    if (!pendingRef) return
    setValue((previous) => (previous.trim() ? previous.replace(/\s*$/, '') + ' ' : '') + pendingRef)
    onRefUsed?.(null)
    requestAnimationFrame(focusEnd)
  }, [pendingRef, onRefUsed])

  // 语音讨论「结束并整理」后，把整理出的 prompt 填入输入框**等待编辑**，不直接发送。
  // 语音转写难免有错字/断句错误，直接发出去用户没有任何纠正机会。
  // 已有草稿时追加而非覆盖，避免吞掉用户已输入的内容。
  useEffect(() => {
    if (!pendingDraft) return
    setValue((previous) => (previous.trim() ? `${previous.replace(/\s*$/, '')}\n\n${pendingDraft}` : pendingDraft))
    onDraftUsed?.(null)
    requestAnimationFrame(focusEnd)
  }, [pendingDraft, onDraftUsed])

  useEffect(() => { setModelDraft(model || '') }, [model])

  // 高度跟随内容：历史引用、上传、@ 选择都会改写 value，不能只依赖 onChange。
  useEffect(() => {
    const node = areaRef.current
    if (!node) return
    node.style.height = 'auto'
    node.style.height = `${Math.min(220, Math.max(72, node.scrollHeight))}px`
  }, [value])

  useEffect(() => {
    if (suggest?.kind !== 'file' || files.length || filesLoading.current) return
    filesLoading.current = true
    api('/api/workspace/files')
      .then((body) => setFiles(body.files || []))
      .catch(() => {})
      .finally(() => { filesLoading.current = false })
  }, [suggest, files.length])

  // 点击外部或按 Esc 收起下拉浮层。
  useEffect(() => {
    if (!openMenu && !footerMenu) return undefined
    const onDown = (event) => {
      if (!rootRef.current?.contains(event.target)) { setOpenMenu(null); setFooterMenu(null) }
    }
    const onKey = (event) => { if (event.key === 'Escape') { setOpenMenu(null); setFooterMenu(null) } }
    document.addEventListener('mousedown', onDown)
    document.addEventListener('keydown', onKey)
    return () => {
      document.removeEventListener('mousedown', onDown)
      document.removeEventListener('keydown', onKey)
    }
  }, [openMenu, footerMenu])

  useEffect(() => () => {
    voiceActiveRef.current = false
    if (elapsedTimerRef.current) clearInterval(elapsedTimerRef.current)
    if (waveTimerRef.current) clearInterval(waveTimerRef.current)
    dictationRef.current?.stop()
    dictationRef.current = null
    if (mediaStreamRef.current) {
      mediaStreamRef.current.getTracks().forEach((track) => track.stop())
      mediaStreamRef.current = null
    }
    if (audioCtxRef.current) {
      try { audioCtxRef.current.close() } catch (_) {}
      audioCtxRef.current = null
    }
  }, [])

  const slashMatches = useMemo(() => {
    if (!value.startsWith('/') || value.includes(' ')) return []
    return QUICK_ACTIONS.filter((action) => action.cmd.startsWith(value))
  }, [value])

  const fileFragment = useMemo(() => {
    const match = /(^|\s)@([^\s]*)$/.exec(value)
    return match ? match[2] : null
  }, [value])

  const fileMatches = useMemo(() => {
    if (fileFragment === null) return null
    const query = fileFragment.toLowerCase()
    return files.filter((file) => file.path.toLowerCase().includes(query)).slice(0, 8)
  }, [fileFragment, files])

  useEffect(() => {
    if (slashMatches.length) setSuggest({ kind: 'slash', items: slashMatches })
    else if (fileMatches !== null) setSuggest({ kind: 'file', items: fileMatches })
    else setSuggest(null)
    setCursor(0)
  }, [slashMatches, fileMatches])

  const choose = (item) => {
    if (suggest?.kind === 'slash') {
      onQuickAction(item.run)
      setValue('')
    } else if (item.upload) {
      uploadRef.current?.click()
    } else if (suggest?.kind === 'file') {
      setValue((previous) => previous.replace(/@([^\s]*)$/, '@' + item.path + ' '))
    }
    setSuggest(null)
    requestAnimationFrame(focusEnd)
  }

  // 运行中也可以继续发送：后端会入队（最多 5 条），这里只负责把位次/满员说清楚。
  const queueLimit = 5
  const queued = queue?.queued || 0
  const queueFull = queued >= queueLimit

  const submit = () => {
    if (voiceActiveRef.current) return
    const text = value
    if (!text.trim()) return
    if (queueFull) {
      onError?.(`队列已满（${queued}/${queueLimit}），请等前面的消息处理完成后再发送`)
      return
    }
    setSuggest(null)
    setOpenMenu(null)
    setFooterMenu(null)
    setValue('')
    // 没有选中会话时不再拦下用户：交给上层自动在当前工作区新建会话再发送。
    onSend(text)
  }

  const submitCustomModel = () => {
    const next = modelDraft.trim()
    if (next !== (model || '')) onModel(next)
    setOpenMenu(null)
  }

  const onKeyDown = (event) => {
    if (voiceActiveRef.current) {
      if (event.key === 'Escape') { event.preventDefault(); finishVoice(true) }
      return
    }
    if (suggest) {
      const count = suggest.items.length + (suggest.kind === 'file' ? 1 : 0)
      if (event.key === 'ArrowDown') { event.preventDefault(); setCursor((index) => (index + 1) % count); return }
      if (event.key === 'ArrowUp') { event.preventDefault(); setCursor((index) => (index - 1 + count) % count); return }
      if (event.key === 'Tab' || (event.key === 'Enter' && !event.nativeEvent.isComposing)) {
        event.preventDefault()
        if (suggest.kind === 'file') {
          if (cursor === suggest.items.length) choose({ upload: true })
          else choose(suggest.items[cursor])
        } else {
          choose(suggest.items[cursor])
        }
        return
      }
      if (event.key === 'Escape') { event.preventDefault(); setSuggest(null); return }
    }
    if (event.key === 'Escape' && (openMenu || footerMenu)) { setOpenMenu(null); setFooterMenu(null); return }
    // 中文输入法回车确认候选词时不能提交。
    if (event.key === 'Enter' && !event.shiftKey && !event.ctrlKey && !event.nativeEvent.isComposing) {
      event.preventDefault()
      submit()
    }
  }

  const uploadFile = async (list) => {
    const items = Array.from(list || [])
    if (!items.length) return
    setUploading(true)
    try {
      for (const file of items) {
        const response = await fetch(withToken(withWorkspace('/api/upload?name=' + encodeURIComponent(file.name), workspace)), { method: 'POST', body: file })
        if (!response.ok) throw new Error(`HTTP ${response.status}`)
        const body = await response.json()
        setAttachments((previous) => [...previous, { name: file.name, path: body.path, size: file.size }])
        setValue((previous) => (previous.trim() ? previous.replace(/\s*$/, '') + '\n' : '') + '@' + body.path + ' ')
      }
      requestAnimationFrame(focusEnd)
    } catch (error) {
      onError?.(`上传失败：${error.message}`)
    } finally {
      setUploading(false)
      if (uploadRef.current) uploadRef.current.value = ''
    }
  }

  const removeAttachment = (path) => {
    setAttachments((items) => items.filter((item) => item.path !== path))
    setValue((previous) => previous.split('\n').filter((line) => line.trim() !== '@' + path).join('\n'))
  }

  const onPaste = (event) => {
    const pasted = Array.from(event.clipboardData?.files || [])
    if (!pasted.length) return
    event.preventDefault()
    uploadFile(pasted)
  }

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

  // 声波优先取真实麦克风音量；拿不到权限时退化为合成波形，不阻断转写。
  const startWaveform = async () => {
    let analyser = null
    let data = null
    try {
      if (!navigator.mediaDevices?.getUserMedia) throw new Error('no media devices')
      const stream = await navigator.mediaDevices.getUserMedia({ audio: true })
      if (!voiceActiveRef.current) {
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
      if (!voiceActiveRef.current) return
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

  const startVoiceTimer = () => {
    voiceStartedRef.current = Date.now()
    setVoiceElapsed(0)
    if (elapsedTimerRef.current) clearInterval(elapsedTimerRef.current)
    elapsedTimerRef.current = setInterval(() => {
      if (!voiceActiveRef.current) return
      setVoiceElapsed(Math.floor((Date.now() - voiceStartedRef.current) / 1000))
    }, 250)
  }

  const stopVoiceTimer = () => {
    if (elapsedTimerRef.current) { clearInterval(elapsedTimerRef.current); elapsedTimerRef.current = 0 }
  }

  const finishVoice = (restore) => {
    if (!voiceActiveRef.current) return
    voiceActiveRef.current = false
    setVoiceActive(false)
    const dictation = dictationRef.current
    dictationRef.current = null
    const spoken = (dictation?.text() || '').trim()
    dictation?.stop()
    stopVoiceTimer()
    stopWaveform()
    const base = voiceBaseRef.current
    if (restore) {
      setValue(base)
    } else {
      setValue(base && spoken ? base.replace(/\s*$/, '') + ' ' + spoken : base || spoken)
      requestAnimationFrame(focusEnd)
    }
  }

  // 实时语音输入：转写显示在输入框，底栏切换为取消 / 声波 / 计时 / 确认。
  // 识别会话由 createDictation 自动续接，长时间说话不会因浏览器单次会话上限而中断。
  const startVoice = () => {
    if (voiceActiveRef.current) return
    const dictation = createDictation({
      lang: navigator.language || 'zh-CN',
      onText: (spoken) => {
        if (!voiceActiveRef.current) return
        const base = voiceBaseRef.current
        setValue(base && spoken ? base.replace(/\s*$/, '') + ' ' + spoken : base || spoken)
      },
      onFail: (message) => {
        // 重试耗尽才走到这里：保留已识别内容再结束，避免长段语音白说。
        onError?.(message)
        finishVoice(false)
      },
    })
    if (!dictation) { onError?.('当前浏览器不支持实时语音输入，请使用 Chrome / Edge'); return }
    voiceActiveRef.current = true
    voiceBaseRef.current = value
    dictationRef.current = dictation
    setSuggest(null)
    setOpenMenu(null)
    setFooterMenu(null)
    setVoiceActive(true)
    startVoiceTimer()
    startWaveform()
    dictation.start()
  }

  const perm = PERMISSION_LOOKUP[permission] || PERMISSION_PRESETS[1]
  const shownModel = resolveModelLabel(model, sessionModel)
  const canSend = Boolean(value.trim())

  return (
    <form className="composer" ref={rootRef} onSubmit={(event) => { event.preventDefault(); submit() }}>
      {suggest && (
        <div className="menu" role="listbox">
          <div className="menu-head">
            <span>{suggest.kind === 'slash' ? '命令与技能' : '添加上下文'}</span>
            <span className="menu-hint">↑↓ 选择 · Tab 补全 · Esc 关闭</span>
          </div>
          {suggest.items.map((item, position) => {
            const index = position
            return suggest.kind === 'slash' ? (
              <button type="button" key={item.cmd} className={`menu-item ${index === cursor ? 'active' : ''}`} onMouseDown={(event) => { event.preventDefault(); choose(item) }}>
                <b>{item.cmd}</b><span>{item.label}</span>
              </button>
            ) : (
              <button type="button" key={item.path} className={`menu-item ${index === cursor ? 'active' : ''}`} onMouseDown={(event) => { event.preventDefault(); choose(item) }}>
                <b>{item.path}</b><span>{Math.max(1, Math.round((item.size || 0) / 1024))}K</span>
              </button>
            )
          })}
          {suggest.kind === 'file' && suggest.items.length === 0 && (
            <div className="menu-empty">没有匹配的文件</div>
          )}
          {suggest.kind === 'file' && (
            <button
              type="button"
              className={`menu-item ${cursor === suggest.items.length ? 'active' : ''}`}
              onMouseDown={(event) => { event.preventDefault(); choose({ upload: true }) }}
            >
              <b>上传文件…</b><span>图片 / 文档</span>
            </button>
          )}
        </div>
      )}
      {attachments.length > 0 && (
        <div className="composer-attachments">
          {attachments.map((item) => (
            <span key={item.path} className="attachment-chip" title={item.path}>
              <Icon name="paperclip" size={12} />
              <span className="attachment-name">{item.name}</span>
              <button type="button" className="attachment-remove" onClick={() => removeAttachment(item.path)} aria-label={`移除 ${item.name}`}>×</button>
            </span>
          ))}
        </div>
      )}
      {!voiceActive && (queueFull || queued > 0 || (queue?.mine || 0) > 1) && (
        <div className={`queue-hint ${queueFull ? 'full' : ''}`}>
          {queueFull
            ? `队列已满（${queued}/${queueLimit}），等前面的消息处理完成后再发送`
            : `已排队：${queued} 条待处理${(queue?.mine || 0) > 1 ? ` · 你的消息第 ${queue.mine} 位` : ''}`}
        </div>
      )}
      <div
        className={`composer-box ${dragging ? 'dragging' : ''}`}
        onClick={(event) => { if (!event.target.closest('button, input, textarea, .popover')) areaRef.current?.focus() }}
        onDragOver={(event) => { event.preventDefault(); setDragging(true) }}
        onDragLeave={() => setDragging(false)}
        onDrop={(event) => { event.preventDefault(); setDragging(false); uploadFile(event.dataTransfer.files) }}
      >
        <textarea
          ref={areaRef}
          value={value}
          rows={1}
          readOnly={voiceActive}
          className={voiceActive ? 'voice-input' : ''}
          placeholder={voiceActive ? '正在聆听…' : currentId ? '@ 添加上下文，/ 使用命令和技能' : '直接输入即可发送，会在当前工作区新建任务'}
          onChange={(event) => setValue(event.target.value)}
          onKeyDown={onKeyDown}
          onPaste={onPaste}
        />
        {dragging && <div className="composer-drop">松开以上传文件</div>}
        {voiceActive ? (
          <div className="composer-bar voice-bar">
            <button type="button" className="voice-btn cancel" onClick={() => finishVoice(true)} title="取消语音输入" aria-label="取消语音输入">
              <Icon name="x" size={18} />
            </button>
            <div className="voice-wave" aria-hidden="true">
              {waveBars.map((level, index) => (
                <span key={index} style={{ height: `${Math.round(3 + level * 25)}px` }} />
              ))}
            </div>
            <span className="voice-time">{voiceClock(voiceElapsed)}</span>
            <button type="button" className="voice-btn confirm" onClick={() => finishVoice(false)} title="确认语音输入" aria-label="确认语音输入">
              <Icon name="check" size={18} />
            </button>
          </div>
        ) : (
        <div className="composer-bar">
          <div className="bar-left">
            <div className="bar-item">
              <button
                type="button"
                className={`icon-btn ${openMenu === 'plus' ? 'active' : ''}`}
                title="添加上下文"
                aria-label="添加上下文"
                onClick={() => setOpenMenu(openMenu === 'plus' ? null : 'plus')}
                disabled={uploading}
              >
                <Icon name="plus" />
              </button>
              {openMenu === 'plus' && (
                <Popover className="left">
                  <PopoverItem icon="paperclip" label="上传文件" hint="图片 / 文档，也可直接粘贴" onClick={() => { setOpenMenu(null); uploadRef.current?.click() }} />
                  <PopoverItem glyph="@" label="引用文件" hint="输入 @ 选择工作区文件" onClick={() => { setOpenMenu(null); insert('@') }} />
                  <PopoverItem glyph="/" label="命令与技能" hint="输入 / 打开快捷命令" onClick={() => { setOpenMenu(null); insert('/') }} />
                  <div className="popover-sep" />
                  <PopoverItem icon="paperclip" label="打开附件面板" onClick={() => { setOpenMenu(null); onQuickAction('attachments') }} />
                </Popover>
              )}
            </div>
            <div className="bar-item">
              <button
                type="button"
                className={`permission-chip tone-${perm.tone} ${openMenu === 'permission' ? 'active' : ''}`}
                title="本轮权限模式"
                onClick={() => setOpenMenu(openMenu === 'permission' ? null : 'permission')}
              >
                <Icon name="shield" size={15} />
                <span>{perm.label}</span>
                <Icon name="chevron" size={12} className="chevron" />
              </button>
              {openMenu === 'permission' && (
                <Popover className="left perm-pop">
                  <div className="popover-head">本轮权限</div>
                  {PERMISSION_PRESETS.map((option) => (
                    <PopoverItem
                      key={option.id}
                      icon="shield"
                      iconTone={`tone-${option.tone}`}
                      label={option.label}
                      hint={option.hint}
                      selected={option.id === permission}
                      onClick={() => { onPermission(option.id); setOpenMenu(null) }}
                    />
                  ))}
                </Popover>
              )}
            </div>
            <div className="bar-item">
              <button
                type="button"
                className={`caps-pill ${openMenu === 'caps' ? 'active' : ''}`}
                title="能力"
                aria-label="能力"
                onClick={() => setOpenMenu(openMenu === 'caps' ? null : 'caps')}
              >
                <span className="caps-brand">
                  <svg viewBox="0 0 24 24" width="15" height="15" aria-hidden="true">
                    <path d="M4 15.4c3-4.6 6.6-6.1 10.7-4.4l-1.3 2.7c-2.2 1.3-4.5 1.7-7 1.1L4 15.4Z" fill="#ffffff" opacity=".95" />
                    <path d="M9.7 6.8c3.9-.2 7.3 1.7 10 5.6-2.3.7-4.4.5-6.4-.5-1.9-1-3.1-2.7-3.6-5.1Z" fill="#ffffff" opacity=".8" />
                  </svg>
                </span>
                <span className="caps-mini"><Icon name="search" size={11} /></span>
                <span className="caps-mini"><Icon name="doc" size={11} /></span>
              </button>
              {openMenu === 'caps' && (
                <Popover className="left caps-pop">
                  <div className="popover-head">能力</div>
                  {QUICK_ACTIONS.map((action) => (
                    <PopoverItem
                      key={action.cmd}
                      icon={CAP_ICONS[action.run]}
                      label={action.label}
                      hint={action.cmd}
                      onClick={() => { setOpenMenu(null); onQuickAction(action.run) }}
                    />
                  ))}
                </Popover>
              )}
            </div>
          </div>
          <span className="bar-spacer" />
          <div className="bar-item model-wrap">
            <button
              type="button"
              className={`model-btn ${openMenu === 'model' ? 'active' : ''}`}
              title="本轮模型"
              onClick={() => { setModelDraft(model || ''); setOpenMenu(openMenu === 'model' ? null : 'model') }}
            >
              <span className="model-name">{shownModel.label}</span>
              {shownModel.badge ? <span className="model-badge">{shownModel.badge}</span> : null}
              <Icon name="chevron" size={12} className="chevron" />
            </button>
            {openMenu === 'model' && (
              <Popover className="right model-pop">
                <div className="popover-head">模型</div>
                {MODEL_PRESETS.map((option) => (
                  <PopoverItem
                    key={option.id || 'default'}
                    label={option.label}
                    badge={option.badge}
                    hint={option.hint}
                    selected={(model || '') === option.id}
                    onClick={() => { onModel(option.id); setOpenMenu(null) }}
                  />
                ))}
                <div className="popover-sep" />
                <div className="model-custom">
                  <input
                    value={modelDraft}
                    placeholder="自定义模型名"
                    onChange={(event) => setModelDraft(event.target.value)}
                    onKeyDown={(event) => {
                      if (event.key === 'Enter' && !event.nativeEvent.isComposing) { event.preventDefault(); submitCustomModel() }
                    }}
                  />
                  <button type="button" onClick={submitCustomModel} disabled={!modelDraft.trim()}>应用</button>
                </div>
              </Popover>
            )}
          </div>
          <button type="button" className="icon-btn" title="语音输入" aria-label="语音输入" onClick={startVoice}>
            <Icon name="mic" />
          </button>
          <button type="button" className="icon-btn" title="语音讨论" aria-label="语音讨论" onClick={onVoiceDiscussion}>
            <Icon name="wave" />
          </button>
          {running && (
            <button type="button" className="send-btn stop" onClick={onStop} title="停止" aria-label="停止"><Icon name="stop" size={16} /></button>
          )}
          <button
            type="submit"
            className="send-btn"
            disabled={!canSend || queueFull}
            title={queueFull
              ? `队列已满（${queued}/${queueLimit}）`
              : !canSend
                ? '输入内容后发送（Enter）'
                : running
                  ? '追加发送（Enter）· 会排在当前任务之后'
                  : currentId ? '发送（Enter）' : '发送（Enter）· 会在当前工作区新建任务'}
            aria-label="发送"
          ><Icon name="arrowUp" /></button>
        </div>
        )}
        <input ref={uploadRef} type="file" multiple style={{ display: 'none' }} onChange={(event) => uploadFile(event.target.files)} />
      </div>
      <div className="composer-footer">
        <div className="footer-item">
          <button type="button" className="footer-btn" onClick={() => setFooterMenu(footerMenu === 'env' ? null : 'env')}>
            <Icon name="laptop" size={16} />
            <span>本地</span>
            <Icon name="chevron" size={12} className="chevron" />
          </button>
          {footerMenu === 'env' && (
            <Popover className="left footer-pop">
              <div className="popover-head">运行环境</div>
              <PopoverItem icon="laptop" label="本地" hint="claw serve 运行在本机" selected onClick={() => setFooterMenu(null)} />
              <PopoverItem icon="cloud" label="远程 / 云端" hint="当前构建未启用" disabled />
            </Popover>
          )}
        </div>
        <div className="footer-item">
          <button type="button" className="footer-btn" onClick={() => setFooterMenu(footerMenu === 'workspace' ? null : 'workspace')}>
            <Icon name="folder" size={16} />
            <span className="workspace-name">{basename(workspace) || '未选择工作区'}</span>
            <Icon name="chevron" size={12} className="chevron" />
          </button>
          {footerMenu === 'workspace' && (
            <Popover className="left footer-pop workspace-pop">
              <div className="popover-head">{currentId ? '工作区 · 切换会新建任务' : '工作区 · 首个消息在此新建任务'}</div>
              {workspaces.map((item) => (
                <PopoverItem
                  key={item.path}
                  icon="folder"
                  label={basename(item.path)}
                  hint={item.path}
                  selected={item.path === workspace}
                  onClick={() => {
                    setFooterMenu(null)
                    if (item.path === workspace) return
                    // 没有选中会话时切换工作区不该建空会话：先只记住目标工作区，
                    // 用户发出第一条消息时再在那个工作区里新建。
                    if (!currentId) onPickWorkspace?.(item.path)
                    else onNewInWorkspace?.(item.path)
                  }}
                />
              ))}
              {workspaces.length === 0 && <div className="popover-empty">还没有工作区</div>}
              <div className="popover-sep" />
              <PopoverItem icon="plus" label="选择其他文件夹…" onClick={() => { setFooterMenu(null); onOpenWorkspacePicker?.() }} />
            </Popover>
          )}
        </div>
        {/* 统计信息与工作区控件同行：左侧是本地/工作区，右侧是轮次与 token 用量，省出一整行高度。 */}
        {(stats?.turns > 0 || stats?.total_tokens > 0) && (
          <div className="composer-metrics">
            <span>↻ {stats.turns || 0} 轮 {stats.steps || 0} 步{stats.duration_ms > 0 && stats.output_tokens > 0 ? ` · ${Math.max(1, Math.round((stats.output_tokens || 0) / (stats.duration_ms / 1000)))} tok/s` : ''}</span>
            <span>💾 {formatTokens(stats.total_tokens || 0)} tok · 缓存命中 {stats.cache_hit_pct || 0}%</span>
          </div>
        )}
      </div>
    </form>
  )
}
