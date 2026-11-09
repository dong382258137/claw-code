import React, { useCallback, useEffect, useState } from 'react'
import { api, postJson, withWorkspace } from '../api'

/// 逐 hunk 审查：每个 hunk 可单独「丢弃」，等价 `git checkout -p` 对该块答 no。
export function DiffPane({ workspace, onError }) {
  const [diff, setDiff] = useState({ files: [], untracked: [], patch: '', hunks: [] })
  const [selected, setSelected] = useState('')
  // 选中文件的单文件 patch：未跟踪文件没有 hunk，只能靠它展示内容。
  const [filePatch, setFilePatch] = useState('')
  const [busy, setBusy] = useState('')

  const load = useCallback(() => {
    api(withWorkspace('/api/workspace/diff', workspace))
      .then((next) => setDiff({
        files: next.files || [],
        untracked: next.untracked || [],
        patch: next.patch || '',
        hunks: next.hunks || [],
      }))
      .catch((error) => onError?.(`diff 失败：${error.message}`))
  }, [onError, workspace])
  useEffect(() => { load() }, [load])

  // 选中文件变化时取该文件的单文件 patch（未跟踪文件没有 hunk，只有这条路能展示）。
  useEffect(() => {
    if (!selected) { setFilePatch(''); return }
    api(withWorkspace(`/api/workspace/diff?path=${encodeURIComponent(selected)}`, workspace))
      .then((single) => setFilePatch(single.patch || ''))
      .catch(() => setFilePatch(''))
  }, [selected, workspace])

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
      load()
    } catch (error) {
      onError?.(`丢弃失败：${error.message}`)
      load()
    } finally {
      setBusy('')
    }
  }

  const visible = selected ? diff.hunks.filter((hunk) => hunk.path === selected) : []
  const hasHunks = visible.length > 0

  return (
    <div className="pane">
      <div className="pane-toolbar">
        {selected
          ? <button onClick={() => { setSelected(''); load() }}>‹ 列表</button>
          : <button onClick={load}>刷新</button>}
        <span className="hint">{diff.files.length} 个已修改 · {diff.untracked.length} 个未跟踪</span>
      </div>
      <div className="pane-body scroll">
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

        {selected && hasHunks && visible.map((hunk) => {
          const key = `${hunk.path}#${hunk.index}`
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
              <pre className="patch">
                {(hunk.lines || []).map((line, index) => (
                  <span
                    key={index}
                    className={line.startsWith('+') ? 'l-add' : line.startsWith('-') ? 'l-del' : ''}
                  >{line + '\n'}</span>
                ))}
              </pre>
            </div>
          )
        })}

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
