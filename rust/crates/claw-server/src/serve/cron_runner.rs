//! Phase 6:serve 进程内定时任务触发器。
//!
//! 每 15 秒扫描各工作区 `.claw/crons.json` 中启用的任务:到期的任务被封装为
//! [`PromptJob`] 进入现有 [`Scheduler`],与会话内消息共用同一套并发门闸与
//! 权限模式;每个任务在首次执行时创建一个专属会话,之后固定复用。
//!
//! 下次执行时间只保存在进程内(键 = 工作区 + cron_id),启动时以
//! `last_run_at`(没有则 `created_at`)为基准重算:停机期间到点的任务
//! 启动后只补跑最近一次,不会把错过的每个时间点都重放一遍。

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use axum::http::StatusCode;
use chrono::Local;
use serde_json::{json, Value};

use super::cron_expr::CronSchedule;
use super::scheduler::{EnqueueError, PromptJob, SessionState};
use super::{
    create_session_file, cron_entry_json, cron_registry_for, load_web_settings, now_ms,
    save_session_title, session_roots, spawn_session_worker, AppState, CRON_STORE_REL,
};

/// 扫描间隔:15 秒足以覆盖分钟级 cron,又不会让空转太频繁。
pub const CRON_TICK: Duration = Duration::from_secs(15);

/// 下次到期时间表。`i64::MAX` 表示表达式在可预见的未来不会命中。
#[derive(Default)]
struct NextDue {
    entries: Mutex<HashMap<String, i64>>,
}

impl NextDue {
    fn get_or_set(&self, key: &str, compute: impl FnOnce() -> i64) -> i64 {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *entries.entry(key.to_string()).or_insert_with(compute)
    }

    fn set(&self, key: &str, value: i64) {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        entries.insert(key.to_string(), value);
    }
}

/// 启动后台 tick 线程;重复调用会启动多个线程,调用方只需在 serve 启动时调用一次。
pub fn spawn_cron_runner(state: AppState) {
    let next_due = NextDue::default();
    let spawned = std::thread::Builder::new()
        .name("claw-cron".to_string())
        .spawn(move || loop {
            // 单次扫描 panic 只丢一轮:线程必须活到进程退出,否则所有定时任务静默停摆。
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                tick(&state, &next_due);
            }));
            if let Err(payload) = result {
                eprintln!(
                    "[serve] 定时任务扫描异常:{}",
                    super::panic_payload_message(&*payload)
                );
            }
            std::thread::sleep(CRON_TICK);
        });
    if let Err(error) = spawned {
        eprintln!("[serve] 定时任务线程启动失败:{error}");
    }
}

fn tick(state: &AppState, next_due: &NextDue) {
    let now = Local::now();
    let now_ms = now_ms();
    let now_i64 = i64::try_from(now_ms).unwrap_or(i64::MAX);
    for root in session_roots(state) {
        // 没有 crons.json 的工作区直接跳过:避免 tick 顺手在用户目录创建 .claw。
        if !root.join(CRON_STORE_REL).exists() {
            continue;
        }
        let _guard = state
            .crons_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Ok(registry) = cron_registry_for(&root) else {
            continue;
        };
        for entry in registry.list(true) {
            let Ok(schedule) = CronSchedule::parse(&entry.schedule) else {
                continue;
            };
            let key = format!("{}|{}", root.display(), entry.cron_id);
            let base_ms = entry
                .last_run_at
                .unwrap_or(entry.created_at)
                .saturating_mul(1000);
            let due = next_due.get_or_set(&key, || {
                schedule
                    .next_after_ms(base_ms)
                    .and_then(|ms| i64::try_from(ms).ok())
                    .unwrap_or(i64::MAX)
            });
            if now_i64 < due {
                continue;
            }
            // 先推进下次到期时间再执行:即使本轮入队失败,也不会在下一 tick 重复触发。
            let next = schedule
                .next_after_ms(now_ms)
                .and_then(|ms| i64::try_from(ms).ok())
                .unwrap_or(i64::MAX);
            next_due.set(&key, next);

            match enqueue_cron(state, &root, &registry, &entry, "schedule") {
                Ok(_) => {}
                Err((_, reason)) => {
                    let _ = registry.record_finish(&entry.cron_id, "skipped", Some(&reason));
                    if let Some(session) = entry.session_id.as_deref() {
                        state.events.emit(
                            session,
                            json!({
                                "kind": "cron",
                                "phase": "skipped",
                                "cron_id": entry.cron_id,
                                "reason": reason,
                                "at": now_ms,
                            }),
                        );
                    }
                }
            }
        }
    }
}

/// 立即执行(手动触发)。并发与自动触发共用同一条入队路径。
pub fn run_now(
    state: &AppState,
    workspace: &Path,
    cron_id: &str,
) -> Result<Value, (StatusCode, String)> {
    let _guard = state
        .crons_lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let registry = cron_registry_for(workspace)?;
    let entry = registry
        .get(cron_id)
        .ok_or((StatusCode::NOT_FOUND, "定时任务不存在".to_string()))?;
    let session = enqueue_cron(state, workspace, &registry, &entry, "manual")?;
    let updated = registry
        .get(cron_id)
        .map_or(Value::Null, |entry| cron_entry_json(&entry));
    Ok(json!({
        "status": "queued",
        "session": session,
        "cron": updated,
    }))
}

/// 入队一次任务执行。
///
/// 返回的 `Err` 已带 HTTP 状态码,自动触发只取其中的原因文本。
fn enqueue_cron(
    state: &AppState,
    workspace: &Path,
    registry: &runtime::CronRegistry,
    entry: &runtime::CronEntry,
    trigger: &str,
) -> Result<String, (StatusCode, String)> {
    let (session, session_path) = ensure_session(workspace, registry, entry).map_err(|error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("创建定时任务会话失败:{error}"),
        )
    })?;
    if state.scheduler.status(&session).state != SessionState::Idle {
        return Err((
            StatusCode::CONFLICT,
            "该定时任务上一次执行尚未结束,本次跳过".to_string(),
        ));
    }
    let job = PromptJob {
        session_id: session.clone(),
        session_path,
        workspace: workspace.to_path_buf(),
        text: entry.prompt.clone(),
        permission: cron_permission(),
        model: None,
        cron_id: Some(entry.cron_id.clone()),
        resume: None,
    };
    let enqueued = match state.scheduler.enqueue(job) {
        Ok(enqueued) => enqueued,
        Err(EnqueueError::UpgradePending) => {
            return Err((
                StatusCode::CONFLICT,
                "该定时任务会话正在热升级，本次跳过".to_string(),
            ))
        }
        Err(EnqueueError::QueueFull) => {
            return Err((
                StatusCode::TOO_MANY_REQUESTS,
                "定时任务专属会话排队已满,本次跳过".to_string(),
            ))
        }
        Err(EnqueueError::OrphanLimit { active, limit }) => {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                format!("僵死会话线程过多({active}/{limit}),本次跳过"),
            ))
        }
    };
    if let Err(error) = registry.record_start(&entry.cron_id) {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("记录定时任务状态失败:{error}"),
        ));
    }
    if enqueued.start_worker {
        spawn_session_worker(state.clone(), session.clone());
    }
    state.events.emit(
        &session,
        json!({
            "kind": "cron",
            "phase": "start",
            "cron_id": entry.cron_id,
            "trigger": trigger,
            "at": now_ms(),
        }),
    );
    Ok(session)
}

/// 任务结束回调(worker 在正常收尾路径调用)。
pub fn finish_cron_run(
    state: &AppState,
    workspace: &Path,
    cron_id: &str,
    session: &str,
    status: &str,
    error: Option<&str>,
) {
    {
        let _guard = state
            .crons_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Ok(registry) = cron_registry_for(workspace) {
            let _ = registry.record_finish(cron_id, status, error);
        }
    }
    state.events.emit(
        session,
        json!({
            "kind": "cron",
            "phase": "end",
            "cron_id": cron_id,
            "status": status,
            "error": error,
            "at": now_ms(),
        }),
    );
}

/// 取该任务的专属会话;不存在(首次执行或已被删除)时新建并写回绑定。
fn ensure_session(
    workspace: &Path,
    registry: &runtime::CronRegistry,
    entry: &runtime::CronEntry,
) -> Result<(String, std::path::PathBuf), String> {
    if let Some(session_id) = entry.session_id.as_deref() {
        if let Ok(store) = runtime::SessionStore::from_cwd(workspace) {
            let path = store.sessions_dir().join(format!("{session_id}.jsonl"));
            if path.exists() {
                return Ok((session_id.to_string(), path));
            }
        }
    }
    let (session_id, path) = create_session_file(workspace)?;
    let title = cron_session_title(entry);
    if let Err(error) = save_session_title(workspace, &session_id, &title) {
        eprintln!("[serve] 定时任务会话标题写入失败:{error}");
    }
    if let Err(error) = registry.bind_session(&entry.cron_id, &session_id) {
        eprintln!("[serve] 定时任务会话绑定失败:{error}");
    }
    Ok((session_id, path))
}

fn cron_session_title(entry: &runtime::CronEntry) -> String {
    let name = entry
        .description
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| entry.prompt.trim());
    format!("定时任务 · {}", crate::truncate_for_summary(name, 40))
}

/// 定时任务的权限模式:继承 Web 工作区设置,但最高只到 `workspace-write`。
fn cron_permission() -> runtime::PermissionMode {
    let settings = load_web_settings();
    match settings["permission"].as_str().unwrap_or("workspace-write") {
        "read-only" => runtime::PermissionMode::ReadOnly,
        _ => runtime::PermissionMode::WorkspaceWrite,
    }
}
