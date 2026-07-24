//! DAG status reporting (G8.10) — render DAG execution status as human-readable text.
//!
//! Used by `dag_status` tool to provide LLM-visible progress reports.

use crate::multi_agent::dag::types::{Dag, DagNodeStatus, DagRun, DagStatus};

/// Render a DAG run status as a human-readable summary.
#[must_use]
pub fn render_dag_status(dag: &Dag, run: &DagRun) -> String {
    let mut out = String::with_capacity(256 + dag.nodes.len() * 80);

    let status_icon = match run.status {
        DagStatus::Pending => "⏳",
        DagStatus::Running => "▶",
        DagStatus::Completed => "✓",
        DagStatus::Failed => "✗",
        DagStatus::Cancelled => "⊘",
    };

    out.push_str(&format!(
        "{} DAG `{}` ({})\n",
        status_icon, dag.name, dag.id
    ));
    out.push_str(&format!("  Status: {:?}\n", run.status));
    out.push_str(&format!("  Run ID: {}\n", run.id));

    if let Some(started) = run.started_at {
        out.push_str(&format!("  Started: {}s\n", started));
    }
    if let Some(completed) = run.completed_at {
        if let Some(started) = run.started_at {
            out.push_str(&format!(
                "  Duration: {}s\n",
                completed.saturating_sub(started)
            ));
        }
    }

    out.push_str("\nNodes:\n");
    for node in &dag.nodes {
        let status = run.node_status(&node.id).unwrap_or(DagNodeStatus::Pending);
        let icon = match status {
            DagNodeStatus::Pending => "⏳",
            DagNodeStatus::Ready => "○",
            DagNodeStatus::Running => "▶",
            DagNodeStatus::Succeeded => "✓",
            DagNodeStatus::Failed => "✗",
            DagNodeStatus::Skipped => "⊘",
        };
        out.push_str(&format!("  {} {} — {}\n", icon, node.id, node.label));
    }

    out
}

/// Render a compact one-line summary for status bar use.
#[must_use]
pub fn render_dag_summary(run: &DagRun) -> String {
    let total = run.node_statuses.len();
    let succeeded = run
        .node_statuses
        .iter()
        .filter(|(_, s)| *s == DagNodeStatus::Succeeded)
        .count();
    let failed = run
        .node_statuses
        .iter()
        .filter(|(_, s)| *s == DagNodeStatus::Failed)
        .count();
    let running = run
        .node_statuses
        .iter()
        .filter(|(_, s)| *s == DagNodeStatus::Running)
        .count();

    format!(
        "DAG {:?}: {}/{} done, {} running, {} failed",
        run.status, succeeded, total, running, failed
    )
}
