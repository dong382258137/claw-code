import React, { useCallback, useEffect, useState } from 'react'
import { api, postJson, withWorkspace } from '../api'

export function TasksPane({ currentId, workspace, onError }) {
  const [data, setData] = useState({ todos: [], tasks: [], crons: [], plan_mode: null })
  const [goal, setGoal] = useState(null)
  const [plans, setPlans] = useState([])
  const [webTodos, setWebTodos] = useState([])
  const [newTodo, setNewTodo] = useState('')

  const load = useCallback(() => {
    if (!currentId) return
    Promise.all([
      api(`/api/session/${encodeURIComponent(currentId)}/tasks`).catch(() => ({ todos: [], tasks: [], crons: [] })),
      api(withWorkspace('/api/workspace/goal', workspace)).catch(() => ({ goal: null })),
      api(withWorkspace('/api/workspace/plans', workspace)).catch(() => ({ plans: [] })),
      api(`/api/session/${encodeURIComponent(currentId)}/todos`).catch(() => ({ items: [] })),
    ]).then(([tasks, goalBody, planBody, todoBody]) => {
      setData(tasks)
      setGoal(goalBody.goal)
      setPlans(planBody.plans || [])
      setWebTodos(todoBody.items || [])
    }).catch((error) => onError?.(`任务加载失败：${error.message}`))
  }, [currentId, onError, workspace])
  useEffect(() => { load() }, [load])

  const saveTodos = useCallback(async (items) => {
    setWebTodos(items)
    try {
      await postJson(`/api/session/${encodeURIComponent(currentId)}/todos`, { items })
    } catch (error) {
      onError?.(`待办保存失败：${error.message}`)
    }
  }, [currentId, onError])

  if (!currentId) return <div className="empty">先选择会话</div>
  const goalHtml = goal && (goal.text || goal.state) && (
    <div className="goal">
      <b>目标</b> {goal.text}
      <div className="muted">状态 {goal.state || '—'} · tokens {goal.tokens_used || 0}{goal.token_budget ? ` / ${goal.token_budget}` : ''} · 阻塞 {goal.blocked_count || 0} 次</div>
    </div>
  )
  return (
    <div className="pane">
      <div className="pane-toolbar"><button onClick={load}>刷新</button>{data.plan_mode && <span className="chip">计划模式</span>}</div>
      <div className="pane-body scroll">
        {goalHtml}
        {plans.length > 0 && <div className="group">计划</div>}
        {plans.map((plan) => {
          const steps = plan.plan && Array.isArray(plan.plan.steps) ? plan.plan.steps : []
          const done = steps.filter((step) => step.status === 'completed' || step.status === 'done').length
          const status = plan.plan?.status || ''
          return (
            <details key={plan.id} className="plan">
              <summary>
                <span>▤ {plan.plan?.title || plan.id}</span>
                <span className="muted">{status ? `${status} · ` : ''}{steps.length ? `${done}/${steps.length} 步` : ''}</span>
              </summary>
              {steps.length > 0
                ? steps.map((step, index) => {
                    const stepStatus = step.status || ''
                    const finished = stepStatus === 'completed' || stepStatus === 'done'
                    const running = stepStatus === 'in_progress' || stepStatus === 'running'
                    return (
                      <div key={index} className={`row todo ${finished ? 'done' : ''}`}>
                        <span>{finished ? '✔' : running ? '▶' : '○'} {step.description || step.title || step.id || `第 ${index + 1} 步`}</span>
                        <span className="muted">{stepStatus}</span>
                      </div>
                    )
                  })
                : <div className="muted" style={{ padding: '4px 8px' }}>该计划没有步骤明细</div>}
            </details>
          )
        })}
        <div className="group">当前待办（可编辑 · 下一条消息会带给 agent）</div>
        {webTodos.map((todo, index) => (
          <div key={index} className={`row todo ${todo.done ? 'done' : ''}`}>
            <label className="todo-check">
              <input
                type="checkbox"
                checked={Boolean(todo.done)}
                onChange={() => saveTodos(webTodos.map((item, i) => (i === index ? { ...item, done: !item.done } : item)))}
              />
              <span>{todo.text}</span>
            </label>
            <button className="ghost" title="删除" onClick={() => saveTodos(webTodos.filter((_, i) => i !== index))}>×</button>
          </div>
        ))}
        {webTodos.length === 0 && <div className="muted" style={{ padding: '2px 8px 6px' }}>还没有待办，添加一条试试</div>}
        <div className="todo-add">
          <input
            value={newTodo}
            placeholder="添加待办，Enter 保存"
            onChange={(event) => setNewTodo(event.target.value)}
            onKeyDown={(event) => {
              if (event.key !== 'Enter') return
              event.preventDefault()
              const text = newTodo.trim()
              if (!text) return
              saveTodos([...webTodos, { text, done: false }])
              setNewTodo('')
            }}
          />
          <button onClick={() => {
            const text = newTodo.trim()
            if (!text) return
            saveTodos([...webTodos, { text, done: false }])
            setNewTodo('')
          }}>添加</button>
        </div>

        {data.todos.length > 0 && <div className="group">历史 TodoWrite（只读）</div>}
        {data.todos.map((todo, index) => (
          <div key={index} className={`row todo ${todo.status === 'completed' ? 'done' : ''}`}>
            <span>{todo.status === 'completed' ? '✔' : todo.status === 'in_progress' ? '▶' : '○'} {todo.content}</span>
          </div>
        ))}
        {data.tasks.length > 0 && <div className="group">任务工具</div>}
        {data.tasks.map((task, index) => <div key={index} className="row"><span>◆ {task.tool}</span><span className="muted">{task.detail}</span></div>)}
        {data.crons.length > 0 && <div className="group">定时记录</div>}
        {data.crons.map((cron, index) => <div key={index} className="row"><span>⏱ {cron.tool}</span><span className="muted">{cron.detail}</span></div>)}
      </div>
    </div>
  )
}
