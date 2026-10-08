//! The Owner's entry points shared by the CLI, the console and the web interface: a
//! request becomes a milestone plan. Execution waits for the Owner's approval.

use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::{
    domain::ProjectConfig,
    project::{ProjectManager, ProjectSettings, Store, TaskRecord, plan::TaskPlan},
};

const PREVIEW_LINES: usize = 150;
/// Every agent sees this preview, the Reviewer too (32k context), so it stays modest.
const PREVIEW_CHARS: usize = 12_000;

/// What every agent sees of the Owner's files: names, sizes and the start of text files.
pub fn inputs_preview(files: &[PathBuf]) -> Result<String> {
    let mut preview = String::new();
    for file in files {
        let bytes =
            std::fs::read(file).with_context(|| format!("cannot read {}", file.display()))?;
        let name = file.file_name().and_then(|n| n.to_str()).unwrap_or("file");
        preview.push_str(&format!("\n--- inputs/{name} ({} bytes)", bytes.len()));
        match String::from_utf8(bytes) {
            Ok(text) => {
                let head: String = text
                    .lines()
                    .take(PREVIEW_LINES)
                    .collect::<Vec<_>>()
                    .join("\n")
                    .chars()
                    .take(PREVIEW_CHARS)
                    .collect();
                let cut = head.len() < text.trim_end().len();
                preview.push_str(&format!(
                    ":\n{head}{}\n",
                    if cut {
                        "\n[... rest of the file is in the workspace ...]"
                    } else {
                        ""
                    }
                ));
            }
            Err(_) => preview.push_str(": binary file\n"),
        }
    }
    Ok(preview)
}

/// A project name chosen by the Owner, as an id: lowercase, digits and '-'.
pub fn normalize_project_name(name: &str) -> Result<String> {
    let id: String = name
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    anyhow::ensure!(
        !id.is_empty() && id.len() <= 40,
        "project name must have 1-40 letters or digits"
    );
    Ok(id)
}

pub fn project_id_from(prompt: &str) -> String {
    let words: Vec<String> = prompt
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|word| word.len() > 1)
        .take(4)
        .map(str::to_lowercase)
        .collect();
    let stem: String = words.join("-").chars().take(30).collect();
    let suffix = &uuid::Uuid::new_v4().simple().to_string()[..4];
    if stem.is_empty() {
        format!("request-{suffix}")
    } else {
        format!("{stem}-{suffix}")
    }
}

/// A request from the Owner. `project_id` adds it to an existing project; otherwise a new
/// project is created, named `new_project_name` or derived from the prompt.
#[derive(Debug, Clone, Default)]
pub struct NewRequest {
    pub prompt: String,
    pub files: Vec<PathBuf>,
    pub project_id: Option<String>,
    pub new_project_name: Option<String>,
    pub test_command: Option<String>,
}

/// The project a request landed in.
#[derive(Debug, Clone)]
pub struct Prepared {
    pub project_id: String,
    pub created: bool,
    /// Files committed to `inputs/` on main.
    pub inputs: Vec<String>,
    /// What every agent sees of the files (also part of a new project's objective).
    pub preview: String,
}

/// Creates the project (or checks the existing one and applies a new test command) and
/// commits the Owner's files. Starts no work.
pub fn prepare(
    store: &Store,
    manager: &ProjectManager<'_>,
    request: &NewRequest,
) -> Result<Prepared> {
    anyhow::ensure!(!request.prompt.trim().is_empty(), "the prompt is empty");
    for file in &request.files {
        anyhow::ensure!(file.is_file(), "not a file: {}", file.display());
    }
    let preview = inputs_preview(&request.files)?;
    let prompt = &request.prompt;

    // Refuse before creating anything: a failed request used to leave an empty project behind.
    anyhow::ensure!(
        request.project_id.is_some() || !manager.planner_missing(),
        "no active planner: activate the Planner (team window, or `employees set-status <ID> active`)          before starting a new project"
    );
    let (project_id, created) = match &request.project_id {
        Some(id) => {
            store.project(id)?;
            anyhow::ensure!(
                store.pending_plan(id)?.is_none(),
                "project {id} already has a pending plan; approve or revise it first"
            );
            if request.test_command.is_some() {
                manager.configure(
                    id,
                    ProjectSettings {
                        test_command: request.test_command.clone(),
                        ..ProjectSettings::default()
                    },
                )?;
            }
            (id.clone(), false)
        }
        None => {
            let id = match &request.new_project_name {
                Some(name) => {
                    let id = normalize_project_name(name)?;
                    anyhow::ensure!(
                        store.project(&id).is_err(),
                        "project {id} already exists; continue it with --project {id}"
                    );
                    id
                }
                None => project_id_from(prompt),
            };
            let mut objective = prompt.trim().to_string();
            if !preview.is_empty() {
                objective.push_str(&format!(
                    "\n\nFILES PROVIDED BY THE OWNER (copied to inputs/ in the workspace):{preview}"
                ));
            }
            let config = ProjectConfig {
                project_id: id.clone(),
                name: prompt
                    .lines()
                    .next()
                    .unwrap_or("Owner request")
                    .chars()
                    .take(60)
                    .collect(),
                objective,
                max_iterations: 3,
                max_architecture_revisions: 1,
                rules: vec![
                    "Work from the Owner's request and the provided files only.".to_string(),
                    "Do not claim tests passed: the platform runs them.".to_string(),
                ],
                acceptance_criteria: vec!["The Owner's request is fully satisfied.".to_string()],
                assigned_team: None,
                test_command: request
                    .test_command
                    .clone()
                    .filter(|command| !command.trim().is_empty()),
                test_timeout_secs: 300,
                remote_url: None,
                budget: None,
            };
            store.add_project(&config)?;
            (id, true)
        }
    };

    let inputs = manager.add_inputs(&project_id, &request.files)?;
    Ok(Prepared {
        project_id,
        created,
        inputs,
        preview,
    })
}

/// All public request entry points propose milestones before starting implementation.
pub async fn plan_request(
    manager: &ProjectManager<'_>,
    request: &NewRequest,
    prepared: &Prepared,
) -> Result<TaskPlan> {
    let mut scope = request.prompt.trim().to_string();
    if !prepared.inputs.is_empty() {
        scope.push_str(&format!(
            "\n\nOWNER INPUT FILES: {}\n{}",
            prepared.inputs.join(", "),
            prepared.preview
        ));
    }
    manager.plan(&prepared.project_id, Some(&scope)).await
}

/// Adds the request as a task ("Owner requests" milestone, no plan approval needed). An
/// existing project gets the file preview in the task itself.
pub fn add_request_task(
    store: &Store,
    manager: &ProjectManager<'_>,
    request: &NewRequest,
    prepared: &Prepared,
) -> Result<TaskRecord> {
    let text = if prepared.inputs.is_empty()
        || store
            .project(&prepared.project_id)?
            .config
            .objective
            .contains(&prepared.preview)
    {
        request.prompt.clone()
    } else {
        format!(
            "{}\n\nFILES PROVIDED BY THE OWNER:{}",
            request.prompt.trim(),
            prepared.preview
        )
    };
    manager.add_request(&prepared.project_id, &text, &prepared.inputs)
}
