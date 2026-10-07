mod common;

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use ai_team::{
    BoxFuture,
    domain::{Artifact, ArtifactKind, ProjectConfig, Role, Stage, TeamAssignment, TeamState},
    llm::{Completion, LlmProvider, ProviderFactory},
    orchestrator::Orchestrator,
    planner::Planner,
    project::{ProjectManager, ProjectSettings, ProjectStatus, Store, TaskStatus},
    registry::Registry,
};
use anyhow::Result;
use common::{ScriptedProvider, TestLlm, project, sample_company, test_providers, test_task_plan};

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
}

fn project_with(max_iterations: u32) -> ProjectConfig {
    let mut config = project(max_iterations);
    config.project_id = "shop".to_string();
    config
}

/// Adds the project, plans it with the test planner and approves the plan.
async fn planned(env: &Env, max_iterations: u32) {
    let orchestrator = env.orchestrator(test_providers());
    let manager = ProjectManager::new(&env.store, &orchestrator);
    env.store
        .add_project(&project_with(max_iterations))
        .unwrap();
    manager.plan("shop", None).await.unwrap();
    manager.approve("shop", None).unwrap();
}

fn statuses(store: &Store) -> Vec<(String, TaskStatus)> {
    store
        .tasks("shop")
        .unwrap()
        .into_iter()
        .map(|task| (task.id, task.status))
        .collect()
}

fn status_of(store: &Store, id: &str) -> TaskStatus {
    store.task(id).unwrap().status
}

// ---------------------------------------------------------------------------
// Planning
// ---------------------------------------------------------------------------

#[tokio::test]
async fn plan_waits_for_owner_approval_then_creates_ordered_tasks() {
    let env = env();
    let orchestrator = env.orchestrator(test_providers());
    let manager = ProjectManager::new(&env.store, &orchestrator);
    env.store.add_project(&project_with(3)).unwrap();

    let plan = manager.plan("shop", Some("keep it small")).await.unwrap();
    assert_eq!(plan.milestones.len(), 2);
    assert_eq!(
        env.store.project("shop").unwrap().status,
        ProjectStatus::Planning
    );
    assert!(
        env.store.tasks("shop").unwrap().is_empty(),
        "nothing exists before approval"
    );

    let err = manager.run("shop", None).await.unwrap_err();
    assert!(format!("{err:#}").contains("no approved plan"));

    let tasks = manager.approve("shop", Some("go")).unwrap();
    let ids: Vec<_> = tasks.iter().map(|task| task.id.as_str()).collect();
    assert_eq!(ids, ["shop-T01", "shop-T02", "shop-T03"]);
    assert_eq!(tasks[1].depends_on, ["shop-T01"]);
    assert_eq!(tasks[2].depends_on, ["shop-T02"]);
    assert!(tasks.iter().all(|task| task.status == TaskStatus::Planned));
    assert_eq!(
        env.store.project("shop").unwrap().status,
        ProjectStatus::Planned
    );

    assert!(
        manager.approve("shop", None).is_err(),
        "a plan is approved once"
    );
    let decisions = env.store.owner_decisions("shop").unwrap();
    assert_eq!(decisions[0].decision, "approve_plan");
}

#[tokio::test]
async fn invalid_task_plans_are_sent_back_and_never_stored() {
    let env = env();
    let cyclic = serde_json::json!({"milestones": [{"name": "Core", "tasks": [
        {"key": "A", "title": "A", "description": "a", "acceptance_criteria": ["x"], "depends_on": ["B"]},
        {"key": "B", "title": "B", "description": "b", "acceptance_criteria": ["y"], "depends_on": ["A"]}
    ]}]})
    .to_string();
    let scripted = ScriptedProvider::new([cyclic, test_task_plan().to_string()]);
    let orchestrator = env
        .orchestrator(test_providers())
        .with_planner(Planner::new(
            "EMP-PLAN-001",
            "sys",
            Arc::new(scripted.clone()),
        ));
    let manager = ProjectManager::new(&env.store, &orchestrator);
    env.store.add_project(&project_with(3)).unwrap();

    manager.plan("shop", None).await.unwrap();
    let second_prompt = &scripted.prompts()[1];
    assert!(second_prompt.contains("REJECTED BY THE CONTROL PLANE"));
    assert!(second_prompt.contains("dependency cycle among tasks: A, B"));

    let stored = env.store.pending_plan("shop").unwrap().unwrap();
    assert_eq!(
        stored.milestones[0].tasks[0].key, "T1",
        "only the valid plan is stored"
    );
}

#[tokio::test]
async fn three_invalid_task_plans_fail() {
    let env = env();
    let scripted = ScriptedProvider::new(["no plan", "{\"milestones\": []}", "still no"]);
    let orchestrator = env
        .orchestrator(test_providers())
        .with_planner(Planner::new("EMP-PLAN-001", "sys", Arc::new(scripted)));
    let manager = ProjectManager::new(&env.store, &orchestrator);
    env.store.add_project(&project_with(3)).unwrap();

    let err = manager.plan("shop", None).await.unwrap_err();
    assert!(format!("{err:#}").contains("no valid task plan after 3 attempts"));
    assert!(env.store.pending_plan("shop").unwrap().is_none());
}

// ---------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------

#[tokio::test]
async fn run_executes_tasks_in_dependency_order_and_reports_milestones() {
    let env = env();
    planned(&env, 3).await;
    let orchestrator = env.orchestrator(test_providers());
    let manager = ProjectManager::new(&env.store, &orchestrator);

    let summary = manager.run("shop", None).await.unwrap();

    let executed: Vec<_> = summary.executed.iter().map(|(id, _)| id.as_str()).collect();
    assert_eq!(executed, ["shop-T01", "shop-T02", "shop-T03"]);
    assert!(
        summary
            .executed
            .iter()
            .all(|(_, status)| *status == TaskStatus::Done)
    );
    assert_eq!(summary.project_status, ProjectStatus::Done);

    // Milestone reports, one per milestone, written when the milestone closed.
    assert_eq!(summary.reports.len(), 2);
    let core = std::fs::read_to_string(&summary.reports[0]).unwrap();
    assert!(core.contains("shop-T01") && core.contains("shop-T02") && !core.contains("shop-T03"));

    // A dependent task sees the approved implementation of its dependency.
    let t02 = env.store.load_task_state("shop-T02").unwrap().unwrap();
    assert!(t02.task.contains("COMPLETED DEPENDENCIES"));
    assert!(t02.task.contains("### shop-T01: Data model"));
    assert!(t02.task.contains("# Implementation (test, call 2)"));

    // A second run has nothing left to do.
    let again = manager.run("shop", None).await.unwrap();
    assert!(again.executed.is_empty());
}

#[tokio::test]
async fn artifacts_are_stored_once_and_referenced_by_hash() {
    let env = env();
    planned(&env, 3).await;
    let orchestrator = env.orchestrator(test_providers());
    ProjectManager::new(&env.store, &orchestrator)
        .run("shop", Some(1))
        .await
        .unwrap();

    let conn = rusqlite::Connection::open(env.root.join("data/ai-team.db")).unwrap();
    let state_json: String = conn
        .query_row(
            "SELECT state_json FROM tasks WHERE id = 'shop-T01'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(state_json.contains("artifact:"));
    assert!(
        !state_json.contains("# Implementation (test"),
        "content stays out of the row"
    );

    let task = env.store.task("shop-T01").unwrap();
    let hash = task.impl_hash.unwrap();
    assert!(env.store.artifact_path(&hash).is_file());
    let restored = env.store.load_task_state("shop-T01").unwrap().unwrap();
    assert!(
        restored
            .implementation
            .unwrap()
            .content
            .starts_with("# Implementation (test, call 2)")
    );
}

#[tokio::test]
async fn max_tasks_limits_one_run() {
    let env = env();
    planned(&env, 3).await;
    let orchestrator = env.orchestrator(test_providers());
    let summary = ProjectManager::new(&env.store, &orchestrator)
        .run("shop", Some(1))
        .await
        .unwrap();
    assert_eq!(summary.executed.len(), 1);
    assert_eq!(summary.project_status, ProjectStatus::InProgress);
    assert_eq!(status_of(&env.store, "shop-T02"), TaskStatus::Planned);
}

/// Counts model calls per role on top of `TestLlm`.
#[derive(Clone, Default)]
struct Calls(Arc<Mutex<Vec<Role>>>);

impl Calls {
    fn count(&self, role: Role) -> usize {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|r| **r == role)
            .count()
    }

    fn providers(&self) -> ProviderFactory {
        let calls = self.clone();
        Arc::new(move |role, config| {
            Ok(Arc::new(Counting {
                inner: TestLlm::new(role, config.model.clone()),
                role,
                calls: calls.clone(),
            }) as Arc<dyn LlmProvider>)
        })
    }
}

struct Counting {
    inner: TestLlm,
    role: Role,
    calls: Calls,
}

impl LlmProvider for Counting {
    fn model_name(&self) -> &str {
        self.inner.model_name()
    }

    fn generate<'a>(&'a self, system: &'a str, user: &'a str) -> BoxFuture<'a, Result<Completion>> {
        self.calls.0.lock().unwrap().push(self.role);
        self.inner.generate(system, user)
    }
}

#[tokio::test]
async fn interrupted_task_resumes_with_its_saved_work() {
    let env = env();
    planned(&env, 3).await;

    // Simulate a crash right after the Architect finished shop-T01.
    let mut state = TeamState::new(
        "old-run".to_string(),
        "shop".to_string(),
        "shop-T01".to_string(),
        "task".to_string(),
        String::new(),
    );
    state.team = Some(TeamAssignment {
        architect: "EMP-ARCH-001".to_string(),
        builder: "EMP-BUILD-001".to_string(),
        reviewer: "EMP-REV-001".to_string(),
    });
    state.specification = Some(Artifact {
        kind: ArtifactKind::Specification,
        author: "EMP-ARCH-001".to_string(),
        revision: 1,
        content: "SAVED SPECIFICATION".to_string(),
    });
    state.stage = Stage::ArchitectureReady;
    env.store
        .save_task_state("shop-T01", TaskStatus::InProgress, &state)
        .unwrap();

    let calls = Calls::default();
    let orchestrator = env.orchestrator(calls.providers());
    let summary = ProjectManager::new(&env.store, &orchestrator)
        .run("shop", Some(1))
        .await
        .unwrap();

    assert_eq!(
        summary.executed,
        [("shop-T01".to_string(), TaskStatus::Done)]
    );
    assert_eq!(
        calls.count(Role::Architect),
        0,
        "the saved specification is reused"
    );
    assert_eq!(calls.count(Role::Planner), 0, "the saved team is kept");
    let resumed = env.store.load_task_state("shop-T01").unwrap().unwrap();
    assert_eq!(
        resumed.specification.unwrap().content,
        "SAVED SPECIFICATION"
    );
    assert!(
        resumed
            .history
            .iter()
            .any(|item| item == "orchestrator: resumed")
    );
    assert!(
        resumed
            .history
            .iter()
            .any(|item| item.contains("team kept"))
    );
}

// ---------------------------------------------------------------------------
// Owner decisions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn human_review_blocks_dependents_until_the_owner_resumes() {
    let env = env();
    planned(&env, 1).await; // one iteration: the test reviewer rejects its first review
    let orchestrator = env.orchestrator(test_providers());
    let manager = ProjectManager::new(&env.store, &orchestrator);

    let summary = manager.run("shop", None).await.unwrap();
    assert_eq!(
        summary.executed,
        [("shop-T01".to_string(), TaskStatus::HumanReviewRequired)]
    );
    assert_eq!(status_of(&env.store, "shop-T02"), TaskStatus::Blocked);
    assert_eq!(
        status_of(&env.store, "shop-T03"),
        TaskStatus::Blocked,
        "blocking propagates"
    );

    assert!(
        manager.resume_task("shop-T02", None, 2).is_err(),
        "only stopped tasks resume"
    );
    let task = manager
        .resume_task("shop-T01", Some("use UUIDs for ids"), 2)
        .unwrap();
    assert_eq!(task.iteration_budget, 3);
    assert_eq!(
        status_of(&env.store, "shop-T02"),
        TaskStatus::Planned,
        "unblocked"
    );

    let summary = manager.run("shop", Some(1)).await.unwrap();
    assert_eq!(
        summary.executed,
        [("shop-T01".to_string(), TaskStatus::Done)]
    );
    let state = env.store.load_task_state("shop-T01").unwrap().unwrap();
    assert!(
        state
            .task
            .contains("OWNER INSTRUCTIONS:\n- use UUIDs for ids")
    );
    assert_eq!(
        state.iteration, 3,
        "continued from iteration 1, not from scratch"
    );
    assert_eq!(
        env.store.runs_of("shop-T01").unwrap().len(),
        2,
        "one run per attempt"
    );
}

#[tokio::test]
async fn owner_can_accept_a_stopped_task() {
    let env = env();
    planned(&env, 1).await;
    let orchestrator = env.orchestrator(test_providers());
    let manager = ProjectManager::new(&env.store, &orchestrator);
    manager.run("shop", None).await.unwrap();

    manager
        .accept_task("shop-T01", Some("good enough"))
        .unwrap();
    assert_eq!(status_of(&env.store, "shop-T01"), TaskStatus::Done);
    assert_eq!(status_of(&env.store, "shop-T02"), TaskStatus::Planned);

    // shop-T02 now stops in human review too (one iteration); accept it to close "Core".
    manager.run("shop", Some(1)).await.unwrap();
    manager.accept_task("shop-T02", None).unwrap();
    let report = std::fs::read_to_string(env.root.join("data/reports/shop-M1.md")).unwrap();
    assert!(report.contains("accepted by the Owner, not by the Reviewer"));
    assert!(report.contains("Owner decision: accept — good enough"));
}

#[tokio::test]
async fn cancelled_task_keeps_dependents_blocked() {
    let env = env();
    planned(&env, 3).await;
    let orchestrator = env.orchestrator(test_providers());
    let manager = ProjectManager::new(&env.store, &orchestrator);

    manager.cancel_task("shop-T01", Some("not needed")).unwrap();
    assert_eq!(
        statuses(&env.store),
        [
            ("shop-T01".to_string(), TaskStatus::Cancelled),
            ("shop-T02".to_string(), TaskStatus::Blocked),
            ("shop-T03".to_string(), TaskStatus::Blocked),
        ]
    );
    let summary = manager.run("shop", None).await.unwrap();
    assert!(summary.executed.is_empty());
    assert_eq!(summary.project_status, ProjectStatus::InProgress);
    assert!(
        manager.cancel_task("shop-T01", None).is_err(),
        "already cancelled"
    );
}

#[tokio::test]
async fn failed_task_keeps_its_error_and_can_be_retried() {
    let env = env();
    planned(&env, 3).await;

    // The Builder's model is unreachable for this run.
    let broken: ProviderFactory = Arc::new(|role, config| {
        Ok(if role == Role::Builder {
            Arc::new(ScriptedProvider::new(Vec::<String>::new())) as Arc<dyn LlmProvider>
        } else {
            Arc::new(TestLlm::new(role, config.model.clone()))
        })
    });
    let orchestrator = env.orchestrator(broken);
    let manager = ProjectManager::new(&env.store, &orchestrator);
    let summary = manager.run("shop", None).await.unwrap();
    assert_eq!(
        summary.executed,
        [("shop-T01".to_string(), TaskStatus::Failed)]
    );
    let failed = env.store.task("shop-T01").unwrap();
    assert!(failed.error.unwrap().contains("Builder failed"));
    assert!(
        failed.spec_hash.is_some(),
        "the specification survives the failure"
    );
    let failed_state = env.store.load_task_state("shop-T01").unwrap().unwrap();
    assert_eq!(
        failed_state.iteration, 0,
        "a failed Builder call does not use up an iteration"
    );

    let orchestrator = env.orchestrator(test_providers());
    let manager = ProjectManager::new(&env.store, &orchestrator);
    manager.resume_task("shop-T01", None, 2).unwrap();
    let summary = manager.run("shop", None).await.unwrap();
    assert!(
        summary
            .executed
            .iter()
            .all(|(_, status)| *status == TaskStatus::Done)
    );
    assert_eq!(summary.project_status, ProjectStatus::Done);
}

#[test]
fn store_survives_reopening() {
    let dir = tempfile::tempdir().unwrap();
    let data: &Path = &dir.path().join("data");
    Store::open(data)
        .unwrap()
        .add_project(&project_with(2))
        .unwrap();

    let reopened = Store::open(data).unwrap();
    assert_eq!(reopened.project("shop").unwrap().config.max_iterations, 2);
    assert!(
        reopened.add_project(&project_with(2)).is_err(),
        "ids are unique"
    );
}

#[tokio::test]
async fn budget_stops_new_tasks_and_kpis_count_the_work() {
    let env = env();
    planned(&env, 3).await;
    let orchestrator = env.orchestrator(test_providers());
    let manager = ProjectManager::new(&env.store, &orchestrator);
    manager.run("shop", Some(1)).await.unwrap();
    let used = manager.project_usage("shop").unwrap();
    let used_tokens = used.input_tokens + used.output_tokens;
    assert!(used_tokens > 0);

    // A budget already spent: no new task starts, and the run says why.
    let budget = |tokens| ProjectSettings {
        budget_tokens: Some(tokens),
        ..ProjectSettings::default()
    };
    manager.configure("shop", budget(used_tokens)).unwrap();
    let summary = manager.run("shop", None).await.unwrap();
    assert!(summary.executed.is_empty());
    let reason = summary.budget_stop.unwrap();
    assert!(
        reason.contains("token budget reached") && reason.contains("shop-T02 not started"),
        "{reason}"
    );
    assert_eq!(status_of(&env.store, "shop-T02"), TaskStatus::Planned);

    // 0 removes the limit and the work continues.
    let config = manager.configure("shop", budget(0)).unwrap();
    assert_eq!(config.budget, None);
    let summary = manager.run("shop", None).await.unwrap();
    assert_eq!(summary.executed.len(), 2);
    assert_eq!(summary.budget_stop, None);

    let report = ai_team::kpi::collect(&env.store, Some("shop")).unwrap();
    assert_eq!((report.overall.tasks, report.overall.done), (3, 3));
    assert_eq!(report.by_builder.len(), 1);
    assert!(report.overall.done_usage.input_tokens > 0);
}

#[tokio::test]
async fn dashboard_snapshot_shows_projects_kpis_team_and_activity() {
    let env = env();
    planned(&env, 3).await;
    let orchestrator = env.orchestrator(test_providers());
    ProjectManager::new(&env.store, &orchestrator)
        .run("shop", None)
        .await
        .unwrap();

    let snapshot = ai_team::dashboard::snapshot(&ai_team::dashboard::Sources {
        data_dir: env.root.join("data"),
        employees_dir: env.company.join("employees"),
        audit_dir: env.root.join("audit"),
    })
    .unwrap();

    let project = &snapshot["projects"][0];
    assert_eq!(project["id"], "shop");
    assert_eq!(project["counts"]["total"], 3);
    assert_eq!(project["counts"]["done"], 3);
    assert_eq!(project["tasks"][0]["status"], "DONE");
    assert!(project["tasks"][0]["history"].as_array().unwrap().len() > 3);
    assert_eq!(snapshot["kpi"]["overall"]["done"], 3);
    assert!(snapshot["team"]["employees"].as_array().unwrap().len() >= 5);
    let activity = snapshot["activity"].as_array().unwrap();
    assert!(!activity.is_empty());
    // The feed carries events, never their payloads (prompts, files).
    assert!(activity.iter().all(|event| event.get("payload").is_none()));
}
