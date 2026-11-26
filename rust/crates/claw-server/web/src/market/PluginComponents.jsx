import React from 'react'

// 如实渲染后端返回的组件清单：技能数、Agent/命令/Hook、MCP 服务（含传输类型）、
// 以及本次暂不支持的组件。确认安装与已安装详情共用同一份展示。
export function PluginComponents({ components, title = '组件清单' }) {
  if (!components) return null
  const skills = components.skill_paths || []
  const servers = components.mcp_servers || []
  const unsupported = components.unsupported || []
  return (
    <div className="plugin-components">
      <h4>{title}</h4>
      <div className="plugin-components-row">
        <span>技能</span>
        <b>{components.skill_count || 0} 个</b>
      </div>
      {skills.length ? (
        <ul className="plugin-components-list">
          {skills.map((path) => (
            <li key={path}>{path}</li>
          ))}
        </ul>
      ) : null}
      <div className="plugin-components-row">
        <span>Agent</span>
        <b>{components.agent_count || 0} 个</b>
      </div>
      <div className="plugin-components-row">
        <span>命令</span>
        <b>{components.command_count || 0} 个</b>
      </div>
      <div className="plugin-components-row">
        <span>Hook</span>
        <b>{components.hook_count || 0} 个</b>
      </div>
      <div className="plugin-components-row">
        <span>MCP 服务</span>
        <b>{components.mcp_server_count || 0} 个</b>
      </div>
      {servers.map((server) => (
        <div key={server.name} className="plugin-components-row plugin-mcp">
          <span>{server.name}</span>
          <b>{server.transport}</b>
        </div>
      ))}
      <div className={unsupported.length ? 'plugin-components-row plugin-unsupported' : 'plugin-components-row'}>
        <span>不支持的组件</span>
        <b>{unsupported.length ? `${unsupported.length} 项` : '无'}</b>
      </div>
      {unsupported.length ? (
        <ul className="plugin-components-list">
          {unsupported.map((item) => (
            <li key={item}>{item}</li>
          ))}
        </ul>
      ) : null}
    </div>
  )
}
