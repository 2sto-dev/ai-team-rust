//! Project execution: plans tasks through the Planner, runs them one at a time through the
//! orchestrator, persists every step, and applies Owner decisions to stopped tasks.

use std::{collections::HashMap, fmt::Write as _, fs, path::PathBuf, time::Duration};

use anyhow::{Context, Result, bail};

use super::{
    ProjectStatus, Store, TaskRecord, TaskStatus,
    plan::{TaskPlan, validate_plan},
};
use crate::{
    domain::{ProjectConfig, Stage, TeamState},
    llm::UsageTotals,
    orchestrator::Orchestrator,
    planner::candidates,
    workbench::TaskWorkbench,
    workspace::Workspace,
};

/// Planner proposals per `project plan` before giving up (each rejection is fed back).
const MAX_TASK_PLAN_ATTEMPTS: u32 = 3;
/// How much of a dependency's approved implementation is passed to a dependent task.
const DEPENDENCY_CONTEXT_CHARS: usize = 16_000;

/// How much of the project's files a question sees.
const QUESTION_SNAPSHOT_CHARS: usize = 40_000;
/// How much of the documents attached to a question the model sees (all of them together).
/// With the project snapshot this stays near 30k tokens of a 48k context.
const ATTACHMENT_CHARS: usize = 80_000;

const CONSULTANT_PROMPT: &str = "\
You are the team's technical lead. The Owner asks a question about their project. Answer only \
from the project files, task list and attached documents you are given; say so when something \
is not there. Be \
concise and concrete: name files and functions, and give exact commands (run from the project \
folder). Do not write new code or propose a rewrite unless the Owner asks for it. Answer in the \
language of the question.";

#[derive(Debug)]
pub struct RunSummary {
    /// Tasks worked on in this call, with the status each one ended in.
    pub executed: Vec<(String, TaskStatus)>,
    /// Milestone reports written in this call.
    pub reports: Vec<PathBuf>,
    /// Pushes to the project's remote that failed (the work itself is safe locally).
    pub push_errors: Vec<String>,
    /// Set when the project's budget stopped the run before the next task.
    pub budget_stop: Option<String>,
    pub project_status: ProjectStatus,
}

/// Owner changes to a project's settings; `None` leaves a setting as it is.
#[derive(Debug, Default)]
pub struct ProjectSettings {
    /// "" removes it.
    pub test_command: Option<String>,
    pub test_timeout_secs: Option<u64>,
    /// "" removes it.
    pub remote_url: Option<String>,
    /// 0 removes the limit.
    pub budget_tokens: Option<u64>,
    /// 0 removes the limit.
    pub budget_usd: Option<f64>,
}

/// What `ProjectManager::delete_project` removed.
#[derive(Debug, Clone)]
pub struct ProjectDeletion {
    pub project_id: String,
    pub workspace_removed: bool,
    pub audit_files: usize,
    pub reports: usize,
    pub artifacts: usize,
}

/// Deletes a folder tree, clearing read-only flags first (git marks its objects read-only,
/// which makes a plain `remove_dir_all` fail on Windows).
fn remove_tree(root: &std::path::Path) -> Result<()> {
    fn writable(path: &std::path::Path) {
        if let Ok(meta) = fs::symlink_metadata(path) {
            if meta.is_dir() {
                if let Ok(entries) = fs::read_dir(path) {
                    for entry in entries.flatten() {
                        writable(&entry.path());
                    }
                }
            } else {
                let mut permissions = meta.permissions();
                #[allow(clippy::permissions_set_readonly_false)]
                permissions.set_readonly(false);
                let _ = fs::set_permissions(path, permissions);
            }
        }
    }
    writable(root);
    fs::remove_dir_all(root)?;
    Ok(())
}

pub struct ProjectManager<'a> {
    store: &'a Store,
    orchestrator: &'a Orchestrator,
    /// Print each step of a running task (for an Owner watching the console).
    progress: bool,
}

impl<'a> ProjectManager<'a> {
    pub fn new(store: &'a Store, orchestrator: &'a Orchestrator) -> Self {
        Self {
            store,
            orchestrator,
            progress: false,
        }
    }

    pub fn with_progress(mut self, progress: bool) -> Self {
        self.progress = progress;
        self
    }

    pub fn store(&self) -> &Store {
        self.store
    }

    /// The project's git repository (`<data>/workspaces/<project>`); `main` holds approved work.
    pub fn main_workspace(&self, project_id: &str) -> PathBuf {
        self.store.root().join("workspaces").join(project_id)
    }

    /// Opens (creating an empty one for a new project) the project's repository.
    pub fn repository(&self, project: &ProjectConfig) -> Result<Workspace> {
        let workspace = Workspace::open(&self.main_workspace(&project.project_id))?;
        workspace.set_remote(project.remote_url.as_deref())?;
        Ok(workspace)
    }

    /// Pushes branches to the Owner's remote, if one is configured. A failed push never
    /// changes a task's outcome; it is reported and retried with the next push.
    fn push(
        &self,
        project: &ProjectConfig,
        workspace: &Workspace,
        branches: &[String],
    ) -> Vec<String> {
        if project.remote_url.is_none() {
            return Vec::new();
        }
        let mut errors = Vec::new();
        for branch in branches {
            if let Err(err) = workspace.push(branch) {
                tracing::warn!(%branch, error = %format!("{err:#}"), "push failed");
                errors.push(format!("push {branch}: {err:#}"));
            }
        }
        errors
    }

    /// Owner-only settings that can change after `project add`. An empty string removes the
    /// test command or the remote.
    pub fn configure(&self, project_id: &str, settings: ProjectSettings) -> Result<ProjectConfig> {
        let mut config = self.store.project(project_id)?.config;
        if let Some(url) = settings.remote_url {
            config.remote_url = (!url.trim().is_empty()).then(|| url.trim().to_string());
        }
        if let Some(command) = settings.test_command {
            config.test_command = (!command.trim().is_empty()).then_some(command);
        }
        if let Some(timeout) = settings.test_timeout_secs {
            anyhow::ensure!(timeout > 0, "test timeout must be at least 1 second");
            config.test_timeout_secs = timeout;
        }
        if settings.budget_tokens.is_some() || settings.budget_usd.is_some() {
            let mut budget = config.budget.take().unwrap_or_default();
            if let Some(tokens) = settings.budget_tokens {
                budget.max_tokens = (tokens > 0).then_some(tokens);
            }
            if let Some(cost) = settings.budget_usd {
                anyhow::ensure!(cost >= 0.0, "the cost budget cannot be negative");
                budget.max_cost_usd = (cost > 0.0).then_some(cost);
            }
            config.budget = (!budget.is_empty()).then_some(budget);
        }
        self.store.update_project_config(&config)?;
        self.store.add_owner_decision(
            project_id,
            None,
            "configure",
            Some(&format!(
                "test_command={:?}, test_timeout_secs={}, remote_url={:?}, budget={}",
                config.test_command,
                config.test_timeout_secs,
                config.remote_url,
                config.budget.clone().unwrap_or_default()
            )),
        )?;
        Ok(config)
    }

    /// Copies the Owner's files into the project repository (`inputs/` on main).
    pub fn add_inputs(&self, project_id: &str, files: &[PathBuf]) -> Result<Vec<String>> {
        if files.is_empty() {
            return Ok(Vec::new());
        }
        let project = self.store.project(project_id)?.config;
        let paths = self.repository(&project)?.add_owner_inputs(files)?;
        self.store
            .add_owner_decision(project_id, None, "inputs", Some(&paths.join(", ")))?;
        Ok(paths)
    }

    /// Turns an Owner prompt into a task of the project; it runs on the next `run`.
    pub fn add_request(
        &self,
        project_id: &str,
        prompt: &str,
        inputs: &[String],
    ) -> Result<TaskRecord> {
        let project = self.store.project(project_id)?.config;
        let title: String = prompt
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("Owner request")
            .chars()
            .take(100)
            .collect();
        let mut description = prompt.trim().to_string();
        if !inputs.is_empty() {
            description.push_str(&format!(
                "\n\nFiles provided by the Owner (in the workspace): {}",
                inputs.join(", ")
            ));
        }
        let mut acceptance = vec![
            "The result does exactly what the Owner asked, completely and correctly.".to_string(),
        ];
        if let Some(command) = &project.test_command {
            acceptance.push(format!("`{command}` passes."));
        }
        let id = self.store.add_owner_task(
            project_id,
            "Owner requests",
            title.trim(),
            &description,
            &acceptance,
            project.max_iterations,
        )?;
        self.store
            .add_owner_decision(project_id, Some(&id), "request", Some(prompt.trim()))?;
        self.store.task(&id)
    }

    /// Planning usage plus every task's usage.
    pub fn project_usage(&self, project_id: &str) -> Result<UsageTotals> {
        let mut total = self.store.planning_usage(project_id)?;
        for task in self.store.tasks(project_id)? {
            if let Some(state) = self.store.load_task_state(&task.id)? {
                total.add(&state.usage);
            }
        }
        Ok(total)
    }

    // -- planning -----------------------------------------------------------

    /// Asks the Planner to break the project into tasks; the control plane validates the plan
    /// and stores it as pending. Nothing runs until the Owner approves it.
    pub async fn plan(&self, project_id: &str, note: Option<&str>) -> Result<TaskPlan> {
        let project = self.store.project(project_id)?;
        anyhow::ensure!(
            project.status == ProjectStatus::Planning,
            "project {project_id} already has an approved plan"
        );
        let planner = self
            .orchestrator
            .planner()
            .context("no active planner in the registry")?;
        let registry = self
            .orchestrator
            .registry()
            .context("project planning needs an orchestrator built from the registry")?;
        let team = candidates(registry);

        let outcome = self
            .plan_attempts(project_id, &project.config, planner, &team, note)
            .await;
        self.store
            .add_planning_usage(project_id, &self.orchestrator.take_usage())?;
        outcome
    }

    async fn plan_attempts(
        &self,
        project_id: &str,
        config: &ProjectConfig,
        planner: &crate::planner::Planner,
        team: &[crate::planner::Candidate],
        note: Option<&str>,
    ) -> Result<TaskPlan> {
        let mut rejection: Option<String> = None;
        for attempt in 1..=MAX_TASK_PLAN_ATTEMPTS {
            let proposal = planner
                .propose_tasks(config, team, note, rejection.as_deref())
                .await
                .context("Planner failed")?;
            let error = match proposal {
                Ok(plan) => match validate_plan(&plan) {
                    Ok(_) => {
                        self.store.save_plan_proposal(project_id, &plan, note)?;
                        return Ok(plan);
                    }
                    Err(err) => format!("{err:#}"),
                },
                Err(invalid) => invalid,
            };
            tracing::warn!(attempt, %error, "task plan rejected by the control plane");
            rejection = Some(error);
        }

        bail!(
            "no valid task plan after {MAX_TASK_PLAN_ATTEMPTS} attempts; last rejection: {}",
            rejection.unwrap_or_default()
        )
    }

    /// Owner approval of the pending plan: creates the milestones and tasks.
    pub fn approve(&self, project_id: &str, note: Option<&str>) -> Result<Vec<TaskRecord>> {
        let project = self.store.project(project_id)?;
        anyhow::ensure!(
            project.status == ProjectStatus::Planning,
            "project {project_id} already has an approved plan"
        );
        let plan = self
            .store
            .pending_plan(project_id)?
            .context("no proposed plan; run `project plan` first")?;
        let ordered = validate_plan(&plan)?;

        self.store
            .approve_plan(project_id, &plan, &ordered, project.config.max_iterations)?;
        self.store
            .add_owner_decision(project_id, None, "approve_plan", note)?;
        self.store.tasks(project_id)
    }

    // -- execution ----------------------------------------------------------

    /// Works through ready tasks one at a time until none is left (or `max_tasks` ran).
    /// Interrupted tasks are resumed first.
    pub async fn run(&self, project_id: &str, max_tasks: Option<usize>) -> Result<RunSummary> {
        let project = self.store.project(project_id)?;
        anyhow::ensure!(
            project.status != ProjectStatus::Planning,
            "project {project_id} has no approved plan; use `project plan` and `project approve`"
        );

        let mut executed = Vec::new();
        let mut reports = Vec::new();
        let mut push_errors = Vec::new();
        let mut budget_stop = None;
        while max_tasks.is_none_or(|max| executed.len() < max) {
            let Some(task) = self.next_task(project_id)? else {
                break;
            };
            if let Some(budget) = &project.config.budget
                && let Some(reason) = budget.exceeded(&self.project_usage(project_id)?)
            {
                budget_stop = Some(format!("{reason}; {} not started", task.id));
                break;
            }
            self.store
                .set_project_status(project_id, ProjectStatus::InProgress)?;
            let (status, errors) = self.execute(&project.config, &task).await?;
            executed.push((task.id.clone(), status));
            push_errors.extend(errors);
            reports.extend(self.write_milestone_reports(&project.config)?);
        }

        self.refresh_blocked(project_id)?;
        Ok(RunSummary {
            executed,
            reports,
            push_errors,
            budget_stop,
            project_status: self.update_project_status(project_id)?,
        })
    }

    /// Recomputes BLOCKED, then picks an interrupted task, else the earliest PLANNED task
    /// whose dependencies are all DONE.
    fn next_task(&self, project_id: &str) -> Result<Option<TaskRecord>> {
        self.refresh_blocked(project_id)?;
        let tasks = self.store.tasks(project_id)?;

        if let Some(task) = tasks.iter().find(|task| task.status.is_interrupted()) {
            return Ok(Some(task.clone()));
        }
        let status: HashMap<&str, TaskStatus> = tasks
            .iter()
            .map(|task| (task.id.as_str(), task.status))
            .collect();
        Ok(tasks
            .iter()
            .find(|task| {
                task.status == TaskStatus::Planned
                    && task.depends_on.iter().all(|dependency| {
                        status.get(dependency.as_str()) == Some(&TaskStatus::Done)
                    })
            })
            .cloned())
    }

    /// A waiting task is BLOCKED while any dependency is stopped, and PLANNED again once it
    /// is not. Tasks are in dependency order, so one pass propagates blocking downstream.
    fn refresh_blocked(&self, project_id: &str) -> Result<()> {
        let tasks = self.store.tasks(project_id)?;
        let mut status: HashMap<String, TaskStatus> = tasks
            .iter()
            .map(|task| (task.id.clone(), task.status))
            .collect();

        for task in &tasks {
            if !matches!(task.status, TaskStatus::Planned | TaskStatus::Blocked) {
                continue;
            }
            let blocked = task.depends_on.iter().any(|dependency| {
                status
                    .get(dependency)
                    .is_some_and(|s| s.blocks_dependents())
            });
            let next = if blocked {
                TaskStatus::Blocked
            } else {
                TaskStatus::Planned
            };
            if next != task.status {
                self.store.set_task_status(&task.id, next)?;
                status.insert(task.id.clone(), next);
            }
        }
        Ok(())
    }

    /// Runs one task on its branch. Returns its final status and any failed pushes.
    async fn execute(
        &self,
        project: &ProjectConfig,
        task: &TaskRecord,
    ) -> Result<(TaskStatus, Vec<String>)> {
        let task_text = self.compose_task(project, task)?;

        // The task runs under its own iteration budget and is judged on its own criteria.
        let mut config = project.clone();
        config.max_iterations = task.iteration_budget;
        config.acceptance_criteria = task.acceptance_criteria.clone();

        let workspace = self.repository(project)?;
        let conflicts = workspace.start_task(&task.id)?;
        let workbench = TaskWorkbench::new(
            workspace.clone(),
            task.id.clone(),
            project.test_command.clone(),
            Duration::from_secs(project.test_timeout_secs),
        )
        .with_conflicts(conflicts.clone());

        let store = self.store;
        let task_id = task.id.as_str();
        let saved = self.store.load_task_state(&task.id)?;
        let progress = self.progress;
        let printed = std::sync::atomic::AtomicUsize::new(
            saved.as_ref().map_or(0, |state| state.history.len()),
        );
        let show_new_steps = |state: &TeamState| {
            if progress {
                let from = printed.swap(state.history.len(), std::sync::atomic::Ordering::SeqCst);
                for line in state.history.iter().skip(from) {
                    println!("  [{task_id}] {line}");
                }
            }
        };
        let checkpoint = |state: &TeamState| {
            show_new_steps(state);
            store.save_task_state(task_id, status_for(&state.stage), state)
        };

        let mut result = match saved {
            Some(mut state) => {
                state.task = task_text;
                if !conflicts.is_empty() {
                    state.history.push(format!(
                        "platform: merged main into the task branch; conflicts for the Builder: {}",
                        conflicts.join(", ")
                    ));
                }
                self.orchestrator
                    .resume(&config, state, &checkpoint, Some(&workbench))
                    .await
            }
            None => {
                self.orchestrator
                    .start(
                        &config,
                        task.id.clone(),
                        task_text,
                        &checkpoint,
                        Some(&workbench),
                    )
                    .await
            }
        };

        let mut status = match (&result.error, &result.state.stage) {
            (None, Stage::Done) => TaskStatus::Done,
            (None, Stage::HumanReviewRequired) => TaskStatus::HumanReviewRequired,
            _ => TaskStatus::Failed,
        };

        // Approval gate 3: git merges the task branch into main, or the task stops.
        let mut branches = vec![crate::workspace::task_branch(&task.id)];
        if status == TaskStatus::Done {
            let reviewer = result
                .state
                .team
                .as_ref()
                .map_or("the Reviewer", |team| team.reviewer.as_str())
                .to_string();
            let message = format!(
                "Merge {}: {}\n\nApproved by {reviewer} after {} iteration(s).",
                task.id, task.title, result.state.iteration
            );
            match workspace.merge_task(&task.id, &message) {
                Ok(commit) => {
                    result.state.history.push(format!(
                        "platform: merged {} into main ({commit})",
                        crate::workspace::task_branch(&task.id)
                    ));
                    self.store.mark_merged(&task.id)?;
                    branches.push(crate::workspace::MAIN_BRANCH.to_string());
                }
                Err(err) => {
                    status = TaskStatus::Failed;
                    result.state.stage = Stage::Failed;
                    result.state.error = Some(format!("{err:#}"));
                    result
                        .state
                        .history
                        .push("platform: merge FAILED".to_string());
                }
            }
        }
        workspace.checkout_main()?;
        let push_errors = self.push(project, &workspace, &branches);
        for error in &push_errors {
            result.state.history.push(format!("platform: {error}"));
        }
        show_new_steps(&result.state);
        self.store
            .save_task_state(&task.id, status, &result.state)?;
        if !result.state.run_id.is_empty() {
            self.store.record_run(
                &result.state.run_id,
                &task.id,
                status.as_str(),
                &result.state.audit_file,
            )?;
        }
        if let Some(err) = &result.error {
            tracing::warn!(task = %task.id, error = %format!("{err:#}"), "task failed");
        }
        Ok((status, push_errors))
    }

    /// The task text every agent sees: the task itself, project-level context, the approved
    /// work of its dependencies and the Owner's instructions.
    fn compose_task(&self, project: &ProjectConfig, task: &TaskRecord) -> Result<String> {
        let mut text = format!("TASK {}: {}\n\n{}\n", task.id, task.title, task.description);

        let approved = self
            .store
            .tasks(&task.project_id)?
            .into_iter()
            .any(|other| other.id != task.id && other.status == TaskStatus::Done);
        if approved {
            text.push_str(
                "\nCONTEXT: the project already holds work the Owner approved (its current files). \
                 This task changes or extends that work; keep everything it does not mention. \
                 The project OBJECTIVE is the Owner's first request, not a description of this \
                 task.\n",
            );
        }

        if !project.acceptance_criteria.is_empty() {
            text.push_str(
                "\nPROJECT-LEVEL CRITERIA (context; this task covers only part of them):\n",
            );
            for criterion in &project.acceptance_criteria {
                let _ = writeln!(text, "- {criterion}");
            }
        }

        if !task.depends_on.is_empty() {
            text.push_str("\nCOMPLETED DEPENDENCIES (approved work this task builds on):\n");
            for dependency_id in &task.depends_on {
                let dependency = self.store.task(dependency_id)?;
                let content = match &dependency.impl_hash {
                    Some(hash) => self.store.artifact(hash)?,
                    None => "(no implementation recorded)".to_string(),
                };
                let shown: String = content.chars().take(DEPENDENCY_CONTEXT_CHARS).collect();
                let cut = shown.len() < content.len();
                let _ = writeln!(
                    text,
                    "\n### {}: {}\n{shown}{}",
                    dependency.id,
                    dependency.title,
                    if cut { "\n[... truncated ...]" } else { "" }
                );
            }
        }

        let notes: Vec<String> = self
            .store
            .owner_decisions(&task.project_id)?
            .into_iter()
            .filter(|decision| {
                decision.task_id.as_deref() == Some(task.id.as_str())
                    && decision.decision == "resume"
            })
            .filter_map(|decision| decision.note)
            .collect();
        if !notes.is_empty() {
            text.push_str("\nOWNER INSTRUCTIONS:\n");
            for note in notes {
                let _ = writeln!(text, "- {note}");
            }
        }
        Ok(text)
    }

    // -- deletion -------------------------------------------------------------

    /// Permanently deletes a project: its records and statistics, its git workspace, its
    /// milestone reports, the audit trails of its runs and the artifacts nothing else uses.
    /// The Owner's remote repository, if any, is not touched. Irreversible.
    pub fn delete_project(&self, project_id: &str) -> Result<ProjectDeletion> {
        let workspace = self.main_workspace(project_id);
        let deleted = self.store.delete_project(project_id)?;
        let base = self
            .store
            .root()
            .parent()
            .map(PathBuf::from)
            .unwrap_or_default();
        // Stored paths are relative to the folder the CLI ran in: try it, then the data's parent.
        let remove = |path: &PathBuf| {
            [path.clone(), base.join(path)]
                .iter()
                .any(|candidate| candidate.is_file() && fs::remove_file(candidate).is_ok())
        };
        let audit_files = deleted
            .audit_files
            .iter()
            .filter(|path| remove(path))
            .count();
        let mut reports = deleted.reports.iter().filter(|path| remove(path)).count();
        // Reports written before a path was recorded follow the `<project>-M<n>.md` pattern.
        if let Ok(entries) = fs::read_dir(self.store.root().join("reports")) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with(&format!("{project_id}-M"))
                    && name.ends_with(".md")
                    && fs::remove_file(entry.path()).is_ok()
                {
                    reports += 1;
                }
            }
        }
        let workspace_removed = workspace.is_dir();
        if workspace_removed {
            remove_tree(&workspace)
                .with_context(|| format!("cannot delete {}", workspace.display()))?;
        }
        Ok(ProjectDeletion {
            project_id: project_id.to_string(),
            workspace_removed,
            audit_files,
            reports,
            artifacts: deleted.artifacts_removed,
        })
    }

    // -- owner questions ----------------------------------------------------

    /// Answers the Owner's question about a project from its files and tasks. Read-only: no
    /// task is created, nothing is committed and no file changes.
    pub async fn ask(&self, project_id: &str, question: &str) -> Result<String> {
        self.ask_with(Some(project_id), question, &[]).await
    }

    /// A question with documents attached for this question only (`(name, text)`): they are
    /// put in the prompt, never saved. Without a project the answer comes from the documents
    /// alone.
    pub async fn ask_with(
        &self,
        project_id: Option<&str>,
        question: &str,
        attachments: &[(String, String)],
    ) -> Result<String> {
        anyhow::ensure!(!question.trim().is_empty(), "the question is empty");
        anyhow::ensure!(
            project_id.is_some() || !attachments.is_empty(),
            "choose a project or attach a document"
        );
        let mut documents = String::new();
        let mut left = ATTACHMENT_CHARS;
        for (name, text) in attachments {
            let shown: String = text.chars().take(left).collect();
            left -= shown.chars().count();
            let cut = shown.chars().count() < text.chars().count();
            let _ = write!(
                documents,
                "\n--- {name}{}:\n{shown}\n",
                if cut { " (truncated)" } else { "" }
            );
        }
        let documents = if documents.is_empty() {
            String::new()
        } else {
            format!("\n\nDOCUMENTS THE OWNER ATTACHED TO THIS QUESTION:{documents}")
        };

        let Some(project_id) = project_id else {
            let prompt = format!("{documents}\n\nOWNER'S QUESTION:\n{question}\n");
            let answer = self
                .orchestrator
                .consultant()?
                .complete(CONSULTANT_PROMPT, prompt.trim_start())
                .await;
            self.orchestrator.take_usage(); // no project to bill
            return answer;
        };
        let project = self.store.project(project_id)?.config;
        let files = self
            .repository(&project)?
            .snapshot(QUESTION_SNAPSHOT_CHARS)?;
        let mut tasks = String::new();
        for task in self.store.tasks(project_id)? {
            let _ = writeln!(tasks, "- {} [{}] {}", task.id, task.status, task.title);
        }
        let prompt = format!(
            "PROJECT: {}\n\nOBJECTIVE (the Owner's first request):\n{}\n\nTASKS:\n{}\n\
             TEST COMMAND (run from the project folder): {}\n\nPROJECT FILES:\n{files}{documents}\n\n\
             OWNER'S QUESTION:\n{question}\n",
            project.name,
            project.objective,
            if tasks.is_empty() { "- none\n" } else { &tasks },
            project.test_command.as_deref().unwrap_or("none configured"),
        );
        let answer = self
            .orchestrator
            .consultant()?
            .complete(CONSULTANT_PROMPT, &prompt)
            .await;
        self.store
            .add_planning_usage(project_id, &self.orchestrator.take_usage())?;
        answer
    }

    // -- owner decisions ----------------------------------------------------

    /// Sends a stopped task back to work with extra iterations and an optional instruction.
    /// The next `run` continues it from its saved state.
    pub fn resume_task(
        &self,
        task_id: &str,
        note: Option<&str>,
        extra_iterations: u32,
    ) -> Result<TaskRecord> {
        let task = self.store.task(task_id)?;
        anyhow::ensure!(
            task.status.needs_owner(),
            "task {task_id} is {}; only HUMAN_REVIEW_REQUIRED or FAILED tasks can be resumed",
            task.status
        );
        anyhow::ensure!(extra_iterations > 0, "extra iterations must be at least 1");

        self.store.add_iterations(task_id, extra_iterations)?;
        self.store
            .add_owner_decision(&task.project_id, Some(task_id), "resume", note)?;
        self.store.set_task_status(task_id, TaskStatus::Planned)?;
        self.after_decision(&task.project_id)?;
        self.store.task(task_id)
    }

    /// Owner accepts the last implementation of a stopped task as done (recorded as a manual
    /// approval, never as a Reviewer approval).
    pub fn accept_task(&self, task_id: &str, note: Option<&str>) -> Result<TaskRecord> {
        let task = self.store.task(task_id)?;
        anyhow::ensure!(
            task.status.needs_owner(),
            "task {task_id} is {}; only HUMAN_REVIEW_REQUIRED or FAILED tasks can be accepted",
            task.status
        );
        anyhow::ensure!(
            task.impl_hash.is_some(),
            "task {task_id} has no implementation to accept"
        );

        // The Owner accepts the work as it is on the task branch; git still refuses a
        // conflicting merge.
        let project = self.store.project(&task.project_id)?.config;
        let workspace = self.repository(&project)?;
        let message = format!(
            "Merge {task_id}: {}\n\nAccepted by the Owner (not approved by the Reviewer).{}",
            task.title,
            note.map(|note| format!("\nNote: {note}"))
                .unwrap_or_default()
        );
        workspace
            .merge_task(task_id, &message)
            .with_context(|| format!("cannot accept {task_id}"))?;
        self.store.mark_merged(task_id)?;
        for error in self.push(
            &project,
            &workspace,
            &[crate::workspace::MAIN_BRANCH.to_string()],
        ) {
            tracing::warn!(task = %task_id, %error, "push after acceptance failed");
        }

        self.store
            .add_owner_decision(&task.project_id, Some(task_id), "accept", note)?;
        self.store.set_task_status(task_id, TaskStatus::Done)?;
        self.after_decision(&task.project_id)?;
        self.store.task(task_id)
    }

    pub fn cancel_task(&self, task_id: &str, note: Option<&str>) -> Result<TaskRecord> {
        let task = self.store.task(task_id)?;
        anyhow::ensure!(
            !matches!(task.status, TaskStatus::Done | TaskStatus::Cancelled),
            "task {task_id} is already {}",
            task.status
        );

        self.store
            .add_owner_decision(&task.project_id, Some(task_id), "cancel", note)?;
        self.store.set_task_status(task_id, TaskStatus::Cancelled)?;
        self.after_decision(&task.project_id)?;
        self.store.task(task_id)
    }

    fn after_decision(&self, project_id: &str) -> Result<()> {
        self.refresh_blocked(project_id)?;
        let project = self.store.project(project_id)?;
        self.write_milestone_reports(&project.config)?;
        self.update_project_status(project_id)?;
        Ok(())
    }

    fn update_project_status(&self, project_id: &str) -> Result<ProjectStatus> {
        let current = self.store.project(project_id)?.status;
        if current == ProjectStatus::Planning {
            return Ok(current);
        }
        let tasks = self.store.tasks(project_id)?;
        let next = if !tasks.is_empty()
            && tasks
                .iter()
                .all(|task| matches!(task.status, TaskStatus::Done | TaskStatus::Cancelled))
        {
            ProjectStatus::Done
        } else if tasks.iter().all(|task| task.status == TaskStatus::Planned) {
            ProjectStatus::Planned
        } else {
            ProjectStatus::InProgress
        };
        if next != current {
            self.store.set_project_status(project_id, next)?;
        }
        Ok(next)
    }

    // -- reporting ----------------------------------------------------------

    /// Writes `<data>/reports/<project>-M<n>.md` for every milestone whose tasks are all done
    /// or cancelled and that has no report yet.
    fn write_milestone_reports(&self, project: &ProjectConfig) -> Result<Vec<PathBuf>> {
        let project_id = &project.project_id;
        let tasks = self.store.tasks(project_id)?;
        let decisions = self.store.owner_decisions(project_id)?;
        let dir = self.store.root().join("reports");
        let mut written = Vec::new();

        for milestone in self.store.milestones(project_id)? {
            if milestone.report_path.is_some() {
                continue;
            }
            let milestone_tasks: Vec<&TaskRecord> = tasks
                .iter()
                .filter(|task| task.milestone_id == milestone.id)
                .collect();
            if milestone_tasks.is_empty()
                || !milestone_tasks
                    .iter()
                    .all(|task| matches!(task.status, TaskStatus::Done | TaskStatus::Cancelled))
            {
                continue;
            }

            let mut report = format!(
                "# Milestone report: {} — {}\n\nProject: {} ({project_id})\n\n",
                milestone.position + 1,
                milestone.name,
                project.name
            );
            for task in milestone_tasks {
                let accepted_by_owner = decisions.iter().any(|decision| {
                    decision.task_id.as_deref() == Some(task.id.as_str())
                        && decision.decision == "accept"
                });
                let _ = writeln!(
                    report,
                    "## {}: {}\n\n- Status: {}{}",
                    task.id,
                    task.title,
                    task.status,
                    if accepted_by_owner {
                        " (accepted by the Owner, not by the Reviewer)"
                    } else {
                        ""
                    }
                );
                if let Some(state) = self.store.load_task_state(&task.id)? {
                    if let Some(team) = &state.team {
                        let _ = writeln!(
                            report,
                            "- Team: {} / {} / {}",
                            team.architect, team.builder, team.reviewer
                        );
                    }
                    let _ = writeln!(
                        report,
                        "- Iterations: {}, specification revisions: {}",
                        state.iteration, state.architecture_revisions
                    );
                    if let Some(review) = &state.review {
                        let _ = writeln!(
                            report,
                            "- Final review: {:?} — {}",
                            review.decision, review.feedback
                        );
                    }
                }
                for (label, hash) in [
                    ("Specification", &task.spec_hash),
                    ("Implementation", &task.impl_hash),
                ] {
                    if let Some(hash) = hash {
                        let _ = writeln!(
                            report,
                            "- {label}: {}",
                            self.store.artifact_path(hash).display()
                        );
                    }
                }
                for decision in decisions
                    .iter()
                    .filter(|decision| decision.task_id.as_deref() == Some(task.id.as_str()))
                {
                    let _ = writeln!(
                        report,
                        "- Owner decision: {}{}",
                        decision.decision,
                        decision
                            .note
                            .as_deref()
                            .map(|note| format!(" — {note}"))
                            .unwrap_or_default()
                    );
                }
                report.push('\n');
            }

            fs::create_dir_all(&dir)?;
            let path = dir.join(format!("{project_id}-M{}.md", milestone.position + 1));
            fs::write(&path, report)
                .with_context(|| format!("cannot write report {}", path.display()))?;
            self.store
                .mark_milestone_reported(milestone.id, &path.display().to_string())?;
            written.push(path);
        }
        Ok(written)
    }
}

/// Task status that matches where a run currently is.
fn status_for(stage: &Stage) -> TaskStatus {
    match stage {
        Stage::New => TaskStatus::Assigned,
        Stage::ArchitectureReady => TaskStatus::InProgress,
        Stage::Implemented | Stage::Reviewing => TaskStatus::Review,
        Stage::ChangesRequired | Stage::ArchitectureRevisionRequired => TaskStatus::ChangesRequired,
        Stage::Approved | Stage::Done => TaskStatus::Done,
        Stage::HumanReviewRequired => TaskStatus::HumanReviewRequired,
        Stage::Failed => TaskStatus::Failed,
    }
}
