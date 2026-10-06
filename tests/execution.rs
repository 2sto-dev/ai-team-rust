//! Phase 4: files written into task workspaces, the Owner's test command as a gate before
//! review, conflict-checked merges into the main workspace, and usage metering.

mod common;

use std::{fs, path::PathBuf, sync::Arc};

use ai_team::{
    domain::{ProjectConfig, Role},
    llm::{LlmProvider, ProviderFactory},
    orchestrator::Orchestrator,
    project::{ProjectManager, Store, TaskStatus},
    registry::Registry,
};
use common::{ScriptedProvider, TestLlm, edit_contract, project, sample_company, test_providers};

const APPROVE: &str = r#"{"decision":"APPROVED","feedback":"looks good"}"#;

struct Env {
    _dir: tempfile::TempDir,
    root: PathBuf,
    company: PathBuf,
    store: Store,
}

fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let company = sample_company(&root);
    let store = Store::open(root.join("data")).unwrap();
    Env {
        _dir: dir,
        root,
        company,
        store,
    }
}

impl Env {
    fn orchestrator(&self, providers: ProviderFactory) -> Orchestrator {
        let registry = Registry::load(self.company.join("employees")).unwrap();
        Orchestrator::from_registry_with_providers(registry, providers)
            .unwrap()
            .with_audit_dir(self.root.join("audit"))
    }

    fn main_file(&self, relative: &str) -> PathBuf {
        self.root.join("data/workspaces/app/main").join(relative)
    }

    /// Adds, plans (test planner: T01 -> T02 -> T03) and approves the project.
    async fn planned(&self, config: ProjectConfig) {
        let orchestrator = self.orchestrator(test_providers());
        let manager = ProjectManager::new(&self.store, &orchestrator);
        self.store.add_project(&config).unwrap();
        manager.plan("app", None).await.unwrap();
        manager.approve("app", None).unwrap();
    }
}

fn config(max_iterations: u32, test_command: Option<&str>) -> ProjectConfig {
    let mut config = project(max_iterations);
    config.project_id = "app".to_string();
    config.test_command = test_command.map(str::to_string);
    config
}

/// Exits 0 only when `notes/result.md` exists in the workspace.
fn file_exists_command() -> &'static str {
    if cfg!(windows) {
        r"if exist notes\result.md (exit 0) else (exit 1)"
    } else {
        "test -f notes/result.md"
    }
}

/// TestLlm for every role except the ones given as scripted providers.
fn providers(
    builder: Option<ScriptedProvider>,
    reviewer: Option<ScriptedProvider>,
) -> ProviderFactory {
    Arc::new(move |role, config| {
        let scripted = match role {
            Role::Builder => builder.clone(),
            Role::Reviewer => reviewer.clone(),
            _ => None,
        };
        Ok(match scripted {
            Some(provider) => Arc::new(provider) as Arc<dyn LlmProvider>,
            None => Arc::new(TestLlm::new(role, config.model.clone())),
        })
    })
}

fn file_answer(path: &str, content: &str) -> String {
    format!("Implementation.\n\n### FILE: {path}\n```\n{content}\n```\n")
}

#[tokio::test]
async fn approved_work_is_written_tested_and_merged() {
    let env = env();
    env.planned(config(3, Some(file_exists_command()))).await;
    let orchestrator = env.orchestrator(test_providers());
    let manager = ProjectManager::new(&env.store, &orchestrator);

    let summary = manager.run("app", Some(1)).await.unwrap();
    assert_eq!(
        summary.executed,
        [("app-T01".to_string(), TaskStatus::Done)]
    );

    let state = env.store.load_task_state("app-T01").unwrap().unwrap();
    let verification = state.verification.as_ref().unwrap();
    assert!(verification.passed());
    assert!(
        verification.test.as_ref().unwrap().passed,
        "the Owner's command ran and passed"
    );
    assert_eq!(state.written_files, ["notes/result.md"]);
    assert!(
        state
            .history
            .iter()
            .any(|item| item.contains("verification passed"))
    );
    assert!(
        state
            .history
            .iter()
            .any(|item| item.contains("merged 1 file(s)"))
    );

    // The approved version (second Builder answer) is now in the main workspace.
    assert_eq!(
        fs::read_to_string(env.main_file("notes/result.md")).unwrap(),
        "call 2\n"
    );

    // Every model call of the task was metered.
    assert!(state.usage.calls >= 4, "{:?}", state.usage);
    assert!(state.usage.input_tokens > 0 && state.usage.output_tokens > 0);
    assert!(
        manager.project_usage("app").unwrap().calls > state.usage.calls,
        "planning counted"
    );
}

#[tokio::test]
async fn failing_tests_never_reach_the_reviewer() {
    let env = env();
    env.planned(config(2, Some("exit 1"))).await;
    let builder = ScriptedProvider::new([file_answer("a.txt", "v1"), file_answer("a.txt", "v2")]);
    // An empty reviewer queue fails the run if the Reviewer is ever asked.
    let reviewer = ScriptedProvider::new(Vec::<String>::new());
    let orchestrator = env.orchestrator(providers(Some(builder.clone()), Some(reviewer.clone())));

    let summary = ProjectManager::new(&env.store, &orchestrator)
        .run("app", Some(1))
        .await
        .unwrap();

    assert_eq!(
        summary.executed,
        [("app-T01".to_string(), TaskStatus::HumanReviewRequired)]
    );
    assert!(
        reviewer.prompts().is_empty(),
        "failed work is not sent to review"
    );
    let second = &builder.prompts()[1];
    assert!(second.contains("LAST AUTOMATED VERIFICATION"));
    assert!(second.contains("FAILED (exit code 1)"));
    assert!(
        !env.main_file("a.txt").exists(),
        "nothing merges from a stopped task"
    );
}

#[tokio::test]
async fn reviewer_gets_the_platform_test_evidence() {
    let env = env();
    env.planned(config(2, Some(file_exists_command()))).await;
    let builder = ScriptedProvider::new([file_answer("notes/result.md", "done")]);
    let reviewer = ScriptedProvider::new([APPROVE]);
    let orchestrator = env.orchestrator(providers(Some(builder.clone()), Some(reviewer.clone())));

    ProjectManager::new(&env.store, &orchestrator)
        .run("app", Some(1))
        .await
        .unwrap();

    let review_prompt = &reviewer.prompts()[0];
    assert!(review_prompt.contains("AUTOMATED VERIFICATION (performed by the platform"));
    assert!(review_prompt.contains("-> PASSED"));
    let briefing = &builder.prompts()[0];
    assert!(
        briefing.contains("### FILE: relative/path"),
        "the Builder is told the format"
    );
    assert!(
        briefing.contains(file_exists_command()),
        "and which command will run"
    );
}

#[tokio::test]
async fn answers_without_valid_files_are_rejected() {
    let env = env();
    env.planned(config(2, None)).await;
    let builder = ScriptedProvider::new([
        "I would write some code here.".to_string(),
        file_answer("../escape.txt", "x"),
    ]);
    let reviewer = ScriptedProvider::new(Vec::<String>::new());
    let orchestrator = env.orchestrator(providers(Some(builder), Some(reviewer)));

    let summary = ProjectManager::new(&env.store, &orchestrator)
        .run("app", Some(1))
        .await
        .unwrap();

    assert_eq!(summary.executed[0].1, TaskStatus::HumanReviewRequired);
    let state = env.store.load_task_state("app-T01").unwrap().unwrap();
    let problems = state.verification.unwrap().problems.join(" | ");
    assert!(problems.contains("not allowed"), "{problems}");
    assert!(
        !env.root
            .join("data/workspaces/app/tasks/escape.txt")
            .exists()
    );
    assert!(!env.root.join("data/workspaces/app/escape.txt").exists());
}

#[tokio::test]
async fn owner_acceptance_still_refuses_a_conflicting_merge() {
    let env = env();
    env.planned(config(1, None)).await; // TestLlm's reviewer rejects its first review
    let orchestrator = env.orchestrator(test_providers());
    let manager = ProjectManager::new(&env.store, &orchestrator);
    manager.run("app", Some(1)).await.unwrap();
    assert_eq!(
        env.store.task("app-T01").unwrap().status,
        TaskStatus::HumanReviewRequired
    );

    // Someone changed the same file in the main workspace meanwhile.
    fs::create_dir_all(env.main_file("notes")).unwrap();
    fs::write(env.main_file("notes/result.md"), "edited by hand").unwrap();

    let err = manager.accept_task("app-T01", None).unwrap_err();
    assert!(format!("{err:#}").contains("merge conflict"), "{err:#}");
    assert_eq!(
        env.store.task("app-T01").unwrap().status,
        TaskStatus::HumanReviewRequired,
        "a refused acceptance changes nothing"
    );
    assert_eq!(
        fs::read_to_string(env.main_file("notes/result.md")).unwrap(),
        "edited by hand"
    );
}

#[tokio::test]
async fn builder_without_workspace_permission_cannot_act() {
    let env = env();
    edit_contract(&env.company, "EMP-BUILD-001", "  - write_workspace\n", "");
    env.planned(config(2, None)).await;
    let orchestrator = env.orchestrator(test_providers());

    let summary = ProjectManager::new(&env.store, &orchestrator)
        .run("app", Some(1))
        .await
        .unwrap();

    assert_eq!(summary.executed[0].1, TaskStatus::Failed);
    let error = env.store.task("app-T01").unwrap().error.unwrap();
    assert!(error.contains("lacks write_workspace"), "{error}");
}

#[tokio::test]
async fn owner_can_change_the_test_command_later() {
    let env = env();
    env.planned(config(2, None)).await;
    let orchestrator = env.orchestrator(test_providers());
    let manager = ProjectManager::new(&env.store, &orchestrator);

    let updated = manager
        .configure("app", Some("cargo test".to_string()), Some(120))
        .unwrap();
    assert_eq!(updated.test_command.as_deref(), Some("cargo test"));
    assert_eq!(
        env.store.project("app").unwrap().config.test_timeout_secs,
        120
    );

    manager.configure("app", Some(String::new()), None).unwrap();
    assert_eq!(env.store.project("app").unwrap().config.test_command, None);
    let decisions = env.store.owner_decisions("app").unwrap();
    assert_eq!(
        decisions
            .iter()
            .filter(|d| d.decision == "configure")
            .count(),
        2
    );
}
