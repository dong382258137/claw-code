import React, { useCallback, useEffect, useRef, useState } from 'react'
import { api, postJson, withToken, withWorkspace } from '../api'

const EMPTY_TASKS = { todos: [], tasks: [], crons: [], plan_mode: null }
const EMPTY_PLAN = { enabled: false, approved: false, revision: 0, source: 'none', plan: null }

const STATUS_OPTIONS = [
  ['pending', '待办'],
  ['executing', '进行中'],
  ['done', '完成'],
  ['failed', '失败'],
  ['skipped', '跳过'],
]

const statusLabel = (status) => STATUS_OPTIONS.find(([value]) => value === status)?.[1] || status

const stepsToDraft = (plan) =>
  (plan && Array.isArray(plan.steps) ? plan.steps : []).map((step) => ({
    id: step.id || '',
    description: step.description || '',
    acceptance_criteria: step.acceptance_criteria || '',
    status: step.status || 'pending',
  }))

const putJson = (path, body) =>
  api(path, {
    method: 'PUT',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(body),
  })

export function TasksPane({ currentId, workspace, onError }) {
  const [data, setData] = useState(EMPTY_TASKS)
  const [goal, setGoal] = useState(null)
  const [plans, setPlans] = useState([])
  const [webTodos, setWebTodos] = useState([])
  const [planState, setPlanState] = useState(EMPTY_PLAN)
  const [draft, setDraft] = useState({ task_summary: '', steps: [] })
  const [dirty, setDirty] = useState(false)
  const [newTodo, setNewTodo] = useState('')
  const [saving, setSaving] = useState(false)
  const [connected, setConnected] = useState(false)
  const [notice, setNotice] = useState('')

  // 版本号用于丢弃乱序/重复事件：SSE 事件与 GET 快照都带 revision，
  // 只有更大的才覆盖，重连后旧快照不会把新状态冲掉。
  const todoRev = useRef(-1)
  const planRev = useRef(-1)
  // 用户正在编辑时不让后台事件覆盖草稿。
  const dirtyRef = useRef(false)
  const openedOnce = useRef(false)

  const markDirty = useCallback((value) => {
    dirtyRef.current = value
    setDirty(value)
  }, [])

  const applyPlan = useCallback((next) => {
    if (!next) return
    const revision = Number(next.revision || 0)
    if (revision < planRev.current) return
    planRev.current = revision
    setPlanState(next)
    if (!dirtyRef.current) {
      setDraft({
        task_summary: (next.plan && next.plan.task_summary) || '',
        steps: stepsToDraft(next.plan),
      })
    }
  }, [])

  const sessionPath = useCallback(
    (suffix) => `/api/session/${encodeURIComponent(currentId)}${suffix}`,
    [currentId],
  )

  const loadAll = useCallback(() => {
    if (!currentId) return
    Promise.all([
      api(sessionPath('/tasks')).catch(() => EMPTY_TASKS),
      api(withWorkspace('/api/workspace/goal', workspace)).catch(() => ({ goal: null })),
      api(withWorkspace('/api/workspace/plans', workspace)).catch(() => ({ plans: [] })),
      api(sessionPath('/todos')).catch(() => ({ items: [], revision: 0 })),
      api(sessionPath('/plan')).catch(() => null),
    ]).then(([tasks, goalBody, planBody, todoBody, planBody2]) => {
      setData(tasks)
      setGoal(goalBody.goal)
      setPlans(planBody.plans || [])
      const revision = Number(todoBody.revision || 0)
      if (revision >= todoRev.current) {
        todoRev.current = revision
        setWebTodos(todoBody.items || [])
      }
      if (planBody2) applyPlan(planBody2)
    }).catch((error) => onError?.(`任务加载失败：${error.message}`))
  }, [applyPlan, currentId, onError, sessionPath, workspace])

  // 切换会话：先清空所有派生状态，再订阅 + 拉一次快照，
  // 避免上一个会话的待办/计划短暂串台或与事件叠加成重复条目。
  useEffect(() => {
    if (!currentId) return undefined
    todoRev.current = -1
    planRev.current = -1
    openedOnce.current = false
    dirtyRef.current = false
    setData(EMPTY_TASKS)
    setWebTodos([])
    setPlans([])
    setGoal(null)
    setPlanState(EMPTY_PLAN)
    setDraft({ task_summary: '', steps: [] })
    setDirty(false)
    setNotice('')
    setSaving(false)
    setConnected(false)

    const source = new EventSource(withToken(sessionPath('/events')))
    const onStream = (event) => {
      let payload
      try {
        payload = JSON.parse(event.data)
      } catch (_) {
        return
      }
      switch (payload.kind) {
        case 'todos':
          if (payload.scope === 'agent') {
            setData((prev) => ({ ...prev, todos: payload.items || [] }))
          } else {
            const revision = Number(payload.revision || 0)
            if (revision >= todoRev.current) {
              todoRev.current = revision
              setWebTodos(payload.items || [])
            }
          }
          break
        case 'plan':
          applyPlan(payload.state)
          break
        case 'plan_mode':
          setData((prev) => ({ ...prev, plan_mode: Boolean(payload.enabled) }))
          // sidecar 是 Web 端权威状态；没有 sidecar 时才跟随 agent 的工具调用。
          setPlanState((prev) => (prev.source === 'web' ? prev : { ...prev, enabled: Boolean(payload.enabled) }))
          break
        case 'tasks':
          if (payload.tasks) setData(payload.tasks)
          break
        case 'plans':
          setPlans(payload.plans || [])
          break
        default:
          break
      }
    }
    source.addEventListener('stream', onStream)
    source.onopen = () => {
      setConnected(true)
      // 断线重连后事件可能已丢，用一次快照对齐（不是轮询，只在重连时发生）。
      if (openedOnce.current) loadAll()
      openedOnce.current = true
    }
    source.onerror = () => setConnected(false)
    loadAll()
    return () => {
      source.removeEventListener('stream', onStream)
      source.close()
      setConnected(false)
    }
  }, [applyPlan, currentId, loadAll, sessionPath])

  const saveTodos = useCallback(async (items) => {
    setWebTodos(items)
    try {
      const body = await postJson(sessionPath('/todos'), { items })
      const revision = Number(body.revision || 0)
      if (revision >= todoRev.current) todoRev.current = revision
    } catch (error) {
      onError?.(`待办保存失败：${error.message}`)
    }
  }, [onError, sessionPath])

  const editStep = (index, patch) => {
    markDirty(true)
    setDraft((prev) => ({
      ...prev,
      steps: prev.steps.map((step, i) => (i === index ? { ...step, ...patch } : step)),
    }))
  }

  const addStep = () => {
    markDirty(true)
    setDraft((prev) => ({
      ...prev,
      steps: [...prev.steps, { id: '', description: '', acceptance_criteria: '', status: 'pending' }],
    }))
  }

  const removeStep = (index) => {
    markDirty(true)
    setDraft((prev) => ({ ...prev, steps: prev.steps.filter((_, i) => i !== index) }))
  }

  const planBody = (extra = {}) => ({
    plan_id: (planState.plan && planState.plan.id) || null,
    task_summary: draft.task_summary,
    steps: draft.steps.map((step) => ({
      id: step.id || null,
      description: step.description,
      acceptance_criteria: step.acceptance_criteria,
      status: step.status,
    })),
    ...extra,
  })

  const savePlan = async (extra = {}) => {
    if (!currentId || saving) return null
    setSaving(true)
    try {
      const next = await putJson(sessionPath('/plan'), planBody(extra))
      markDirty(false)
      applyPlan(next)
      setNotice('计划已保存')
      return next
    } catch (error) {
      onError?.(`计划保存失败：${error.message}`)
      return null
    } finally {
      setSaving(false)
    }
  }

  const togglePlanMode = async () => {
    if (!currentId || saving) return
    setSaving(true)
    try {
      const next = await putJson(sessionPath('/plan'), { enabled: !planState.enabled })
      applyPlan(next)
    } catch (error) {
      onError?.(`计划模式切换失败：${error.message}`)
    } finally {
      setSaving(false)
    }
  }

  const confirmPlan = async () => {
    if (!currentId || saving) return
    setSaving(true)
    try {
      // confirm 会把 approved 置真、计划模式置假，并按正常 prompt 通道入队一轮。
      const next = await postJson(sessionPath('/plan/confirm'), planBody())
      markDirty(false)
      if (next.plan) applyPlan(next.plan)
      setNotice('计划已确认，已提交给 agent 继续执行')
    } catch (error) {
      onError?.(`计划确认失败：${error.message}`)
    } finally {
      setSaving(false)
    }
  }

  if (!currentId) return <div className="empty">先选择会话</div>
  const goalHtml = goal && (goal.text || goal.state) && (
    <div className="goal">
      <b>目标</b> {goal.text}
      <div className="muted">状态 {goal.state || '—'} · tokens {goal.tokens_used || 0}{goal.token_budget ? ` / ${goal.token_budget}` : ''} · 阻塞 {goal.blocked_count || 0} 次</div>
    </div>
  )
  const plan = planState.plan
  return (
    <div className="pane">
      <div className="pane-toolbar">
        <button onClick={loadAll}>刷新</button>
        <button className={planState.enabled ? 'active' : ''} disabled={saving} onClick={togglePlanMode}>
          {planState.enabled ? '计划模式：开' : '计划模式：关'}
        </button>
        {planState.approved && <span className="chip">已确认</span>}
        <span className={`task-conn ${connected ? 'on' : ''}`} title={connected ? '事件流已连接' : '事件流断开，正在重连'} />
      </div>
      <div className="pane-body scroll">
        {goalHtml}
        <div className="group">计划（可编辑 · 保存后注入后续轮次）</div>
        <div className="plan-edit">
          {plan ? (
            <>
              <input
                className="plan-summary"
                value={draft.task_summary}
                placeholder="任务摘要"
                onChange={(event) => {
                  markDirty(true)
                  setDraft((prev) => ({ ...prev, task_summary: event.target.value }))
                }}
              />
              {draft.steps.map((step, index) => (
                <div className="plan-step" key={step.id || `new-${index}`}>
                  <span className="plan-step-no">{index + 1}</span>
                  <div className="plan-step-fields">
                    <input
                      value={step.description}
                      placeholder="步骤描述"
                      onChange={(event) => editStep(index, { description: event.target.value })}
                    />
                    <input
                      className="plan-step-acceptance"
                      value={step.acceptance_criteria}
                      placeholder="验收标准（可选）"
                      onChange={(event) => editStep(index, { acceptance_criteria: event.target.value })}
                    />
                  </div>
                  <select value={step.status} onChange={(event) => editStep(index, { status: event.target.value })}>
                    {STATUS_OPTIONS.map(([value, label]) => <option key={value} value={value}>{label}</option>)}
                  </select>
                  <button className="ghost" title="删除该步骤" onClick={() => removeStep(index)}>×</button>
                </div>
              ))}
              <div className="plan-actions">
                <button onClick={addStep}>添加步骤</button>
                <button disabled={saving || !dirty} onClick={() => savePlan()}>保存计划</button>
                <button
                  className="primary"
                  disabled={saving || draft.steps.filter((step) => step.description.trim()).length === 0}
                  onClick={confirmPlan}
                >
                  确认并继续
                </button>
                {dirty && <span className="muted">有未保存的修改</span>}
                {notice && <span className="muted">{notice}</span>}
              </div>
            </>
          ) : (
            <div className="plan-actions">
              <button
                disabled={saving}
                onClick={() => savePlan({ steps: [{ description: '第一步', acceptance_criteria: '', status: 'pending' }] })}
              >
                新建计划
              </button>
              <span className="muted">还没有计划；也可以让 agent 处于计划模式后先生成</span>
            </div>
          )}
        </div>

        {plans.length > 0 && <div className="group">计划文件（只读）</div>}
        {plans.map((item) => {
          const steps = item.plan && Array.isArray(item.plan.steps) ? item.plan.steps : []
          const done = steps.filter((step) => ['completed', 'done', 'Succeeded'].includes(step.status)).length
          const status = (item.plan && item.plan.status) || ''
          return (
            <details key={item.id} className="plan">
              <summary>
                <span>▤ {(item.plan && item.plan.task_summary) || item.plan?.title || item.id}</span>
                <span className="muted">{status ? `${status} · ` : ''}{steps.length ? `${done}/${steps.length} 步` : ''}</span>
              </summary>
              {steps.length > 0
                ? steps.map((step, index) => {
                    const stepStatus = step.status || ''
                    const finished = ['completed', 'done', 'Succeeded'].includes(stepStatus)
                    const running = ['in_progress', 'running', 'Executing'].includes(stepStatus)
                    return (
                      <div key={index} className={`row todo ${finished ? 'done' : ''}`}>
                        <span>{finished ? '✔' : running ? '▶' : '○'} {step.description || step.title || step.id || `第 ${index + 1} 步`}</span>
                        <span className="muted">{statusLabel(stepStatus)}</span>
                      </div>
                    )
                  })
                : <div className="muted" style={{ padding: '4px 8px' }}>该计划没有步骤明细</div>}
            </details>
          )
        })}

        <div className="group">当前待办（可编辑 · 下一条消息会带给 agent）</div>
        {webTodos.map((todo, index) => (
          <div key={`${index}-${todo.text}`} className={`row todo ${todo.done ? 'done' : ''}`}>
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

        {data.todos.length > 0 && <div className="group">历史 TodoWrite（实时）</div>}
        {data.todos.map((todo, index) => (
          <div key={`${index}-${todo.content || ''}`} className={`row todo ${['completed', 'done'].includes(todo.status) ? 'done' : ''}`}>
            <span>{['completed', 'done'].includes(todo.status) ? '✔' : todo.status === 'in_progress' ? '▶' : '○'} {todo.content}</span>
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
