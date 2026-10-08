//! What the Owner's web page shows: projects, tasks, KPIs, the team and recent activity.
//! Every call reads the store, the registry and the audit files afresh, so the page shows a
//! run while it happens. Served by `web`.

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::Result;
use serde_json::{Value, json};

use crate::{
    kpi,
    project::{Store, TaskStatus},
    registry::{Capability, Registry},
};

/// History lines shown per task, newest last.
const HISTORY_LINES: usize = 12;
/// Audit events in the activity feed.
const ACTIVITY_EVENTS: usize = 150;

/// Where the dashboard reads from.
#[derive(Debug, Clone)]
pub struct Sources {
    pub data_dir: PathBuf,
    pub employees_dir: PathBuf,
    pub audit_dir: PathBuf,
}

/// Everything the page shows, as one JSON document.
pub fn snapshot(sources: &Sources) -> Result<Value> {
    let store = Store::open(&sources.data_dir)?;
    let mut projects = Vec::new();
    for project in store.projects()? {
        let config = &project.config;
        let id = &config.project_id;
        let milestones = store.milestones(id)?;
        let mut usage = store.planning_usage(id)?;
        let mut tasks = Vec::new();
        for task in store.tasks(id)? {
            let state = store.load_task_state(&task.id)?;
            if let Some(state) = &state {
                usage.add(&state.usage);
            }
            let milestone = milestones
                .iter()
                .find(|milestone| milestone.id == task.milestone_id)
                .map(|milestone| milestone.name.clone());
            tasks.push(json!({
                "id": task.id,
                "title": task.title,
                "status": task.status.as_str(),
                "needs_owner": task.status.needs_owner(),
                "milestone": milestone,
                "depends_on": task.depends_on,
                "iteration_budget": task.iteration_budget,
                "error": task.error,
                "iterations": state.as_ref().map(|state| state.iteration),
                "spec_revisions": state.as_ref().map(|state| state.architecture_revisions),
                "team": state.as_ref().and_then(|state| state.team.clone()),
                "review": state.as_ref().and_then(|state| state.review.as_ref()).map(|review| json!({
                    "decision": format!("{:?}", review.decision),
                    "feedback": review.feedback,
                })),
                "tests": state
                    .as_ref()
                    .and_then(|state| state.verification.as_ref())
                    .map(|verification| match &verification.test {
                        Some(test) if test.passed => "passed",
                        Some(_) => "failed",
                        None => "none",
                    }),
                "warnings": state
                    .as_ref()
                    .and_then(|state| state.verification.as_ref())
                    .map(|verification| verification.warnings.clone())
                    .unwrap_or_default(),
                "history": state.as_ref().map(|state| {
                    let skip = state.history.len().saturating_sub(HISTORY_LINES);
                    state.history[skip..].to_vec()
                }).unwrap_or_default(),
                "usage": state.as_ref().map(|state| state.usage.clone()),
            }));
        }
        let count = |status: TaskStatus| {
            tasks
                .iter()
                .filter(|task| task["status"] == status.as_str())
                .count()
        };
        projects.push(json!({
            "id": id,
            "name": config.name,
            "status": project.status.to_string(),
            "test_command": config.test_command,
            "remote_url": config.remote_url,
            "budget": config.budget.as_ref().map(ToString::to_string),
            "budget_stop": config
                .budget
                .as_ref()
                .and_then(|budget| budget.exceeded(&usage)),
            "usage": usage,
            "counts": {
                "total": tasks.len(),
                "done": count(TaskStatus::Done),
                "waiting": tasks.iter().filter(|task| task["needs_owner"] == true).count(),
                "running": count(TaskStatus::InProgress) + count(TaskStatus::Review) + count(TaskStatus::ChangesRequired),
            },
            "pending_plan": store.pending_plan(id)?,
            // The project's own statistics (the page shows them for the selected project).
            "kpi": kpi::collect(&store, Some(id))?.overall,
            "milestones": milestones.iter().map(|milestone| json!({
                "name": milestone.name,
                "report": milestone.report_path,
            })).collect::<Vec<_>>(),
            "tasks": tasks,
        }));
    }

    let (team, proposals) = match Registry::load(&sources.employees_dir) {
        Ok(registry) => (team(&registry), proposals(registry.company_dir())),
        Err(err) => (json!({ "error": format!("{err:#}") }), Vec::new()),
    };

    Ok(json!({
        "generated_unix_ms": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis() as u64)
            .unwrap_or_default(),
        "projects": projects,
        "kpi": kpi::collect(&store, None)?,
        "team": team,
        "proposals": proposals,
        "activity": activity(&sources.audit_dir),
    }))
}

fn team(registry: &Registry) -> Value {
    let employees: Vec<Value> = registry
        .employees()
        .map(|employee| {
            let contract = &employee.contract;
            json!({
                "id": employee.id(),
                "name": contract.name,
                "title": contract.title,
                "function": contract.function.to_string(),
                "department": contract.department,
                "manager": contract.manager_id,
                "status": serde_json::to_value(contract.status).unwrap_or(Value::Null),
                "model": contract.model.as_ref().map(|model| model.model.clone()),
                // The whole model block, for the team settings window.
                "model_config": contract.model,
                "skills": contract.skills,
                "permissions": contract.permissions,
                "skill_packs": contract.skill_packs,
                "mcp_servers": contract.mcp_servers,
                "veto": employee.has(Capability::VetoReview),
            })
        })
        .collect();
    json!({ "employees": employees })
}

/// Hiring proposals waiting for the Owner in `company/proposals/`.
fn proposals(company: &Path) -> Vec<String> {
    let mut ids: Vec<String> = fs::read_dir(company.join("proposals"))
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry.path().is_dir())
                .map(|entry| entry.file_name().to_string_lossy().to_string())
                .collect()
        })
        .unwrap_or_default();
    ids.sort();
    ids
}

/// The newest audit events across run files, newest first, without payloads (they can hold
/// whole prompts and files).
fn activity(audit_dir: &Path) -> Vec<Value> {
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = fs::read_dir(audit_dir)
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "jsonl"))
                .filter_map(|entry| Some((entry.metadata().ok()?.modified().ok()?, entry.path())))
                .collect()
        })
        .unwrap_or_default();
    files.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));

    let mut events = Vec::new();
    for (_, path) in files.into_iter().take(10) {
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let mut run_task = Value::Null;
        for line in text.lines() {
            let Ok(event) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            // RUN_CREATED / RUN_RESUMED name the task; later events of the run inherit it.
            if !event["payload"]["task_id"].is_null() {
                run_task = event["payload"]["task_id"].clone();
            }
            let task = run_task.clone();
            events.push(json!({
                "unix_ms": event["unix_ms"],
                "run_id": event["run_id"],
                "actor": event["actor"],
                "event": event["event"],
                "task_id": task,
            }));
        }
    }
    events.sort_by_key(|event| std::cmp::Reverse(event["unix_ms"].as_u64().unwrap_or(0)));
    events.truncate(ACTIVITY_EVENTS);
    events
}
