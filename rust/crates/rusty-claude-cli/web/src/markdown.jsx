import React, { useMemo } from 'react'

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
  text = text.replace(/\[([^\]]+)\]\((https?:\/\/[^)\s]+)\)/g, '<a href="$2" target="_blank" rel="noreferrer">$1</a>')
  return text
}

function renderLines(lines) {
  const out = []
  let list = []
  const flush = () => {
    if (list.length) {
      out.push(`<ul>${list.map((item) => `<li>${item}</li>`).join('')}</ul>`)
      list = []
    }
  }
  for (const line of lines) {
    if (/^\s*[-*]\s+/.test(line)) {
      list.push(renderInline(line.replace(/^\s*[-*]\s+/, '')))
      continue
    }
    flush()
    if (/^###\s+/.test(line)) out.push(`<h3>${renderInline(line.replace(/^###\s+/, ''))}</h3>`)
    else if (/^##\s+/.test(line)) out.push(`<h2>${renderInline(line.replace(/^##\s+/, ''))}</h2>`)
    else if (/^#\s+/.test(line)) out.push(`<h1>${renderInline(line.replace(/^#\s+/, ''))}</h1>`)
    else out.push(renderInline(line))
  }
  flush()
  return out.join('\n')
}

export function Markdown({ text }) {
  const html = useMemo(() => {
    const pieces = String(text || '').split(/```/)
    return pieces
      .map((piece, index) => {
        if (index % 2 === 1) {
          const body = piece.replace(/^([a-zA-Z0-9_+-]*)\n/, '')
          return `<pre><code>${escapeHtml(body)}</code></pre>`
        }
        return renderLines(piece.split('\n'))
      })
      .join('')
  }, [text])
  return <div className="md" dangerouslySetInnerHTML={{ __html: html }} />
}
