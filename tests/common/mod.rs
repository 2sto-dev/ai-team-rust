#![allow(dead_code)]

use std::{
    collections::VecDeque,
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU32, Ordering},
    },
};

use ai_team::{
    BoxFuture,
    config::{ModelConfig, ProviderKind},
    domain::{ProjectConfig, Role},
    llm::{Completion, LlmProvider, ProviderFactory, Usage},
    planner::TASK_PLAN_HEADING,
};
use anyhow::{Result, anyhow};
use serde_json::Value;

pub fn project(max_iterations: u32) -> ProjectConfig {
    ProjectConfig {
        project_id: "test".to_string(),
        name: "Test Project".to_string(),
        objective: "Test the reusable team workflow.".to_string(),
        max_iterations,
        max_architecture_revisions: 1,
        rules: vec!["Keep contracts explicit.".to_string()],
        acceptance_criteria: vec!["Reviewer must approve.".to_string()],
        assigned_team: None,
        test_command: None,
        test_timeout_secs: 60,
        remote_url: None,
    }
}

/// An HTTP model config with short timeouts and no retries (tests override the provider).
pub fn fast_model(model: &str) -> ModelConfig {
    let mut config = ModelConfig::new(ProviderKind::Openai, model);
    config.timeout_secs = 5;
    config.max_retries = 0;
    config.retry_backoff_ms = 10;
    config
}

// ---------------------------------------------------------------------------
// Company fixtures
// ---------------------------------------------------------------------------

/// Base URL written into test contracts; nothing listens there, because orchestrators in
/// tests build their agents with [`test_providers`].
pub const UNUSED_BASE_URL: &str = "http://127.0.0.1:9";

/// Contract YAML for a test employee; `model: None` means no model block (system employee).
pub fn contract_yaml(
    id: &str,
    employee_type: &str,
    function: &str,
    manager: &str,
    permissions: &[&str],
    model: Option<&str>,
) -> String {
    let mut yaml = format!(
        "# test contract\n\
         employee_id: {id}\n\
         name: {function} {id}\n\
         title: Test {function}\n\
         department: test\n\
         manager_id: {manager}\n\
         employee_type: {employee_type}\n\
         function: {function}\n\
         status: active\n"
    );
    if let Some(model) = model {
        yaml.push_str(&format!(
            "model:\n  provider: ollama\n  model: {model}\n  base_url: {UNUSED_BASE_URL}\n"
        ));
    }
    yaml.push_str("skills:\n  - testing\n");
    if permissions.is_empty() {
        yaml.push_str("permissions: []\n");
    } else {
        yaml.push_str("permissions:\n");
        for permission in permissions {
            yaml.push_str(&format!("  - {permission}\n"));
        }
    }
    yaml
}

pub fn write_employee(employees: &Path, folder: &str, contract: &str) {
    let dir = employees.join(folder);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("contract.yaml"), contract).unwrap();
    fs::write(
        dir.join("job_description.md"),
        format!("# {folder}\nDo the job.\n"),
    )
    .unwrap();
}

/// A valid company with one employee per function. Returns the company folder
/// (containing `employees/`).
pub fn sample_company(root: &Path) -> PathBuf {
    let company = root.join("company");
    let employees = company.join("employees");
    write_employee(
        &employees,
        "EMP-ORCH-001",
        &contract_yaml("EMP-ORCH-001", "system", "orchestrator", "OWNER", &[], None),
    );
    write_employee(
        &employees,
        "EMP-PLAN-001",
        &contract_yaml(
            "EMP-PLAN-001",
            "agent",
            "planner",
            "EMP-ORCH-001",
            &["propose_plan"],
            Some("test-planner"),
        ),
    );
    write_employee(
        &employees,
        "EMP-ARCH-001",
        &contract_yaml(
            "EMP-ARCH-001",
            "agent",
            "architect",
            "EMP-ORCH-001",
            &["write_specification", "delegate_subtasks"],
            Some("test-architect"),
        ),
    );
    write_employee(
        &employees,
        "EMP-BUILD-001",
        &contract_yaml(
            "EMP-BUILD-001",
            "agent",
            "builder",
            "EMP-ORCH-001",
            &[
                "write_implementation",
                "delegate_subtasks",
                "write_workspace",
                "run_tests",
            ],
            Some("test-builder"),
        ),
    );
    write_employee(
        &employees,
        "EMP-REV-001",
        &contract_yaml(
            "EMP-REV-001",
            "agent",
            "reviewer",
            "EMP-ORCH-001",
            &["review_work", "delegate_subtasks"],
            Some("test-reviewer"),
        ),
    );
    company
}

/// Replaces text inside an employee's contract (panics if the text is not there).
pub fn edit_contract(company: &Path, employee_id: &str, from: &str, to: &str) {
    let path = company
        .join("employees")
        .join(employee_id)
        .join("contract.yaml");
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.contains(from), "{employee_id}: '{from}' not found");
    fs::write(&path, text.replacen(from, to, 1)).unwrap();
}

// ---------------------------------------------------------------------------
// Offline LLMs (exist only in test code; contracts cannot name them)
// ---------------------------------------------------------------------------

/// Role-aware fake model. The Reviewer rejects its first call and approves afterwards (one
/// correction cycle per agent instance); the Planner picks the first candidate of each
/// function from the roster lines of its prompt (`- EMP-... | function | ...`).
pub struct TestLlm {
    role: Role,
    model: String,
    calls: AtomicU32,
}

impl TestLlm {
    pub fn new(role: Role, model: impl Into<String>) -> Self {
        Self {
            role,
            model: model.into(),
            calls: AtomicU32::new(0),
        }
    }
}

fn first_candidate(prompt: &str, function: &str) -> String {
    prompt
        .lines()
        .filter_map(|line| line.strip_prefix("- "))
        .map(|line| line.split(" | ").collect::<Vec<_>>())
        .find(|fields| fields.len() > 1 && fields[1] == function)
        .map(|fields| fields[0].to_string())
        .unwrap_or_default()
}

impl LlmProvider for TestLlm {
    fn model_name(&self) -> &str {
        &self.model
    }

    fn generate<'a>(
        &'a self,
        _system_prompt: &'a str,
        user_prompt: &'a str,
    ) -> BoxFuture<'a, Result<Completion>> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        let answer = match self.role {
            Role::Planner if user_prompt.contains(TASK_PLAN_HEADING) => {
                test_task_plan().to_string()
            }
            Role::Planner => serde_json::json!({
                "team": {
                    "architect": first_candidate(user_prompt, "architect"),
                    "builder": first_candidate(user_prompt, "builder"),
                    "reviewer": first_candidate(user_prompt, "reviewer"),
                },
                "rationale": "test planner: first candidate of each function"
            })
            .to_string(),
            Role::Architect => "# Specification (test)\n- explicit contracts".to_string(),
            Role::Builder => format!(
                "# Implementation (test, call {call})

### FILE: notes/result.md
```
call {call}
```
"
            ),
            Role::Reviewer if call == 1 => serde_json::json!({
                "decision": "CHANGES_REQUIRED",
                "target": "builder",
                "feedback": "test gate: add tests for every contract"
            })
            .to_string(),
            Role::Reviewer => {
                serde_json::json!({ "decision": "APPROVED", "feedback": "test gate passed" })
                    .to_string()
            }
        };
        let usage = Usage {
            input_tokens: (user_prompt.len() / 4) as u64,
            output_tokens: (answer.len() / 4) as u64,
        };
        Box::pin(async move {
            Ok(Completion {
                text: answer,
                usage: Some(usage),
            })
        })
    }
}

/// The task plan `TestLlm` proposes: T1 -> T2 in milestone "Core", T3 (after T2) in "Extras".
pub fn test_task_plan() -> Value {
    serde_json::json!({
        "milestones": [
            {"name": "Core", "tasks": [
                {"key": "T1", "title": "Data model", "description": "Define the data model.",
                 "acceptance_criteria": ["model documented"], "depends_on": []},
                {"key": "T2", "title": "API", "description": "Expose the API.",
                 "acceptance_criteria": ["endpoints listed"], "depends_on": ["T1"]}
            ]},
            {"name": "Extras", "tasks": [
                {"key": "T3", "title": "Docs", "description": "Write the docs.",
                 "acceptance_criteria": ["readme exists"], "depends_on": ["T2"]}
            ]}
        ],
        "rationale": "test plan"
    })
}

/// Provider factory for orchestrators in tests: a fresh `TestLlm` per agent, no network.
pub fn test_providers() -> ProviderFactory {
    Arc::new(|role, config| Ok(Arc::new(TestLlm::new(role, config.model.clone()))))
}

// ---------------------------------------------------------------------------
// Scripted LLM
// ---------------------------------------------------------------------------

/// Returns queued answers in order and remembers every prompt it received.
/// An empty queue is an error, which doubles as a "this agent fails" fixture.
#[derive(Clone, Default)]
pub struct ScriptedProvider {
    answers: Arc<Mutex<VecDeque<String>>>,
    prompts: Arc<Mutex<Vec<String>>>,
}

impl ScriptedProvider {
    pub fn new<I, S>(answers: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            answers: Arc::new(Mutex::new(answers.into_iter().map(Into::into).collect())),
            prompts: Arc::default(),
        }
    }

    pub fn prompts(&self) -> Vec<String> {
        self.prompts.lock().unwrap().clone()
    }
}

impl LlmProvider for ScriptedProvider {
    fn model_name(&self) -> &str {
        "scripted"
    }

    fn generate<'a>(
        &'a self,
        _system_prompt: &'a str,
        user_prompt: &'a str,
    ) -> BoxFuture<'a, Result<Completion>> {
        self.prompts.lock().unwrap().push(user_prompt.to_string());
        let next = self.answers.lock().unwrap().pop_front();
        Box::pin(async move {
            next.map(|text| Completion { text, usage: None })
                .ok_or_else(|| anyhow!("scripted provider has no answer left"))
        })
    }
}

// ---------------------------------------------------------------------------
// Audit helpers
// ---------------------------------------------------------------------------

pub fn audit_events(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

pub fn single_audit_file(dir: &Path) -> PathBuf {
    let mut files: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(files.len(), 1, "expected exactly one audit file");
    files.pop().unwrap()
}
