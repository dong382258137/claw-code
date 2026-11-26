import React, { useState } from 'react'
import { postJson } from '../api.js'
import { PluginComponents } from './PluginComponents.jsx'
import { errorText, REASON_LABELS } from './pluginUtils.js'

// 安装确认：先展示来源与完整性锚点，untrusted 来源必须再勾选一次风险确认；
// 安装成功后在同一弹窗内展示后端返回的真实组件清单（技能/MCP/不支持项）。
export function PluginInstallDialog({ source, entry, onClose, onInstalled }) {
  const [accepted, setAccepted] = useState(Boolean(source.trusted))
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState(null)
  const [report, setReport] = useState(null)

  const install = async () => {
    setBusy(true)
    setError(null)
    try {
      const result = await postJson('/api/plugins/install', {
        source_id: source.id,
        name: entry.name,
        confirm_untrusted: !source.trusted,
      })
      setReport(result)
      onInstalled?.(result)
    } catch (err) {
      setError(err)
    } finally {
      setBusy(false)
    }
  }

  return (
    <div className="plugin-mask">
      <div className="plugin-modal" role="dialog" aria-modal="true">
        <h3>{report ? '安装完成' : `安装 ${entry.name}`}</h3>
        <p className="plugin-modal-sub">
          来源 {source.name || source.id} · {source.trusted ? '受信任' : '未受信任'} ·{' '}
          {entry.source?.kind || '未知形态'}
        </p>

        {!report ? (
          <>
            <dl className="skill-meta">
              <dt>版本</dt>
              <dd>{entry.version || '未声明'}</dd>
              <dt>描述</dt>
              <dd>{entry.description || '（无描述）'}</dd>
              <dt>完整性</dt>
              <dd className="skill-path">
                {entry.source?.sha
                  ? `sha ${entry.source.sha}`
                  : entry.source?.sha256
                    ? `sha256 ${entry.source.sha256}`
                    : '无（接口层已拒绝安装）'}
              </dd>
              {entry.declared_skills?.length ? (
                <>
                  <dt>声明技能</dt>
                  <dd>
                    {entry.declared_skills.length} 个（{entry.declared_skills.join('、')}）
                  </dd>
                </>
              ) : null}
            </dl>

            {!entry.installable ? (
              <div className="plugin-warn-box">
                该条目不可安装（<code>{entry.reason || 'not_installable'}</code>）：
                {entry.reason_detail || REASON_LABELS[entry.reason] || '未通过校验'}
              </div>
            ) : (
              <div className="skill-notice">
                安装后会按实际扫描结果导入组件；技能数、MCP 服务及传输类型、暂不支持的组件会在完成后如实列出。
              </div>
            )}

            {!source.trusted ? (
              <div className="plugin-warn-box">
                该来源未受信任：插件可能携带未经验证的技能说明、命令与 MCP 服务配置，安装后会被直接接入当前环境。
                <label className="plugin-check">
                  <input
                    type="checkbox"
                    checked={accepted}
                    onChange={(event) => setAccepted(event.target.checked)}
                  />
                  <span>我已了解风险，确认从该来源安装</span>
                </label>
              </div>
            ) : null}

            {error ? (
              <div className="skill-error">
                <span>{errorText(error)}</span>
              </div>
            ) : null}

            <div className="plugin-modal-actions">
              <button type="button" onClick={onClose} disabled={busy}>
                取消
              </button>
              <button
                type="button"
                className="primary"
                onClick={install}
                disabled={busy || !accepted || !entry.installable}
              >
                {busy ? '安装中…' : '确认安装'}
              </button>
            </div>
          </>
        ) : (
          <>
            <dl className="skill-meta">
              <dt>插件</dt>
              <dd>
                {report.name} <span className="plugin-error-code">{report.plugin_id}</span>
              </dd>
              <dt>版本</dt>
              <dd>v{report.version}</dd>
              <dt>安装路径</dt>
              <dd className="skill-path">{report.path}</dd>
              <dt>状态</dt>
              <dd>{report.enabled ? '已启用' : '未启用'}</dd>
            </dl>
            <PluginComponents components={report.components} title="后端返回的组件清单" />
            <div className="plugin-modal-actions">
              <button type="button" className="primary" onClick={onClose}>
                完成
              </button>
            </div>
          </>
        )}
      </div>
    </div>
  )
}
