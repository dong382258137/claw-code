import React, { useCallback, useEffect, useMemo, useState } from 'react'
import { api, postJson, withWorkspace } from '../api'

const EMPTY_DIFF = { files: [], untracked: [], patch: '', hunks: [], is_git: true }

/// 给 hunk 的每行补出旧/新文件行号。
///
/// 引用统一用**新文件行号**（agent 之后 read_file 看到的就是这份文件）。
/// 删除行在新文件里没有独立行号，锚定到它所在的插入位置（当前 newNo），
/// 这样引用仍然落在删除发生处的邻近行，而不是一个不存在的行号。
function annotateHunk(hunk) {
  let oldNo = Number(hunk.old_start) || 1
  let newNo = Number(hunk.new_start) || 1
  return (hunk.lines || []).map((line, index) => {
    const tag = line.startsWith('+') ? '+' : line.startsWith('-') ? '-' : ' '
    const anchor = newNo
    const oldLabel = tag === '+' ? '' : String(oldNo)
    const newLabel = tag === '-' ? '' : String(newNo)
    if (tag === '+') newNo += 1
    else if (tag === '-') oldNo += 1
    else { oldNo += 1; newNo += 1 }
    return { line, tag, oldLabel, newLabel, anchor, index }
  })
}

function checkpointLabel(checkpoint) {
  if (!checkpoint) return ''
  return checkpoint.seq === 0 ? '起始状态' : `第 ${checkpoint.seq} 轮`
}

function formatTime(ms) {
  if (!ms) return ''
  return new Date(ms).toLocaleString('zh-CN', { hour12: false })
}

/// 逐 hunk 审查：可逐块丢弃；可选中行/区间插入 `@路径:行号` 引用；
/// 可按轮次把工作区回滚到某一轮结束时的状态。
export function DiffPane({ workspace, currentId, onError, onReference }) {
  const [diff, setDiff] = useState(EMPTY_DIFF)
  const [selected, setSelected] = useState('')
  // 选中文件的单文件 patch：未跟踪文件没有 hunk，只能靠它展示内容。
  const [filePatch, setFilePatch] = useState('')
  const [busy, setBusy] = useState('')

  // 行级提问：selection 记录起点/终点（新文件行号），拖动或 Shift+点击扩展。
  const [selection, setSelection] = useState(null)
  const [dragging, setDragging] = useState(false)
  const [quoteNote, setQuoteNote] = useState('')

  const [checkpoints, setCheckpoints] = useState([])
  const [showCheckpoints, setShowCheckpoints] = useState(false)
  const [rollbackPreview, setRollbackPreview] = useState(null)
  const [checkpointBusy, setCheckpointBusy] = useState('')
  const [checkpointNote, setCheckpointNote] = useState('')

  const load = useCallback(() => {
    api(withWorkspace('/api/workspace/diff', workspace))
      .then((next) => setDiff({
        files: next.files || [],
        untracked: next.untracked || [],
        patch: next.patch || '',
        hunks: next.hunks || [],
        // 后端明确返回 is_git=false 时展示原因；旧后端没有该字段按可用处理。
        is_git: next.is_git !== false,
      }))
      .catch((error) => {
        setDiff((previous) => ({ ...previous, is_git: null }))
        onError?.(`diff 失败：${error.message}`)
      })
  }, [onError, workspace])
  useEffect(() => { load() }, [load])

  // 选中文件变化时取该文件的单文件 patch（未跟踪文件没有 hunk，只有这条路能展示）。
  useEffect(() => {
    if (!selected) { setFilePatch(''); return }
    api(withWorkspace(`/api/workspace/diff?path=${encodeURIComponent(selected)}`, workspace))
      .then((single) => setFilePatch(single.patch || ''))
      .catch(() => setFilePatch(''))
  }, [selected, workspace])

  // 拖选到面板外松手也要结束拖动。
  useEffect(() => {
    const stop = () => setDragging(false)
    window.addEventListener('mouseup', stop)
    return () => window.removeEventListener('mouseup', stop)
  }, [])

  const loadCheckpoints = useCallback(() => {
    if (!currentId) { setCheckpoints([]); return }
    api(`/api/session/${encodeURIComponent(currentId)}/checkpoints`)
      .then((next) => setCheckpoints(next.checkpoints || []))
      .catch((error) => onError?.(`检查点读取失败：${error.message}`))
  }, [currentId, onError])

  const toggleCheckpoints = () => {
    setShowCheckpoints((value) => {
      if (!value) loadCheckpoints()
      return !value
    })
  }

  const discard = async (hunk) => {
    const key = `${hunk.path}#${hunk.index}`
    setBusy(key)
    try {
      await postJson(withWorkspace('/api/workspace/diff/discard', workspace), {
        path: hunk.path,
        index: hunk.index,
        // 指纹是唯一权威判据：服务端用它确认「讨论的是同一块」，
        // 再逐行比对文件内容，任一不符即整块拒绝。
        fingerprint: hunk.fingerprint,
      })
      // 丢弃一块后，同文件其余 hunk 的行号与 index 都会重排，必须整表重取。
      setSelection(null)
      load()
    } catch (error) {
      onError?.(`丢弃失败：${error.message}`)
      load()
    } finally {
      setBusy('')
    }
  }

  const previewRollback = async (checkpoint) => {
    if (!currentId) return
    setCheckpointBusy(checkpoint.id)
    setCheckpointNote('')
    try {
      const response = await postJson(
        `/api/session/${encodeURIComponent(currentId)}/checkpoints/rollback`,
        { checkpoint: checkpoint.id, dry_run: true },
      )
      setRollbackPreview({ checkpoint, plan: response.plan })
    } catch (error) {
      onError?.(`预览回滚失败：${error.message}`)
    } finally {
      setCheckpointBusy('')
    }
  }

  const applyRollback = async () => {
    if (!currentId || !rollbackPreview) return
    const { checkpoint, plan } = rollbackPreview
    setCheckpointBusy(checkpoint.id)
    try {
      await postJson(
        `/api/session/${encodeURIComponent(currentId)}/checkpoints/rollback`,
        { checkpoint: checkpoint.id, token: plan.token },
      )
      setRollbackPreview(null)
      setCheckpointNote(
        `已回滚到${checkpointLabel(checkpoint)}（恢复 ${plan.restore} 个文件、删除 ${plan.delete} 个）`,
      )
      setSelected('')
      setSelection(null)
      load()
      loadCheckpoints()
    } catch (error) {
      onError?.(`回滚失败：${error.message}`)
    } finally {
      setCheckpointBusy('')
    }
  }

  const selectedRange = useMemo(() => {
    if (!selection) return null
    return {
      lo: Math.min(selection.start, selection.end),
      hi: Math.max(selection.start, selection.end),
    }
  }, [selection])

  const startSelection = (event, hunkKey, path, anchor) => {
    if (event.shiftKey && selection && selection.hunkKey === hunkKey) {
      event.preventDefault()
      setSelection({ hunkKey, path, start: selection.start, end: anchor })
      setDragging(false)
      return
    }
    event.preventDefault()
    setSelection({ hunkKey, path, start: anchor, end: anchor })
    setDragging(true)
    setQuoteNote('')
  }

  const extendSelection = (hunkKey, anchor) => {
    if (!dragging) return
    setSelection((previous) => (
      previous && previous.hunkKey === hunkKey ? { ...previous, end: anchor } : previous
    ))
  }

  const quoteSelection = () => {
    if (!selection || !selectedRange) return
    const { lo, hi } = selectedRange
    // 与 FilesPane 保持一致：`@path:12 ` / `@path:12-40 `，服务端会展开成带行号的代码块。
    const ref = lo === hi ? `@${selection.path}:${lo} ` : `@${selection.path}:${lo}-${hi} `
    onReference?.(ref)
    setQuoteNote(`已插入引用 ${ref.trim()}`)
    setSelection(null)
  }

  const visible = selected ? diff.hunks.filter((hunk) => hunk.path === selected) : []
  const hasHunks = visible.length > 0

  return (
    <div className="pane">
      <div className="pane-toolbar">
        {selected
          ? <button onClick={() => { setSelected(''); setSelection(null); load() }}>‹ 列表</button>
          : <button onClick={load}>刷新</button>}
        {currentId && (
          <button
            className={showCheckpoints ? 'active' : ''}
            title="按轮次把工作区恢复到该轮结束时的状态"
            onClick={toggleCheckpoints}
          >轮次</button>
        )}
        <span className="hint">{diff.files.length} 个已修改 · {diff.untracked.length} 个未跟踪</span>
      </div>
      <div className="pane-body scroll">
        {/* 非 git：面板此前只会显示 0 个改动（git diff 的错误文本被忽略）。 */}
        {diff.is_git === false && (
          <div className="diff-git-notice">
            <strong>当前工作区不是 Git 仓库，差异对比与逐块丢弃不可用。</strong>
            <p>这个面板靠 <code>git diff</code> 读取改动，非 Git 目录下没有可比对的基线。可以：</p>
            <ul>
              <li>在项目根目录执行 <code>git init</code> 并提交一次，再点「刷新」；</li>
              <li>用「文件」标签直接查看或编辑文件；</li>
              <li>「轮次」检查点回滚<b>不依赖 Git</b>，仍可用于恢复工作区。</li>
            </ul>
          </div>
        )}

        {showCheckpoints && currentId && (
          <div className="cp-panel">
            <div className="cp-title">
              轮次检查点
              <span className="muted">回滚会把工作区文件恢复到该轮结束时的内容</span>
            </div>
            {checkpoints.length === 0 && (
              <div className="hint">还没有检查点：完成一轮对话后会在轮次结束时自动生成。</div>
            )}
            {checkpoints.map((checkpoint) => (
              <div className="cp-row" key={checkpoint.id}>
                <span className="cp-seq">{checkpointLabel(checkpoint)}</span>
                <span className="cp-label" title={checkpoint.label}>{checkpoint.label || '（无提示）'}</span>
                <span className="cp-meta">
                  {checkpoint.files} 文件{checkpoint.partial ? ' · 部分' : ''}
                  <br />
                  {formatTime(checkpoint.created_ms)}
                </span>
                <button
                  className="ghost"
                  disabled={checkpointBusy === checkpoint.id}
                  onClick={() => previewRollback(checkpoint)}
                >{checkpointBusy === checkpoint.id ? '读取…' : '回滚到此'}</button>
              </div>
            ))}

            {rollbackPreview && (
              <div className="cp-confirm">
                <div>
                  确认回滚到 <b>{checkpointLabel(rollbackPreview.checkpoint)}</b>？
                </div>
                <div className="cp-summary">
                  恢复 {rollbackPreview.plan.restore} · 删除 {rollbackPreview.plan.delete}
                  {' · '}未变 {rollbackPreview.plan.unchanged}
                  {rollbackPreview.plan.skipped > 0 && ` · 跳过 ${rollbackPreview.plan.skipped}`}
                </div>
                {rollbackPreview.plan.partial && (
                  <div className="warn">
                    该检查点未完整捕获工作区
                    {rollbackPreview.checkpoint.reason ? `（${rollbackPreview.checkpoint.reason}）` : ''}
                    ：回滚只覆盖已捕获文件，不会删除新增文件。
                  </div>
                )}
                {rollbackPreview.plan.sample && rollbackPreview.plan.sample.length > 0 && (
                  <pre className="cp-sample">{rollbackPreview.plan.sample.join('\n')}</pre>
                )}
                <div className="cp-actions">
                  <button className="ghost" onClick={() => setRollbackPreview(null)}>取消</button>
                  <button onClick={applyRollback} disabled={Boolean(checkpointBusy)}>确认回滚</button>
                </div>
                <div className="hint">回滚会覆盖这些文件当前的未提交改动（包括你自己的修改）。</div>
              </div>
            )}
            {checkpointNote && <div className="hint">{checkpointNote}</div>}
          </div>
        )}

        {!selected && diff.files.map((file) => (
          <button key={file.path} className="diff-row" onClick={() => setSelected(file.path)}>
            <span>{file.path}</span>
            <span><span className="add">+{file.added}</span> <span className="del">-{file.removed}</span></span>
          </button>
        ))}
        {!selected && diff.untracked.length > 0 && <div className="group">未跟踪</div>}
        {!selected && diff.untracked.map((path) => (
          <button key={path} className="diff-row" onClick={() => setSelected(path)}><span>{path}</span><span className="muted">新增</span></button>
        ))}

        {selected && hasHunks && (
          <>
            {/* 固定高度占位：选中行时插入操作条不会把下方 diff 顶下去，
                否则鼠标下的行会瞬间换行，单击变成误选区间。 */}
            <div className="diff-sel-slot">
              {selection && selectedRange && (
                <div className="diff-sel-bar">
                  <span className="path" title={selection.path}>{selection.path}</span>
                  <span>
                    {selectedRange.lo === selectedRange.hi
                      ? `第 ${selectedRange.lo} 行`
                      : `第 ${selectedRange.lo}-${selectedRange.hi} 行`}
                  </span>
                  <span className="spacer" />
                  {onReference && <button onClick={quoteSelection}>插入引用</button>}
                  <button className="ghost" onClick={() => setSelection(null)}>清除</button>
                </div>
              )}
              {!selection && quoteNote && <div className="hint">{quoteNote}</div>}
            </div>
            {visible.map((hunk) => {
              const key = `${hunk.path}#${hunk.index}`
              const lines = annotateHunk(hunk)
              return (
                <div key={key} className="hunk">
                  <div className="hunk-head">
                    <span className="hunk-title">{hunk.header || `@@ 第 ${hunk.new_start} 行`}</span>
                    <button
                      className="ghost"
                      disabled={busy === key}
                      title="把这一块改回未修改状态（其余块保留）"
                      onClick={() => discard(hunk)}
                    >{busy === key ? '丢弃中…' : '丢弃此块'}</button>
                  </div>
                  <div className="patch diff-lines">
                    {lines.map(({ line, tag, oldLabel, newLabel, anchor, index }) => {
                      const active = selection
                        && selection.hunkKey === key
                        && selectedRange
                        && anchor >= selectedRange.lo
                        && anchor <= selectedRange.hi
                      return (
                        <div
                          key={index}
                          className={`diff-line ${tag === '+' ? 'l-add' : tag === '-' ? 'l-del' : ''} ${active ? 'selected' : ''}`}
                          title="点击选中一行；拖动或 Shift+点击选择一段，然后插入引用"
                          onMouseDown={(event) => startSelection(event, key, hunk.path, anchor)}
                          onMouseEnter={() => extendSelection(key, anchor)}
                        >
                          <span className="diff-ln">{oldLabel}</span>
                          <span className="diff-ln">{newLabel}</span>
                          <span className="diff-code">{line || ' '}</span>
                        </div>
                      )
                    })}
                  </div>
                </div>
              )
            })}
          </>
        )}

        {/* 无 hunk 时（未跟踪文件、二进制、纯 mode 变更）回退到该文件的原始 patch 文本。 */}
        {selected && !hasHunks && (
          <pre className="patch">
            {(() => {
              const lines = String(filePatch || '').split('\n')
              const shown = lines.slice(0, 4000)
              return (
                <>
                  {shown.map((line, index) => <span key={index} className={line.startsWith('+') ? 'l-add' : line.startsWith('-') ? 'l-del' : line.startsWith('@@') ? 'l-hunk' : ''}>{line + '\n'}</span>)}
                  {lines.length > shown.length && <span className="muted">{`…已截断 ${lines.length - shown.length} 行…`}</span>}
                  {!filePatch && <span className="muted">该文件没有可展示的文本差异（未跟踪或二进制）。</span>}
                </>
              )
            })()}
          </pre>
        )}
        {selected && <div className="hint">当前查看：{selected}</div>}
      </div>
    </div>
  )
}
