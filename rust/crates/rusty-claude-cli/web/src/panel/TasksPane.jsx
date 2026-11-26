import React, { useCallback, useEffect, useRef, useState } from 'react'
import { api, postJson, withWorkspace } from '../api'
import { fmtTime } from '../shared/utils.js'

const EMPTY_TASKS = { todos: [], tasks: [], crons: [], plan_mode: null, active_plan_id: null }
const EMPTY_PLAN = { enabled: false, approved: false, revision: 0, source: 'none', plan: null }

// 计划状态统一成后端 normalize 后的五种值：历史计划文件里还留着
// `Succeeded`/`completed`/`in_progress` 等旧写法，前端读的时候再兜一次底。
const normalizeStatus = (raw) => {
  const value = String(raw || '').toLowerCase()
  if (value === 'executing' || value === 'in_progress' || value === 'running') return 'executing'
  if (value === 'succeeded' || value === 'completed' || value === 'done') return 'done'
  if (value === 'failed') return 'failed'
  if (value === 'skipped') return 'skipped'
  return 'pending'
}

const STATUS_LABEL = {
  pending: '待办',
  executing: '进行中',
  done: '完成',
  failed: '失败',
  skipped: '跳过',
}

const readSteps = (plan) =>
  (plan && Array.isArray(plan.steps) ? plan.steps : []).map((step, index) => ({
    id: step.id || `s${index + 1}`,
    index: index + 1,
    description: String(step.description || step.title || '').trim() || `第 ${index + 1} 步`,
    acceptance: String(step.acceptance_criteria || '').trim(),
    status: normalizeStatus(step.status),
  }))

// 进度统计：完成率按全部步骤算，跳过计入「已处理」但不虚报完成率。
const summarize = (steps) => {
  const total = steps.length
  const done = steps.filter((step) => step.status === 'done').length
  const failed = steps.filter((step) => step.status === 'failed').length
  const skipped = steps.filter((step) => step.status === 'skipped').length
  const executing = steps.findIndex((step) => step.status === 'executing')
  const pct = total ? Math.round((done / total) * 100) : 0
  const tone = failed > 0
    ? 'failed'
    : executing >= 0
      ? 'running'
      : total > 0 && done + skipped === total
        ? 'done'
        : 'idle'
  return { total, done, failed, skipped, executing, pct, tone }
}

const TONE_LABEL = { running: '执行中', done: '已完成', failed: '有失败', idle: '待执行' }

const GOAL_STATE_LABEL = {
  active: '进行中',
  paused: '已暂停',
  blocked: '受阻',
  done: '已达成',
  completed: '已达成',
  failed: '未达成',
}

const goalTone = (state) => {
  if (state === 'done' || state === 'completed') return 'done'
  if (state === 'blocked' || state === 'failed') return 'failed'
  if (state === 'active') return 'running'
  return 'idle'
}

const putJson = (path, body) =>
  api(path, {
    method: 'PUT',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(body),
  })

// 空态里的小图标：三条计划线 + 一点「待生成」的星芒。
function PlanGlyph() {
  return (
    <svg width="22" height="22" viewBox="0 0 24 24" fill="none" aria-hidden="true">
      <path d="M4 6h10M4 12h7M4 18h9" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" />
      <path d="M18 10.5v5M15.5 13h5" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" />
    </svg>
  )
}

export function TasksPane({ currentId, workspace, taskEvent, connected, onError, onToast }) {
  const [data, setData] = useState(EMPTY_TASKS)
  const [goal, setGoal] = useState(null)
  const [plans, setPlans] = useState([])
  // 当前计划 id：由 /tasks 快照与 tasks 事件维护，用于在只读列表里标出
  // 面板正在展示的那份计划（计划事件本身按会话推送，不按 id 过滤）。
  const [activePlanId, setActivePlanId] = useState(null)
  const [webTodos, setWebTodos] = useState([])
  const [planState, setPlanState] = useState(EMPTY_PLAN)
  const [newTodo, setNewTodo] = useState('')
  const [busy, setBusy] = useState(false)
  // 定时任务（Phase 4 骨架）：存储在工作区 .claw/crons.json，执行器 Phase 6 接入。
  const [scheduledCrons, setScheduledCrons] = useState([])
  const [cronSchedule, setCronSchedule] = useState('')
  const [cronPrompt, setCronPrompt] = useState('')
  const [cronBusy, setCronBusy] = useState(false)

  // 版本号用于丢弃乱序/重复事件：SSE 事件与 GET 快照都带 revision，
  // 只有更大的才覆盖，重连后旧快照不会把新状态冲掉。
  const todoRev = useRef(-1)
  const planRev = useRef(-1)
  // 断线重连（connected 由 App 的唯一事件流透传）后补一次快照。
  const prevConnected = useRef(connected)
  const firstConnectedRun = useRef(true)

  const applyPlan = useCallback((next) => {
    if (!next) return
    const revision = Number(next.revision || 0)
    if (revision < planRev.current) return
    planRev.current = revision
    setPlanState(next)
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
      api(withWorkspace('/api/workspace/crons', workspace)).catch(() => ({ crons: [] })),
      api(sessionPath('/todos')).catch(() => ({ items: [], revision: 0 })),
      api(sessionPath('/plan')).catch(() => null),
    ]).then(([tasks, goalBody, plansBody, cronBody, todoBody, planBody]) => {
      setData(tasks)
      setActivePlanId(tasks.active_plan_id || null)
      setGoal(goalBody.goal)
      setPlans(plansBody.plans || [])
      setScheduledCrons(cronBody.crons || [])
      const revision = Number(todoBody.revision || 0)
      if (revision >= todoRev.current) {
        todoRev.current = revision
        setWebTodos(todoBody.items || [])
      }
      if (planBody) applyPlan(planBody)
    }).catch((error) => onError?.(`任务加载失败：${error.message}`))
  }, [applyPlan, currentId, onError, sessionPath, workspace])

  // 切换会话：清空所有派生状态并拉一次快照。
  // 实时事件由 App 的唯一 EventSource 透传（taskEvent），这里不再单独建连接。
  useEffect(() => {
    if (!currentId) return undefined
    todoRev.current = -1
    planRev.current = -1
    firstConnectedRun.current = true
    prevConnected.current = connected
    setData(EMPTY_TASKS)
    setActivePlanId(null)
    setWebTodos([])
    setPlans([])
    setGoal(null)
    setPlanState(EMPTY_PLAN)
    setScheduledCrons([])
    setNewTodo('')
    setCronSchedule('')
    setCronPrompt('')
    setBusy(false)
    setCronBusy(false)
    loadAll()
    return undefined
    // connected 只在切换会话时取初始值，断线重连由下面的 effect 处理。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [applyPlan, currentId, loadAll])

  // 应用 App 转发的任务事件。事件带着会话 id：切会话瞬间的尾事件不会被误用。
  useEffect(() => {
    if (!taskEvent || taskEvent.sessionId !== currentId) return
    const payload = taskEvent.payload || {}
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
        setActivePlanId((payload.state && payload.state.plan && payload.state.plan.id) || null)
        break
      case 'plan_mode':
        setData((prev) => ({ ...prev, plan_mode: Boolean(payload.enabled) }))
        // sidecar 是 Web 端权威状态；没有 sidecar 时才跟随 agent 的工具调用。
        setPlanState((prev) => (prev.source === 'web' ? prev : { ...prev, enabled: Boolean(payload.enabled) }))
        break
      case 'tasks':
        if (payload.tasks) {
          setData(payload.tasks)
          setActivePlanId(payload.tasks.active_plan_id || null)
        }
        break
      case 'plans':
        setPlans(payload.plans || [])
        break
      default:
        break
    }
  }, [applyPlan, currentId, taskEvent])

  // 断线重连（connected false → true）后事件可能已丢，补一次快照；
  // 首次挂载不重复拉取（切换会话的 effect 已经拉过）。
  useEffect(() => {
    if (firstConnectedRun.current) {
      firstConnectedRun.current = false
      prevConnected.current = connected
      return
    }
    const was = prevConnected.current
    prevConnected.current = connected
    if (connected && !was && currentId) loadAll()
  }, [connected, currentId, loadAll])

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

  const addTodo = () => {
    const text = newTodo.trim()
    if (!text) return
    saveTodos([...webTodos, { text, done: false }])
    setNewTodo('')
  }

  // 计划模式开关是唯一的写入口：计划内容本身全部由模型通过 create_plan /
  // plan_update 维护，面板只负责只读展示。
  const togglePlanMode = async () => {
    if (!currentId || busy) return
    setBusy(true)
    try {
      const next = await putJson(sessionPath('/plan'), { enabled: !planState.enabled })
      applyPlan(next)
    } catch (error) {
      onError?.(`计划模式切换失败：${error.message}`)
    } finally {
      setBusy(false)
    }
  }

  const addCron = async () => {
    const schedule = cronSchedule.trim()
    const prompt = cronPrompt.trim()
    if (!schedule || !prompt) { onError?.('定时任务需要填写表达式和提示词'); return }
    if (cronBusy) return
    setCronBusy(true)
    try {
      const body = await postJson('/api/workspace/crons', { schedule, prompt, workspace })
      setScheduledCrons((list) => [body.cron, ...list])
      setCronSchedule('')
      setCronPrompt('')
      onToast?.('定时任务已创建（真实执行将在 Phase 6 接入）')
    } catch (error) {
      onError?.(`创建定时任务失败：${error.message}`)
    } finally {
      setCronBusy(false)
    }
  }

  const toggleCron = async (cron) => {
    if (cronBusy) return
    setCronBusy(true)
    try {
      const body = await api(
        withWorkspace(`/api/workspace/crons/${encodeURIComponent(cron.cron_id)}/toggle`, workspace),
        { method: 'POST' },
      )
      setScheduledCrons((list) => list.map((item) => (item.cron_id === cron.cron_id ? body.cron : item)))
    } catch (error) {
      onError?.(`切换定时任务失败：${error.message}`)
    } finally {
      setCronBusy(false)
    }
  }

  const deleteCron = async (cron) => {
    if (cronBusy) return
    if (!window.confirm(`删除定时任务「${cron.description || cron.schedule}」？`)) return
    setCronBusy(true)
    try {
      await api(
        withWorkspace(`/api/workspace/crons/${encodeURIComponent(cron.cron_id)}`, workspace),
        { method: 'DELETE' },
      )
      setScheduledCrons((list) => list.filter((item) => item.cron_id !== cron.cron_id))
    } catch (error) {
      onError?.(`删除定时任务失败：${error.message}`)
    } finally {
      setCronBusy(false)
    }
  }

  // Phase 4 骨架：后端明确返回 501，这里把原因原样告诉用户。
  const runCron = async (cron) => {
    try {
      await api(
        withWorkspace(`/api/workspace/crons/${encodeURIComponent(cron.cron_id)}/run`, workspace),
        { method: 'POST' },
      )
    } catch (error) {
      onError?.(`立即执行：${error.detail || error.message}`)
    }
  }

  if (!currentId) return <div className="empty">先选择会话</div>

  const plan = planState.plan
  const steps = readSteps(plan)
  const stats = summarize(steps)
  const heroId = plan?.id || activePlanId
  const heroUpdatedMs = planUpdatedMs(plans, heroId)
  const archive = plans.filter((item) => item.id !== heroId && item.plan && typeof item.plan === 'object')
  const agentTodos = data.todos || []
  const openTodos = webTodos.filter((todo) => !todo.done).length
  const activities = [
    ...(data.tasks || []).map((task) => ({ tone: 'task', tool: task.tool, detail: task.detail })),
    ...(data.crons || []).map((cron) => ({ tone: 'cron', tool: cron.tool, detail: cron.detail })),
  ]
  const goalText = goal && (goal.text || goal.state)
  const hasGoal = Boolean(goalText)
  const modeHint = planState.enabled
    ? '计划模式已开启 · 模型会先规划再执行'
    : '计划模式关闭 · 模型仍可自行创建计划'

  return (
    <div className="pane tasks-pane">
      <div className="tp-head">
        <div className="tp-head-text">
          <div className="tp-title">
            任务与计划
            <span className={`task-conn ${connected ? 'on' : ''}`} title={connected ? '事件流已连接' : '事件流断开，正在重连'} />
          </div>
          <div className="tp-sub">计划由模型在执行过程中自动维护</div>
        </div>
        <div className="tp-head-actions">
          <button className="tp-icon-btn" onClick={loadAll} title="刷新" aria-label="刷新">
            ↻
          </button>
          <button
            className={`tp-mode ${planState.enabled ? 'on' : ''}`}
            disabled={busy}
            onClick={togglePlanMode}
            title={modeHint}
          >
            <span className="tp-mode-track"><span className="tp-mode-thumb" /></span>
            计划模式
          </button>
        </div>
      </div>

      <div className="pane-body tp-body">
        {plan ? (
          <section className={`tp-card tp-plan ${stats.tone}`}>
            <header className="tp-plan-head">
              <div className="tp-ring" style={{ '--pct': stats.pct }} aria-label={`完成 ${stats.pct}%`}>
                <span>{stats.pct}<i>%</i></span>
              </div>
              <div className="tp-plan-head-main">
                <div className="tp-eyebrow">
                  当前计划
                  <span className={`tp-pill ${stats.tone}`}>
                    {stats.total ? TONE_LABEL[stats.tone] : '待补充'}
                  </span>
                  {planState.approved && <span className="tp-pill idle">已确认</span>}
                </div>
                <h3 className="tp-summary">{plan.task_summary || plan.title || '未命名计划'}</h3>
                <div className="tp-plan-meta">
                  <span>{stats.total ? `${stats.done}/${stats.total} 步完成` : '暂无步骤'}</span>
                  {stats.executing >= 0 && (
                    <span className="tp-live">正在执行第 {stats.executing + 1} 步</span>
                  )}
                  {stats.failed > 0 && <span className="tp-bad">{stats.failed} 步失败</span>}
                  {heroUpdatedMs > 0 && <span>{fmtTime(heroUpdatedMs)} 更新</span>}
                  {plan.id && <code>{plan.id}</code>}
                </div>
              </div>
            </header>

            <div className="tp-progress" role="progressbar" aria-valuenow={stats.pct} aria-valuemin={0} aria-valuemax={100}>
              <i style={{ width: `${stats.pct}%` }} />
            </div>

            {steps.length > 0 ? (
              <ol className="tp-steps">
                {steps.map((step) => (
                  <li key={step.id} className={`tp-step ${step.status}`}>
                    <span className="tp-node" aria-hidden="true">
                      {step.status === 'done' ? '✓' : step.status === 'failed' ? '✕' : step.status === 'skipped' ? '–' : ''}
                    </span>
                    <div className="tp-step-main">
                      <div className="tp-step-text">
                        <span className="tp-step-index">{step.index}</span>
                        {step.description}
                      </div>
                      {step.acceptance && <div className="tp-accept">验收：{step.acceptance}</div>}
                    </div>
                    {(step.status === 'executing' || step.status === 'failed' || step.status === 'skipped') && (
                      <span className={`tp-step-state ${step.status}`}>{STATUS_LABEL[step.status]}</span>
                    )}
                  </li>
                ))}
              </ol>
            ) : (
              <div className="tp-plan-empty">计划已创建，等待模型补充步骤。</div>
            )}

            <footer className="tp-foot">
              <span>{modeHint}</span>
              <span className="tp-foot-live">{connected ? '实时同步' : '连接中断'}</span>
            </footer>
          </section>
        ) : (
          <section className="tp-card tp-empty">
            <div className="tp-empty-glyph"><PlanGlyph /></div>
            <h3>还没有执行计划</h3>
            <p>
              发送任务后，模型会先调用 <code>create_plan</code> 拆出步骤，执行中用
              <code>plan_update</code> 推进状态。这里只做只读展示，无需手动维护。
            </p>
            <div className="tp-empty-hint">
              <span className={`tp-dot ${planState.enabled ? 'on' : ''}`} />
              {modeHint}
            </div>
          </section>
        )}

        {hasGoal && (
          <section className="tp-card tp-goal">
            <div className="tp-goal-head">
              <span className="tp-eyebrow">目标</span>
              <span className={`tp-pill ${goalTone(goal.state)}`}>
                {GOAL_STATE_LABEL[goal.state] || goal.state || '未设置'}
              </span>
            </div>
            {goal.text && <p>{goal.text}</p>}
            <div className="tp-goal-stats">
              <span className="tp-stat"><b>{Number(goal.tokens_used || 0).toLocaleString()}</b> tokens</span>
              {goal.token_budget ? <span className="tp-stat">预算 <b>{Number(goal.token_budget).toLocaleString()}</b></span> : null}
              <span className="tp-stat">阻塞 <b>{goal.blocked_count || 0}</b> 次</span>
            </div>
            {goal.token_budget ? (
              <div className="tp-progress slim">
                <i style={{ width: `${Math.min(100, Math.round((Number(goal.tokens_used || 0) / Number(goal.token_budget)) * 100))}%` }} />
              </div>
            ) : null}
          </section>
        )}

        {agentTodos.length > 0 && (
          <section className="tp-section">
            <div className="tp-section-head">
              模型待办
              <span className="tp-count">{agentTodos.length}</span>
            </div>
            <div className="tp-list">
              {agentTodos.map((todo, index) => {
                const done = ['completed', 'done'].includes(todo.status)
                const running = ['in_progress', 'running'].includes(todo.status)
                return (
                  <div key={`${index}-${todo.content || ''}`} className={`tp-todo ${done ? 'done' : ''} ${running ? 'running' : ''}`}>
                    <span className={`tp-node ${done ? 'done' : running ? 'executing' : 'pending'}`} aria-hidden="true">
                      {done ? '✓' : ''}
                    </span>
                    <span className="tp-todo-text">{todo.content}</span>
                    {todo.status && <span className="tp-todo-state">{running ? '进行中' : done ? '完成' : '待办'}</span>}
                  </div>
                )
              })}
            </div>
          </section>
        )}

        {archive.length > 0 && (
          <details className="tp-archive-group">
            <summary>
              计划档案
              <span className="tp-count">{archive.length}</span>
            </summary>
            <div className="tp-archive">
              {archive.map((item) => {
                const itemSteps = readSteps(item.plan)
                const itemStats = summarize(itemSteps)
                return (
                  <details key={item.id} className="tp-archive-item">
                    <summary>
                      <span className={`tp-dot ${itemStats.tone === 'done' ? 'on' : ''}`} />
                      <span className="tp-archive-title">{item.plan.task_summary || item.plan.title || item.id}</span>
                      <span className="tp-archive-pct">{itemStats.total ? `${itemStats.done}/${itemStats.total}` : '—'}</span>
                    </summary>
                    <div className="tp-archive-body">
                      <div className="tp-progress slim"><i style={{ width: `${itemStats.pct}%` }} /></div>
                      {itemSteps.length > 0 && (
                        <ul className="tp-archive-steps">
                          {itemSteps.map((step) => (
                            <li key={step.id} className={step.status}>
                              <span className={`tp-node ${step.status}`} aria-hidden="true">
                                {step.status === 'done' ? '✓' : step.status === 'failed' ? '✕' : step.status === 'skipped' ? '–' : ''}
                              </span>
                              <span className="tp-archive-step-text">{step.description}</span>
                            </li>
                          ))}
                        </ul>
                      )}
                      <div className="tp-archive-meta">
                        {item.updated_ms ? `${fmtTime(item.updated_ms)} 更新` : '时间未知'}
                        {item.plan.status ? ` · ${item.plan.status}` : ''}
                      </div>
                    </div>
                  </details>
                )
              })}
            </div>
          </details>
        )}

        <section className="tp-section">
          <div className="tp-section-head">
            我的待办
            <span className="tp-count">{openTodos}/{webTodos.length}</span>
          </div>
          {webTodos.length > 0 && (
            <div className="tp-list">
              {webTodos.map((todo, index) => (
                <label key={`${index}-${todo.text}`} className={`tp-todo ${todo.done ? 'done' : ''}`}>
                  <input
                    type="checkbox"
                    checked={Boolean(todo.done)}
                    onChange={() => saveTodos(webTodos.map((item, i) => (i === index ? { ...item, done: !item.done } : item)))}
                  />
                  <span className="tp-todo-text">{todo.text}</span>
                  <button
                    className="ghost tp-todo-del"
                    title="删除"
                    onClick={(event) => {
                      event.preventDefault()
                      saveTodos(webTodos.filter((_, i) => i !== index))
                    }}
                  >
                    ×
                  </button>
                </label>
              ))}
            </div>
          )}
          {webTodos.length === 0 && <div className="tp-empty-line">还没有待办，添加一条试试</div>}
          <div className="tp-todo-add">
            <input
              value={newTodo}
              placeholder="添加待办，Enter 保存"
              onChange={(event) => setNewTodo(event.target.value)}
              onKeyDown={(event) => {
                if (event.key !== 'Enter') return
                event.preventDefault()
                addTodo()
              }}
            />
            <button onClick={addTodo}>添加</button>
          </div>
        </section>

        <section className="tp-section">
          <div className="tp-section-head">
            定时任务
            <span className="tp-count">{scheduledCrons.length}</span>
          </div>
          {scheduledCrons.length > 0 && (
            <div className="tp-list">
              {scheduledCrons.map((cron) => (
                <div key={cron.cron_id} className={`tp-cron ${cron.enabled ? '' : 'off'}`}>
                  <span className="tp-cron-schedule">{cron.schedule}</span>
                  <span className="tp-cron-prompt" title={cron.prompt}>{cron.description || cron.prompt}</span>
                  <span className="tp-cron-actions">
                    <button disabled={cronBusy} onClick={() => toggleCron(cron)}>{cron.enabled ? '停用' : '启用'}</button>
                    <button disabled={cronBusy} onClick={() => runCron(cron)}>执行</button>
                    <button disabled={cronBusy} onClick={() => deleteCron(cron)}>×</button>
                  </span>
                </div>
              ))}
            </div>
          )}
          {scheduledCrons.length === 0 && <div className="tp-empty-line">还没有定时任务（真实执行将在 Phase 6 接入）</div>}
          <div className="tp-cron-add">
            <input
              value={cronSchedule}
              placeholder="表达式，如 0 9 * * *"
              onChange={(event) => setCronSchedule(event.target.value)}
            />
            <input
              value={cronPrompt}
              placeholder="到点执行的提示词"
              onChange={(event) => setCronPrompt(event.target.value)}
              onKeyDown={(event) => {
                if (event.key !== 'Enter') return
                event.preventDefault()
                addCron()
              }}
            />
            <button disabled={cronBusy} onClick={addCron}>添加</button>
          </div>
        </section>

        {activities.length > 0 && (
          <details className="tp-activity">
            <summary>
              执行记录
              <span className="tp-count">{activities.length}</span>
            </summary>
            <div className="tp-activity-body">
              {activities.map((item, index) => (
                <div key={index} className="tp-activity-row">
                  <span className="tp-activity-tool">{item.tone === 'cron' ? '⏱' : '◆'} {item.tool}</span>
                  <span className="tp-activity-detail">{item.detail}</span>
                </div>
              ))}
            </div>
          </details>
        )}
      </div>
    </div>
  )
}

// 当前计划对应的文件更新时间：/plan 只带 revision（可能是 sidecar 计数器），
// 真实的毫秒时间在计划文件列表里，按 id 取。
function planUpdatedMs(plans, id) {
  if (!id) return 0
  const found = plans.find((item) => item.id === id)
  return Number(found?.updated_ms || 0)
}
