//! Phase 4: answers committed on task branches of the project's git repository, the Owner's
//! test command as a gate before review, git merges into main, automatic pushes to the
//! Owner's remote, and usage metering.

mod common;

use std::{fs, path::PathBuf, process::Command, sync::Arc};

use ai_team::{
    domain::{ProjectConfig, Role},
    llm::{LlmProvider, ProviderFactory},
    orchestrator::Orchestrator,
    project::{ProjectManager, ProjectSettings, ProjectStatus, Store, TaskStatus},
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

    fn repo(&self) -> PathBuf {
        self.root.join("data/workspaces/app")
    }

    /// A file in the repository's working tree (on `main` between tasks).
    fn main_file(&self, relative: &str) -> PathBuf {
        self.repo().join(relative)
    }

    fn git(&self, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(self.repo())
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
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
            .any(|item| item.contains("merged task/app-T01 into main"))
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
    assert!(!env.root.join("data/workspaces/escape.txt").exists());
    assert!(!env.root.join("data/escape.txt").exists());
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
        .configure(
            "app",
            ProjectSettings {
                test_command: Some("cargo test".to_string()),
                test_timeout_secs: Some(120),
                ..ProjectSettings::default()
            },
        )
        .unwrap();
    assert_eq!(updated.test_command.as_deref(), Some("cargo test"));
    assert_eq!(
        env.store.project("app").unwrap().config.test_timeout_secs,
        120
    );

    manager
        .configure(
            "app",
            ProjectSettings {
                test_command: Some(String::new()),
                ..ProjectSettings::default()
            },
        )
        .unwrap();
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

#[tokio::test]
async fn every_iteration_is_a_commit_on_the_task_branch() {
    let env = env();
    env.planned(config(3, Some(file_exists_command()))).await;
    let orchestrator = env.orchestrator(test_providers());
    ProjectManager::new(&env.store, &orchestrator)
        .run("app", Some(1))
        .await
        .unwrap();

    // TestLlm: iteration 1 rejected by the Reviewer, iteration 2 approved.
    let task_commits = env.git(&["log", "--format=%an|%s", "main^2", "--not", "main^1"]);
    let lines: Vec<&str> = task_commits.lines().collect();
    assert_eq!(lines.len(), 2, "{task_commits}");
    assert!(
        lines
            .iter()
            .all(|line| line.starts_with("EMP-BUILD-001|app-T01: iteration"))
    );

    let merge = env.git(&["log", "-1", "--format=%an|%s|%b", "main"]);
    assert!(
        merge.starts_with("EMP-ORCH-001|Merge app-T01: Data model"),
        "{merge}"
    );
    assert!(
        merge.contains("Approved by EMP-REV-001 after 2 iteration(s)"),
        "{merge}"
    );
    assert_eq!(env.git(&["rev-parse", "--abbrev-ref", "HEAD"]), "main");
    assert!(
        env.git(&["status", "--porcelain"]).is_empty(),
        "clean working tree"
    );

    let body = env.git(&["log", "-1", "--format=%b", "task/app-T01"]);
    assert!(body.contains("tests passed"), "{body}");
}

#[tokio::test]
async fn stopped_task_keeps_its_branch_out_of_main() {
    let env = env();
    env.planned(config(1, None)).await;
    let orchestrator = env.orchestrator(test_providers());
    ProjectManager::new(&env.store, &orchestrator)
        .run("app", Some(1))
        .await
        .unwrap();

    assert!(!env.main_file("notes/result.md").exists(), "not merged");
    assert_eq!(
        env.git(&["log", "--format=%s", "main..task/app-T01"])
            .lines()
            .count(),
        1
    );
}

#[tokio::test]
async fn builder_can_delete_files() {
    let env = env();
    env.planned(config(2, None)).await;
    let builder = ScriptedProvider::new([
        file_answer("old.txt", "obsolete"),
        "Removing it.\n\n### DELETE: old.txt\n### FILE: new.txt\n```\nfresh\n```\n".to_string(),
    ]);
    let reviewer = ScriptedProvider::new([
        r#"{"decision":"CHANGES_REQUIRED","target":"builder","feedback":"drop old.txt"}"#,
        APPROVE,
    ]);
    let orchestrator = env.orchestrator(providers(Some(builder), Some(reviewer)));
    ProjectManager::new(&env.store, &orchestrator)
        .run("app", Some(1))
        .await
        .unwrap();

    assert!(!env.main_file("old.txt").exists());
    assert_eq!(
        fs::read_to_string(env.main_file("new.txt")).unwrap(),
        "fresh\n"
    );
    let state = env.store.load_task_state("app-T01").unwrap().unwrap();
    assert_eq!(state.verification.unwrap().files_deleted, ["old.txt"]);
}

#[tokio::test]
async fn approved_work_is_pushed_to_the_owners_remote() {
    let env = env();
    let remote = env.root.join("remote.git");
    assert!(
        Command::new("git")
            .args(["init", "--bare", "--quiet"])
            .arg(&remote)
            .status()
            .unwrap()
            .success()
    );
    let mut config = config(3, None);
    config.remote_url = Some(remote.to_string_lossy().to_string());
    env.planned(config).await;

    let orchestrator = env.orchestrator(test_providers());
    let summary = ProjectManager::new(&env.store, &orchestrator)
        .run("app", Some(1))
        .await
        .unwrap();
    assert!(summary.push_errors.is_empty(), "{:?}", summary.push_errors);

    let remote_git = |args: &[&str]| {
        let output = Command::new("git")
            .arg("-C")
            .arg(&remote)
            .args(args)
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };
    assert_eq!(
        remote_git(&["rev-parse", "main"]),
        env.git(&["rev-parse", "main"])
    );
    assert_eq!(
        remote_git(&["rev-parse", "task/app-T01"]),
        env.git(&["rev-parse", "task/app-T01"])
    );
}

#[tokio::test]
async fn unreachable_remote_does_not_fail_the_task() {
    let env = env();
    let mut config = config(3, None);
    config.remote_url = Some(env.root.join("missing.git").to_string_lossy().to_string());
    env.planned(config).await;

    let orchestrator = env.orchestrator(test_providers());
    let summary = ProjectManager::new(&env.store, &orchestrator)
        .run("app", Some(1))
        .await
        .unwrap();

    assert_eq!(
        summary.executed,
        [("app-T01".to_string(), TaskStatus::Done)]
    );
    assert!(
        !summary.push_errors.is_empty(),
        "the failed push is reported"
    );
    assert!(
        env.main_file("notes/result.md").is_file(),
        "work is safe locally"
    );
}

#[tokio::test]
async fn resumed_task_resolves_conflicts_with_newer_main() {
    let env = env();
    env.planned(config(1, None)).await; // TestLlm's reviewer rejects the first review
    let orchestrator = env.orchestrator(test_providers());
    let manager = ProjectManager::new(&env.store, &orchestrator);
    manager.run("app", Some(1)).await.unwrap();
    assert_eq!(
        env.store.task("app-T01").unwrap().status,
        TaskStatus::HumanReviewRequired
    );

    // Someone committed a different notes/result.md on main meanwhile.
    fs::create_dir_all(env.main_file("notes")).unwrap();
    fs::write(
        env.main_file("notes/result.md"),
        "from main
",
    )
    .unwrap();
    env.git(&["add", "notes/result.md"]);
    env.git(&[
        "-c",
        "user.name=Owner",
        "-c",
        "user.email=o@x",
        "commit",
        "-q",
        "-m",
        "hand edit",
    ]);

    manager.resume_task("app-T01", None, 2).unwrap();
    let summary = manager.run("app", Some(1)).await.unwrap();
    assert_eq!(
        summary.executed,
        [("app-T01".to_string(), TaskStatus::Done)]
    );

    let state = env.store.load_task_state("app-T01").unwrap().unwrap();
    assert!(
        state
            .history
            .iter()
            .any(|item| item.contains("conflicts for the Builder: notes/result.md"))
    );
    let merged = fs::read_to_string(env.main_file("notes/result.md")).unwrap();
    assert!(
        !merged.contains("<<<<<<<"),
        "the resolution, not the markers, reached main"
    );
}

#[tokio::test]
async fn owner_request_with_a_file_runs_without_a_plan() {
    let env = env();
    env.store.add_project(&config(3, None)).unwrap(); // no plan at all
    let orchestrator = env.orchestrator(test_providers());
    let manager = ProjectManager::new(&env.store, &orchestrator);

    let input = env.root.join("sales.csv");
    fs::write(
        &input,
        "region,amount
north,10
",
    )
    .unwrap();
    let inputs = manager.add_inputs("app", &[input]).unwrap();
    assert_eq!(inputs, ["inputs/sales.csv"]);
    assert_eq!(
        env.git(&["log", "-1", "--format=%an|%s", "main"]),
        "OWNER|Owner inputs"
    );

    let task = manager
        .add_request("app", "Summarise sales.csv per region", &inputs)
        .unwrap();
    assert_eq!(task.id, "app-T01");
    assert!(task.description.contains("inputs/sales.csv"));

    let summary = manager.run("app", None).await.unwrap();
    assert_eq!(summary.executed, [(task.id.clone(), TaskStatus::Done)]);
    assert_eq!(summary.project_status, ProjectStatus::Done);
    assert!(env.main_file("inputs/sales.csv").is_file());

    // A new request reopens the finished project.
    let second = manager.add_request("app", "Also add a chart", &[]).unwrap();
    assert_eq!(second.id, "app-T02");
    assert_eq!(
        env.store.project("app").unwrap().status,
        ProjectStatus::Planned
    );
    let decisions = env.store.owner_decisions("app").unwrap();
    assert!(decisions.iter().any(|d| d.decision == "inputs"));
    assert_eq!(
        decisions.iter().filter(|d| d.decision == "request").count(),
        2
    );

    // The "Owner requests" report is rewritten to cover the new request too.
    let summary = manager.run("app", None).await.unwrap();
    assert_eq!(summary.executed, [(second.id.clone(), TaskStatus::Done)]);
    assert_eq!(summary.reports.len(), 1);
    let report = fs::read_to_string(&summary.reports[0]).unwrap();
    assert!(
        report.contains("app-T01") && report.contains("app-T02"),
        "{report}"
    );
}

/// TestLlm for every role except the Architect, which is scripted.
fn with_architect(architect: ScriptedProvider) -> ProviderFactory {
    Arc::new(move |role, config| {
        Ok(match role {
            Role::Architect => Arc::new(architect.clone()) as Arc<dyn LlmProvider>,
            _ => Arc::new(TestLlm::new(role, config.model.clone())),
        })
    })
}

#[tokio::test]
async fn follow_up_tasks_are_specified_from_the_existing_files() {
    let env = env();
    env.planned(config(2, None)).await;
    let first = env.orchestrator(test_providers());
    ProjectManager::new(&env.store, &first)
        .run("app", Some(1))
        .await
        .unwrap();
    assert_eq!(env.store.task("app-T01").unwrap().status, TaskStatus::Done);

    let architect = ScriptedProvider::new(["Specification: extend notes/result.md."]);
    let second = env.orchestrator(with_architect(architect.clone()));
    ProjectManager::new(&env.store, &second)
        .run("app", Some(1))
        .await
        .unwrap();

    let prompt = &architect.prompts()[0];
    assert!(prompt.contains("EXISTING PROJECT FILES"), "{prompt}");
    assert!(prompt.contains("notes/result.md"), "{prompt}");
    assert!(
        prompt.contains("CONTEXT: the project already holds work"),
        "{prompt}"
    );
}

#[tokio::test]
async fn owner_questions_are_answered_without_changing_anything() {
    let env = env();
    env.planned(config(2, None)).await;
    let first = env.orchestrator(test_providers());
    ProjectManager::new(&env.store, &first)
        .run("app", Some(1))
        .await
        .unwrap();
    let head = env.git(&["rev-parse", "HEAD"]);
    let tasks_before = env.store.tasks("app").unwrap().len();

    let architect = ScriptedProvider::new(["The project has notes/result.md."]);
    let orchestrator = env.orchestrator(with_architect(architect.clone()));
    let answer = ProjectManager::new(&env.store, &orchestrator)
        .ask("app", "ce contine proiectul?")
        .await
        .unwrap();

    assert_eq!(answer, "The project has notes/result.md.");
    let prompt = &architect.prompts()[0];
    assert!(prompt.contains("ce contine proiectul?"), "{prompt}");
    assert!(prompt.contains("notes/result.md"), "{prompt}");
    assert!(prompt.contains("app-T01 [DONE]"), "{prompt}");
    assert_eq!(env.git(&["rev-parse", "HEAD"]), head);
    assert_eq!(env.store.tasks("app").unwrap().len(), tasks_before);
}
