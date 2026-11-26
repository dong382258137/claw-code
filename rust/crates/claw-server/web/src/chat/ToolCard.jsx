import React, { useMemo, useState } from 'react'

// 工具卡定制渲染：对齐 TUI（src/tool_display.rs、src/tui/tool_card.rs）的信息层次。
// row.input / row.output 都是字符串；多数工具输入是 JSON，输出是 JSON 信封，
// 少数工具（create_plan、edit_file 失败等）输出是纯文本，这里统一做容错解析。

const KINDS = [
  { kind: 'bash', test: /^(bash|bash_command|shell|powershell|pwsh|exec|run_command)$/i },
  { kind: 'read', test: /^(read_file|read|view_file)$/i },
  { kind: 'write', test: /^(write_file|write|create_file)$/i },
  { kind: 'edit', test: /^(edit_file|edit|multi_edit|multiedit|replace_lines|apply_patch|str_replace)$/i },
  { kind: 'grep', test: /^(grep_search|grep|search_code)$/i },
  { kind: 'glob', test: /^(glob_search|glob|list_files)$/i },
  { kind: 'plan', test: /^(todowrite|todo_write|create_plan|plan_update|update_plan|plan)$/i },
  { kind: 'web', test: /^(webfetch|web_fetch|websearch|web_search)$/i },
  { kind: 'agent', test: /^(task|dispatch_subagent|spawn_parallel_subagents|taskoutput|taskstop|taskcreate|tasklist|taskget)$/i },
  { kind: 'skill', test: /^(skill|skillsearch|skill_search)$/i },
  { kind: 'ask', test: /^(askuserquestion|ask_user_question)$/i },
  { kind: 'repl', test: /^(repl)$/i },
]

const KIND_LABEL = {
  bash: { icon: '❯', verb: '执行命令' },
  read: { icon: '▤', verb: '读取文件' },
  write: { icon: '✎', verb: '写入文件' },
  edit: { icon: '✎', verb: '修改文件' },
  grep: { icon: '⌕', verb: '搜索代码' },
  glob: { icon: '⌗', verb: '查找文件' },
  plan: { icon: '☑', verb: '更新计划' },
  web: { icon: '⊕', verb: '访问网页' },
  agent: { icon: '⚙', verb: '子代理' },
  skill: { icon: '⚙', verb: '调用技能' },
  ask: { icon: '?', verb: '询问用户' },
  repl: { icon: '❯', verb: '执行代码' },
  generic: { icon: '⚙', verb: '调用工具' },
}

const PATH_KEYS = ['file_path', 'filePath', 'path', 'filename']

function parseMaybeJson(text) {
  if (typeof text !== 'string') return null
  const trimmed = text.trim()
  if (!trimmed.startsWith('{') && !trimmed.startsWith('[')) return null
  try {
    return JSON.parse(trimmed)
  } catch (_) {
    return null
  }
}

function pick(object, keys) {
  if (!object || typeof object !== 'object') return undefined
  for (const key of keys) {
    const value = object[key]
    if (value !== undefined && value !== null && value !== '') return value
  }
  return undefined
}

function cleanPath(value) {
  return String(value == null ? '' : value).replace(/^\\\\\?\\/, '')
}

// JSON 块被截断时无法整体 parse：用正则兜底取出靠前的字符串字段（路径等）。
function looseString(text, keys) {
  const source = String(text == null ? '' : text)
  for (const key of keys) {
    const match = new RegExp(`"${key}"\\s*:\\s*"((?:[^"\\\\]|\\\\.)*)"`).exec(source)
    if (match) {
      try {
        return JSON.parse(`"${match[1]}"`)
      } catch (_) {
        return match[1]
      }
    }
  }
  return ''
}

function firstLine(text, max = 72) {
  const line = String(text == null ? '' : text).split('\n').find((item) => item.trim()) || ''
  return truncateText(line.trim(), max)
}

function truncateText(text, max) {
  const value = String(text == null ? '' : text)
  return value.length > max ? value.slice(0, max) + '…' : value
}

function lineCount(text) {
  if (!text) return 0
  return String(text).split('\n').filter((line, index, all) => index < all.length - 1 || line !== '').length
}

function truncateLines(text, max) {
  const lines = String(text == null ? '' : text).split('\n')
  if (lines.length <= max) return { text: lines.join('\n'), omitted: 0 }
  return { text: lines.slice(0, max).join('\n'), omitted: lines.length - max }
}

function kindOf(name) {
  const found = KINDS.find((item) => item.test.test(String(name || '')))
  return found ? found.kind : 'generic'
}

function colorizePatch(patch) {
  return String(patch || '').split('\n').map((line, index) => {
    let cls = ''
    if (line.startsWith('@@')) cls = 'l-hunk'
    else if (line.startsWith('+++') || line.startsWith('---') || line.startsWith('diff ')) cls = 'l-meta'
    else if (line.startsWith('+')) cls = 'l-add'
    else if (line.startsWith('-')) cls = 'l-del'
    return <span key={index} className={cls}>{line + '\n'}</span>
  })
}

// 结构化补丁（write_file / edit_file 的 structuredPatch）→ 统一 diff 文本。
function patchFromStructured(value) {
  if (!Array.isArray(value)) return ''
  const lines = []
  for (const hunk of value) {
    lines.push('@@')
    for (const line of (hunk && hunk.lines) || []) lines.push(String(line))
  }
  return lines.join('\n')
}

// 输入里只有 old/new 时，前端做一次行级 LCS，生成 -/+ 预览与增删计数。
function lineDiff(oldText, newText, max = 160) {
  const left = String(oldText == null ? '' : oldText).split('\n')
  const right = String(newText == null ? '' : newText).split('\n')
  const n = left.length
  const m = right.length
  if (n * m > 40000) {
    const all = []
    for (const line of left) all.push({ type: '-', line })
    for (const line of right) all.push({ type: '+', line })
    return { entries: all.slice(0, max), omitted: Math.max(0, all.length - max) }
  }
  const dp = Array.from({ length: n + 1 }, () => new Uint16Array(m + 1))
  for (let i = n - 1; i >= 0; i -= 1) {
    for (let j = m - 1; j >= 0; j -= 1) {
      dp[i][j] = left[i] === right[j]
        ? dp[i + 1][j + 1] + 1
        : Math.max(dp[i + 1][j], dp[i][j + 1])
    }
  }
  const entries = []
  let i = 0
  let j = 0
  while (i < n && j < m) {
    if (left[i] === right[j]) {
      entries.push({ type: ' ', line: left[i] })
      i += 1
      j += 1
    } else if (dp[i + 1][j] >= dp[i][j + 1]) {
      entries.push({ type: '-', line: left[i] })
      i += 1
    } else {
      entries.push({ type: '+', line: right[j] })
      j += 1
    }
  }
  while (i < n) {
    entries.push({ type: '-', line: left[i] })
    i += 1
  }
  while (j < m) {
    entries.push({ type: '+', line: right[j] })
    j += 1
  }
  return { entries: entries.slice(0, max), omitted: Math.max(0, entries.length - max) }
}

function diffStat(patch, entries) {
  if (entries) {
    return {
      add: entries.filter((entry) => entry.type === '+').length,
      del: entries.filter((entry) => entry.type === '-').length,
    }
  }
  let add = 0
  let del = 0
  for (const line of String(patch || '').split('\n')) {
    if (line.startsWith('+') && !line.startsWith('+++')) add += 1
    else if (line.startsWith('-') && !line.startsWith('---')) del += 1
  }
  return { add, del }
}

function DiffPatch({ patch }) {
  const { text, omitted } = truncateLines(patch, 60)
  return (
    <>
      <pre className="patch">{colorizePatch(text)}</pre>
      {omitted > 0 && <div className="tool-more">…还有 {omitted} 行，展开「原始数据」查看</div>}
    </>
  )
}

function LineDiff({ entries, omitted }) {
  return (
    <>
      <pre className="patch">
        {entries.map((entry, index) => (
          <span
            key={index}
            className={entry.type === '+' ? 'l-add' : entry.type === '-' ? 'l-del' : 'l-ctx'}
          >
            {(entry.type === ' ' ? ' ' : entry.type) + entry.line + '\n'}
          </span>
        ))}
      </pre>
      {omitted > 0 && <div className="tool-more">…还有 {omitted} 行，展开「原始数据」查看</div>}
    </>
  )
}

function TextPreview({ text, max = 40, className = '' }) {
  const { text: shown, omitted } = truncateLines(text, max)
  return (
    <>
      <pre className={className}>{shown}</pre>
      {omitted > 0 && <div className="tool-more">…还有 {omitted} 行，展开「原始数据」查看</div>}
    </>
  )
}

// 可点路径：点击复制完整路径（面板归另一个会话，这里不做跨面板跳转）。
function PathButton({ path }) {
  const clean = cleanPath(path)
  const [copied, setCopied] = useState(false)
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(clean)
      setCopied(true)
      setTimeout(() => setCopied(false), 1200)
    } catch (_) {}
  }
  if (!clean) return null
  return (
    <button
      type="button"
      className="tool-path"
      onClick={copy}
      title={copied ? '已复制' : `${clean}（点击复制路径）`}
    >
      {copied ? '已复制路径' : clean}
    </button>
  )
}

function FileList({ files, max = 12 }) {
  const list = (files || []).filter(Boolean)
  const shown = list.slice(0, max)
  return (
    <>
      <ul className="tool-files">
        {shown.map((file, index) => (
          <li key={`${file}-${index}`}><PathButton path={file} /></li>
        ))}
      </ul>
      {list.length > shown.length && (
        <div className="tool-more">…还有 {list.length - shown.length} 个文件，展开「原始数据」查看</div>
      )}
    </>
  )
}

// 网页搜索结果：把模型拿到的 markdown 链接抽成可点条目，避免直接糊一坨 JSON。
function LinkList({ items, max = 10 }) {
  const shown = items.slice(0, max)
  return (
    <>
      <ul className="tool-links">
        {shown.map((item, index) => (
          <li key={index}>
            {item.url
              ? <a href={item.url} target="_blank" rel="noreferrer">{item.title || item.url}</a>
              : <span>{truncateText(item.text, 160)}</span>}
            {item.url && item.text ? <div className="tool-link-sub">{truncateText(item.text, 160)}</div> : null}
          </li>
        ))}
      </ul>
      {items.length > shown.length && <div className="tool-more">…还有 {items.length - shown.length} 条结果</div>}
    </>
  )
}

function TodoList({ todos }) {
  const list = (todos || []).filter((todo) => todo && typeof todo === 'object')
  const shown = list.slice(0, 10)
  return (
    <>
      <ul className="tool-todos">
        {shown.map((todo, index) => {
          const mark = todo.status === 'completed' ? '✓' : todo.status === 'in_progress' ? '▸' : '○'
          const cls = todo.status === 'completed' ? 'done' : todo.status === 'in_progress' ? 'active' : ''
          const text = todo.status === 'in_progress' ? (todo.activeForm || todo.content) : todo.content
          return (
            <li key={index}>
              <span className={cls}>{mark}</span>
              <span className={cls}>{text}</span>
            </li>
          )
        })}
      </ul>
      {list.length > shown.length && (
        <div className="tool-more">…还有 {list.length - shown.length} 项，展开「原始数据」查看</div>
      )}
    </>
  )
}

// 按工具类型生成标题与正文；返回结构供 ToolCard 统一排版。
function describeTool(row) {
  const kind = kindOf(row.name)
  const parsedInput = parseMaybeJson(row.input)
  const parsedOutput = parseMaybeJson(row.output)
  const input = parsedInput && typeof parsedInput === 'object' && !Array.isArray(parsedInput) ? parsedInput : {}
  const out = parsedOutput && typeof parsedOutput === 'object' && !Array.isArray(parsedOutput) ? parsedOutput : null
  const rawOutput = typeof row.output === 'string' ? row.output : ''
  const view = {
    kind,
    icon: KIND_LABEL[kind].icon,
    verb: KIND_LABEL[kind].verb,
    path: '',
    target: '',
    badge: '',
    meta: '',
    body: null,
    errorText: '',
    isError: Boolean(row.error),
  }

  if (kind === 'bash' || kind === 'repl') {
    const command = pick(input, ['command', 'cmd', 'script', 'code'])
    const language = pick(input, ['language', 'lang'])
    const stdout = pick(out, ['stdout']) || ''
    const stderr = pick(out, ['stderr']) || ''
    const rc = pick(out, ['returnCodeInterpretation'])
    const exitCode = pick(out, ['exitCode'])
    const lines = lineCount(stdout)
    view.target = command ? '' : firstLine(rawOutput, 60)
    view.isError = view.isError || Boolean((rc && rc !== 'exit_code:0') || (exitCode != null && Number(exitCode) !== 0))
    view.badge = kind === 'repl' && language
      ? String(language)
      : rc
        ? String(rc).replace('exit_code:', '退出码 ')
        : exitCode != null
          ? `退出码 ${exitCode}`
          : lines
            ? `${lines} 行输出`
            : ''
    const body = []
    if (command) {
      body.push(<pre className="tool-term" key="cmd"><span className="prompt">$</span> {command}</pre>)
    }
    if (stdout.trim()) body.push(<TextPreview key="out" text={stdout} max={40} className="tool-stdout" />)
    if (stderr.trim()) body.push(<TextPreview key="err" text={stderr} max={30} className="tool-stderr" />)
    if (!out && rawOutput.trim() && rawOutput.trim() !== stdout.trim()) {
      body.push(<TextPreview key="raw" text={rawOutput} max={30} className="tool-stdout" />)
    }
    view.body = body.length ? body : <div className="tool-empty">（无输出）</div>
    return view
  }

  if (kind === 'read') {
    const file = out && out.file && typeof out.file === 'object' ? out.file : out
    const path = cleanPath(
      pick(input, PATH_KEYS) || pick(file, PATH_KEYS) || looseString(row.input, PATH_KEYS) || looseString(rawOutput, PATH_KEYS) || '',
    )
    const content = typeof file?.content === 'string' ? file.content : ''
    const startLine = Number(pick(file, ['startLine', 'start_line']) || 1)
    const numLines = Number(pick(file, ['numLines', 'num_lines']) || lineCount(content))
    const totalLines = Number(pick(file, ['totalLines', 'total_lines']) || numLines)
    view.path = path
    view.meta = numLines ? `第 ${startLine}–${startLine + Math.max(numLines - 1, 0)} 行 / 共 ${totalLines} 行` : ''
    view.badge = numLines ? `${numLines} 行` : ''
    if (content.trim()) {
      view.body = <TextPreview text={content} max={40} className="tool-code" />
    } else if (row.error) {
      view.body = null
      view.errorText = rawOutput
    } else if (!out && rawOutput.trim()) {
      view.body = <div className="tool-empty">（读取结果未能解析，展开「原始数据」查看）</div>
    } else {
      view.body = <div className="tool-empty">（文件内容为空）</div>
    }
    return view
  }

  if (kind === 'write') {
    const path = cleanPath(
      pick(input, PATH_KEYS) || pick(out, PATH_KEYS) || looseString(row.input, PATH_KEYS) || looseString(rawOutput, PATH_KEYS) || '',
    )
    const content = pick(input, ['content'])
    const lines = content ? lineCount(content) : lineCount(pick(out, ['content']) || '')
    const type = pick(out, ['type'])
    const patch = pick(out, ['gitDiff']) || patchFromStructured(pick(out, ['structuredPatch', 'structured_patch']))
    view.path = path
    view.badge = `${type === 'create' ? '新建' : '更新'}${lines ? ` · ${lines} 行` : ''}`
    view.body = patch
      ? <DiffPatch patch={patch} />
      : <div className="tool-empty">{type === 'create' ? '已创建文件' : '已写入文件'}</div>
    return view
  }

  if (kind === 'edit') {
    const path = cleanPath(
      pick(input, PATH_KEYS) || pick(out, PATH_KEYS) || looseString(row.input, PATH_KEYS) || looseString(rawOutput, PATH_KEYS) || '',
    )
    const oldText = pick(input, ['old_string', 'oldString', 'old_text', 'oldText'])
      || looseString(row.input, ['old_string', 'oldString'])
    const newText = pick(input, ['new_string', 'newString', 'new_text', 'newText'])
      || looseString(row.input, ['new_string', 'newString'])
    let patch = pick(out, ['gitDiff']) || patchFromStructured(pick(out, ['structuredPatch', 'structured_patch']))
    if (!patch && /^(diff --git|@@ )/m.test(rawOutput)) patch = rawOutput
    const entries = !patch && (oldText || newText) ? lineDiff(oldText, newText) : null
    const stat = diffStat(patch, entries && entries.entries)
    view.path = path
    view.badge = stat.add || stat.del ? `+${stat.add} −${stat.del}` : ''
    if (patch) view.body = <DiffPatch patch={patch} />
    else if (entries && entries.entries.length) view.body = <LineDiff entries={entries.entries} omitted={entries.omitted} />
    else if (rawOutput.trim() && !row.error && !/^\s*[{[]/.test(rawOutput)) view.body = <TextPreview text={rawOutput} max={20} />
    else view.body = <div className="tool-empty">（diff 预览不可用，展开「原始数据」查看）</div>
    if (row.error && rawOutput.trim()) view.errorText = rawOutput
    return view
  }

  if (kind === 'grep') {
    const pattern = pick(input, ['pattern', 'query'])
    const scope = cleanPath(pick(input, ['path', 'directory', 'glob']) || '')
    const numMatches = Number(pick(out, ['numMatches', 'num_matches', 'totalMatches']) || 0)
    const numFiles = Number(pick(out, ['numFiles', 'num_files', 'totalFiles']) || 0)
    const content = pick(out, ['content']) || ''
    const files = pick(out, ['filenames', 'files']) || []
    view.target = pattern ? <code className="tool-inline">{truncateText(pattern, 72)}</code> : ''
    view.meta = scope ? `范围 ${scope}` : ''
    view.badge = numMatches || numFiles ? `${numMatches ? `${numMatches} 处 / ` : ''}${numFiles} 文件` : ''
    if (content.trim()) view.body = <TextPreview text={content} max={40} className="tool-code" />
    else if (Array.isArray(files) && files.length) view.body = <FileList files={files} />
    else {
      view.body = <div className="tool-empty">（没有匹配）</div>
      if (row.error && rawOutput.trim()) view.errorText = rawOutput
    }
    return view
  }

  if (kind === 'glob') {
    const pattern = pick(input, ['pattern', 'glob'])
    const scope = cleanPath(pick(input, ['path', 'directory']) || '')
    const files = pick(out, ['filenames', 'files']) || []
    const numFiles = Number(pick(out, ['numFiles', 'num_files']) || (Array.isArray(files) ? files.length : 0))
    view.target = pattern ? <code className="tool-inline">{truncateText(pattern, 72)}</code> : ''
    view.meta = scope ? `范围 ${scope}` : ''
    view.badge = `${numFiles} 个文件`
    view.body = Array.isArray(files) && files.length
      ? <FileList files={files} />
      : <div className="tool-empty">（没有匹配文件）</div>
    if (row.error && (!Array.isArray(files) || !files.length) && rawOutput.trim()) view.errorText = rawOutput
    return view
  }

  if (kind === 'plan') {
    const todos = pick(input, ['todos'])
    const description = pick(input, ['plan_description', 'description', 'update'])
    view.meta = description ? truncateText(firstLine(description, 100), 100) : ''
    view.badge = Array.isArray(todos) ? `${todos.length} 项` : ''
    const body = []
    if (Array.isArray(todos) && todos.length) body.push(<TodoList key="todos" todos={todos} />)
    if (rawOutput.trim()) body.push(<TextPreview key="out" text={rawOutput} max={12} />)
    view.body = body.length ? body : <div className="tool-empty">（计划已更新）</div>
    return view
  }

  if (kind === 'web') {
    const url = pick(input, ['url'])
    const query = pick(input, ['query', 'prompt'])
    const isFetch = Boolean(url)
    view.target = truncateText(firstLine(url || query || '', 96), 96)
    if (isFetch) {
      const code = pick(out, ['code'])
      const codeText = pick(out, ['codeText'])
      const bytes = Number(pick(out, ['bytes']) || 0)
      view.badge = [
        code ? `${code}${codeText ? ` ${codeText}` : ''}` : '',
        bytes ? `${(bytes / 1024).toFixed(1)} KB` : '',
      ].filter(Boolean).join(' · ')
      const text = pick(out, ['result', 'content', 'text']) || rawOutput
      view.body = text.trim()
        ? <TextPreview text={text} max={30} />
        : <div className="tool-empty">（无返回内容）</div>
    } else {
      const results = pick(out, ['results'])
      const items = []
      for (const item of Array.isArray(results) ? results : []) {
        if (typeof item === 'string') {
          const linkPattern = /\[([^\]]+)\]\((https?:\/\/[^)]+)\)/g
          let matched = linkPattern.exec(item)
          let found = false
          while (matched) {
            items.push({ title: matched[1], url: matched[2] })
            found = true
            matched = linkPattern.exec(item)
          }
          if (!found && item.trim()) items.push({ text: item })
        } else if (item && typeof item === 'object') {
          items.push({
            title: pick(item, ['title', 'name']) || '',
            url: pick(item, ['url', 'link']) || '',
            text: pick(item, ['snippet', 'content', 'text']) || '',
          })
        }
      }
      view.badge = items.length ? `${items.length} 条结果` : '网页'
      view.body = items.length
        ? <LinkList items={items} />
        : <TextPreview text={rawOutput} max={24} />
    }
    return view
  }

  if (kind === 'agent') {
    const target = pick(input, ['description', 'agent', 'agent_type', 'task', 'prompt', 'query'])
    view.target = target ? truncateText(firstLine(target, 96), 96) : ''
    view.badge = kindOf(row.name) === 'agent' ? '子代理' : ''
    view.body = rawOutput.trim()
      ? <TextPreview text={rawOutput} max={30} />
      : <div className="tool-empty">（等待子代理返回）</div>
    return view
  }

  if (kind === 'skill') {
    const skill = pick(input, ['skill', 'name'])
    const description = pick(out, ['description']) || looseString(rawOutput, ['description'])
    const path = cleanPath(pick(out, ['path']) || looseString(rawOutput, ['path']) || '')
    const prompt = pick(out, ['prompt'])
    view.target = skill ? String(skill) : ''
    view.badge = '技能'
    view.meta = description ? truncateText(firstLine(description, 140), 140) : ''
    if (path) {
      view.body = (
        <>
          <ul className="tool-files"><li><PathButton path={path} /></li></ul>
          <div className="tool-empty">
            已加载技能说明{prompt ? `（${String(prompt).length} 字）` : ''}，全文见「原始数据」
          </div>
        </>
      )
    } else if (rawOutput.trim() && !/^\s*\{/.test(rawOutput.trim())) {
      view.body = <TextPreview text={rawOutput} max={24} />
    } else {
      view.body = <div className="tool-empty">（技能已加载，展开「原始数据」查看说明）</div>
    }
    return view
  }

  if (kind === 'ask') {
    const question = pick(input, ['question', 'prompt'])
    const answer = pick(out, ['answer'])
    view.target = question ? truncateText(firstLine(question, 96), 96) : ''
    view.badge = pick(out, ['status']) ? String(pick(out, ['status'])) : ''
    view.body = answer
      ? <TextPreview text={answer} max={20} />
      : <div className="tool-empty">（等待回答）</div>
    return view
  }

  // 通用兜底：工具名 + 输入首个字段，正文优先展示纯文本，其次 pretty JSON。
  const key = input && typeof input === 'object' ? Object.keys(input)[0] : ''
  if (key) {
    const value = input[key]
    view.target = `${key}=${truncateText(typeof value === 'string' ? value : JSON.stringify(value), 56)}`
  } else if (row.input) {
    view.target = firstLine(row.input, 72)
  }
  view.verb = row.name || view.verb
  const pretty = out ? JSON.stringify(out, null, 2) : rawOutput
  view.badge = rawOutput ? `${lineCount(rawOutput)} 行输出` : ''
  view.body = pretty.trim()
    ? <TextPreview text={pretty} max={30} className="tool-code" />
    : <div className="tool-empty">（无输出）</div>
  if (row.error && rawOutput.trim()) view.errorText = rawOutput
  return view
}

export function FullTextButton({ meta, onExpand, expanding }) {
  if (!meta?.ref) return null
  const busy = expanding === meta.ref
  return (
    <button
      type="button"
      className="ghost expand-full"
      onClick={() => onExpand(meta)}
      disabled={busy}
      title="从会话文件读取未截断的完整内容"
    >
      {busy ? '读取中…' : `查看全文（${meta.chars} 字）`}
    </button>
  )
}

export function ToolCard({ row, truncations, onExpand, expanding }) {
  const view = useMemo(() => describeTool(row), [row])
  const state = row.output === null ? 'running' : row.error || view.isError ? 'error' : 'done'
  const mark = state === 'running' ? '◌' : state === 'error' ? '✗' : '✓'
  const inputMeta = truncations?.get(row.input)
  const outputMeta = truncations?.get(row.output)
  return (
    <div className={`process-tool tool-card ${state} ${view.kind}`}>
      <div className="tool-card-head">
        <span className="tool-mark">{mark}</span>
        <span className="tool-icon">{view.icon}</span>
        <span className="tool-verb">{view.verb}</span>
        {view.path ? <PathButton path={view.path} /> : null}
        {!view.path && view.target ? <span className="tool-target">{view.target}</span> : null}
        {view.badge ? <span className="tool-badge">{view.badge}</span> : null}
      </div>
      {view.meta ? <div className="tool-meta">{view.meta}</div> : null}
      <div className="tool-card-body">
        {state === 'running' ? <div className="tool-empty">运行中…</div> : view.body}
      </div>
      {view.errorText ? <pre className="tool-error-text">{view.errorText}</pre> : null}
      <details className="tool-raw">
        <summary>原始数据</summary>
        {row.input ? (
          <>
            <div className="tool-raw-label">输入</div>
            <pre className="tool-input">{row.input}</pre>
            <FullTextButton meta={inputMeta} onExpand={onExpand} expanding={expanding} />
          </>
        ) : null}
        {row.output ? (
          <>
            <div className="tool-raw-label">输出</div>
            <pre>{row.output}</pre>
            <FullTextButton meta={outputMeta} onExpand={onExpand} expanding={expanding} />
          </>
        ) : null}
      </details>
    </div>
  )
}
