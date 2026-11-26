const TOOL_CATEGORIES = [
  { match: /read/i, label: '已读取文件' },
  { match: /(glob|grep|search|list)/i, label: '已搜索代码' },
  { match: /(edit|patch|replace)/i, label: '修改了文件' },
  { match: /write/i, label: '已写入文件' },
  { match: /(bash|shell|terminal|exec|run)/i, label: '执行了命令' },
  { match: /(todo|plan)/i, label: '更新了计划' },
]
const CATEGORY_ORDER = ['已读取文件', '已搜索代码', '修改了文件', '已写入文件', '执行了命令', '更新了计划', '已调用工具']

export function toolLabel(name) {
  const found = TOOL_CATEGORIES.find((item) => item.match.test(name))
  return found ? found.label : '已调用工具'
}

export function pairRows(blocks) {
  const rows = []
  for (const block of blocks) {
    if (block.kind === 'tool_use') {
      rows.push({ kind: 'tool', name: block.name, input: block.input || '', output: null, error: false })
    } else if (block.kind === 'tool_result') {
      let target = null
      for (let index = rows.length - 1; index >= 0; index -= 1) {
        if (rows[index].kind === 'tool' && rows[index].output === null) { target = rows[index]; break }
      }
      if (target) {
        target.output = block.output || ''
        target.error = Boolean(block.is_error)
      } else {
        rows.push({ kind: 'tool', name: block.name, input: '', output: block.output || '', error: Boolean(block.is_error) })
      }
    } else {
      rows.push({ kind: block.kind, text: block.text || '' })
    }
  }
  return rows
}

export function groupTurns(messages) {
  const turns = []
  for (const message of messages) {
    const isUser = message.role === 'user'
    if (isUser || turns.length === 0) {
      turns.push({ user: isUser ? message : null, blocks: [], ts: message.ts, end: message.ts })
    }
    const turn = turns[turns.length - 1]
    if (!isUser) {
      for (const block of message.blocks || []) turn.blocks.push({ ...block, role: message.role })
    }
    if (message.ts) turn.end = message.ts
  }
  for (const turn of turns) {
    let answerIndex = -1
    for (let index = turn.blocks.length - 1; index >= 0; index -= 1) {
      const block = turn.blocks[index]
      if (block.kind === 'text' && block.role === 'assistant') { answerIndex = index; break }
    }
    let lastTool = -1
    turn.blocks.forEach((block, index) => { if (block.kind === 'tool_result') lastTool = index })
    if (answerIndex !== -1 && lastTool > answerIndex) answerIndex = -1
    if (answerIndex === -1 && lastTool === -1) {
      for (let index = turn.blocks.length - 1; index >= 0; index -= 1) {
        if (turn.blocks[index].kind === 'text' && turn.blocks[index].role === 'assistant') { answerIndex = index; break }
      }
    }
    turn.answer = answerIndex === -1 ? null : turn.blocks[answerIndex].text
    turn.rows = pairRows(turn.blocks.filter((_, index) => index !== answerIndex))
  }
  return turns
}

export function formatDuration(ms) {
  const seconds = Math.max(1, Math.round(ms / 1000))
  if (seconds < 60) return `${seconds} 秒`
  return `${Math.floor(seconds / 60)} 分 ${seconds % 60} 秒`
}

export function processTitle(rows, live, durationMs) {
  if (live) return rows.some((row) => row.kind === 'tool') ? '正在调用工具' : '正在分析请求'
  const labels = []
  for (const row of rows) {
    const label = row.kind === 'tool' ? toolLabel(row.name) : row.kind === 'thinking' ? '已完成分析' : null
    if (label && !labels.includes(label)) labels.push(label)
  }
  labels.sort((a, b) => CATEGORY_ORDER.indexOf(a) - CATEGORY_ORDER.indexOf(b))
  const summary = labels.length ? labels.slice(0, 3).join('，') : '已完成'
  return durationMs ? `${summary}，用时 ${formatDuration(durationMs)}` : summary
}
