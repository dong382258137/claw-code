import React, { useMemo, useRef } from 'react'

const escapeHtml = (text) =>
  String(text).replace(/[&<>"']/g, (ch) => ({
    '&': '&amp;',
    '<': '&lt;',
    '>': '&gt;',
    '"': '&quot;',
    "'": '&#39;',
  })[ch])

function renderInline(line) {
  let text = escapeHtml(line)
  text = text.replace(/`([^`]+)`/g, '<code>$1</code>')
  text = text.replace(/\*\*([^*]+)\*\*/g, '<b>$1</b>')
  text = text.replace(/~~([^~]+)~~/g, '<s>$1</s>')
  text = text.replace(/\[([^\]]+)\]\((https?:\/\/[^)\s]+)\)/g, '<a href="$2" target="_blank" rel="noreferrer">$1</a>')
  return text
}

const TABLE_SEPARATOR = /^\s*\|?[\s:|-]{3,}\|?\s*$/

function isTableRow(line) {
  return /^\s*\|.*\|\s*$/.test(line)
}

function cells(line) {
  return line.trim().replace(/^\|/, '').replace(/\|$/, '').split('|').map((cell) => cell.trim())
}

function renderTable(rows) {
  const [head, ...body] = rows
  const header = `<thead><tr>${cells(head).map((cell) => `<th>${renderInline(cell)}</th>`).join('')}</tr></thead>`
  const content = body
    .map((row) => `<tr>${cells(row).map((cell) => `<td>${renderInline(cell)}</td>`).join('')}</tr>`)
    .join('')
  return `<table>${header}<tbody>${content}</tbody></table>`
}

function renderBlocks(text) {
  const lines = text.split('\n')
  const out = []
  let index = 0
  let list = []

  const flushList = () => {
    if (list.length) {
      out.push(`<ul>${list.map((item) => `<li>${item}</li>`).join('')}</ul>`)
      list = []
    }
  }

  while (index < lines.length) {
    const line = lines[index]

    // 围栏代码块
    if (/^\s*```/.test(line)) {
      flushList()
      const language = line.replace(/^\s*```/, '').trim()
      const body = []
      index += 1
      while (index < lines.length && !/^\s*```/.test(lines[index])) {
        body.push(lines[index])
        index += 1
      }
      index += 1
      out.push(
        `<div class="code-block"><div class="code-head"><span>${escapeHtml(language || 'code')}</span>` +
        `<button class="code-copy" type="button">复制</button></div>` +
        `<pre><code>${escapeHtml(body.join('\n'))}</code></pre></div>`,
      )
      continue
    }

    // 表格：当前行是 |...| 且下一行是分隔行
    if (isTableRow(line) && index + 1 < lines.length && TABLE_SEPARATOR.test(lines[index + 1])) {
      flushList()
      const rows = [line]
      index += 2
      while (index < lines.length && isTableRow(lines[index])) {
        rows.push(lines[index])
        index += 1
      }
      out.push(renderTable(rows))
      continue
    }

    if (/^\s*[-*]\s+/.test(line)) {
      list.push(renderInline(line.replace(/^\s*[-*]\s+/, '')))
      index += 1
      continue
    }

    flushList()
    if (/^###\s+/.test(line)) out.push(`<h3>${renderInline(line.replace(/^###\s+/, ''))}</h3>`)
    else if (/^##\s+/.test(line)) out.push(`<h2>${renderInline(line.replace(/^##\s+/, ''))}</h2>`)
    else if (/^#\s+/.test(line)) out.push(`<h1>${renderInline(line.replace(/^#\s+/, ''))}</h1>`)
    else if (/^\s*>\s?/.test(line)) out.push(`<blockquote>${renderInline(line.replace(/^\s*>\s?/, ''))}</blockquote>`)
    else if (line.trim() === '') out.push('\n')
    else out.push(renderInline(line))
    index += 1
  }
  flushList()
  return out.join('\n')
}

export function Markdown({ text }) {
  const ref = useRef(null)
  const html = useMemo(() => renderBlocks(String(text || '')), [text])

  const onClick = async (event) => {
    const button = event.target.closest('.code-copy')
    if (!button) return
    const pre = button.closest('.code-block')?.querySelector('pre')
    try {
      await navigator.clipboard.writeText(pre?.textContent || '')
      button.textContent = '已复制'
      setTimeout(() => { button.textContent = '复制' }, 1200)
    } catch (_) {}
  }

  return <div className="md" ref={ref} onClick={onClick} dangerouslySetInnerHTML={{ __html: html }} />
}
