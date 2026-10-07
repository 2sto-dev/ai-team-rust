//! Phase 5a: skill packs, MCP tools, delegation from a lead to its subagents, and the
//! reviewer's advisors with a veto. MCP tests use the shipped stdlib-only `pydoc` server,
//! so they need `python` on PATH.

mod common;

use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use ai_team::{
    agents::{AgentContext, tooling::answer},
    audit::AuditTrail,
    config::ProviderKind,
    domain::{ProjectConfig, Stage},
    llm::{LlmProvider, ProviderFactory, provider_for},
    mcp::Toolbox,
    orchestrator::Orchestrator,
    project::{ProjectManager, Store, TaskStatus},
    registry::Registry,
};
use common::{
    ScriptedProvider, TestLlm, audit_events, contract_yaml, fast_model, project, sample_company,
    write_employee,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

fn pydoc_server() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("company/mcp/pydoc_server.py")
}

/// Adds skill packs, the pydoc MCP server and extra fields to the sample company.
fn write_resources(company: &Path) {
    let skill = company.join("skills/python-tests");
    fs::create_dir_all(&skill).unwrap();
    fs::write(
        skill.join("SKILL.md"),
        "---\nname: Python tests\ndescription: unittest habits\n---\nAlways write unittest cases.\n",
    )
    .unwrap();
    fs::write(
        company.join("mcp.yaml"),
        format!(
            "servers:\n  pydoc:\n    command: python\n    args: [{:?}]\n",
            pydoc_server().to_string_lossy()
        ),
    )
    .unwrap();
}

/// `contract_yaml` plus skill packs and MCP servers.
fn contract_with(base: String, packs: &[&str], servers: &[&str]) -> String {
    let list = |items: &[&str]| {
        if items.is_empty() {
            " []\n".to_string()
        } else {
            items
                .iter()
                .map(|i| format!("\n  - {i}"))
                .collect::<String>()
                + "\n"
        }
    };
    base.replace(
        "skills:\n  - testing\n",
        &format!(
            "skills:\n  - testing\nskill_packs:{}mcp_servers:{}",
            list(packs),
            list(servers)
        ),
    )
}

fn add_subagent(company: &Path, id: &str, manager: &str, permissions: &[&str], model: &str) {
    write_employee(
        &company.join("employees"),
        id,
        &contract_with(
            contract_yaml(
                id,
                "subagent",
                "specialist",
                manager,
                permissions,
                Some(model),
            ),
            &["python-tests"],
            &[],
        ),
    );
}

fn load(company: &Path) -> anyhow::Result<Registry> {
    Registry::load(company.join("employees"))
}

// ---------------------------------------------------------------------------
// Registry: skill packs, MCP servers, veto
// ---------------------------------------------------------------------------

#[test]
fn skill_packs_join_the_system_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let company = sample_company(dir.path());
    write_resources(&company);
    add_subagent(
        &company,
        "EMP-PY-001",
        "EMP-BUILD-001",
        &["write_implementation"],
        "test-py",
    );

    let registry = load(&company).unwrap();
    let prompt = registry.system_prompt(registry.get("EMP-PY-001").unwrap());
    assert!(prompt.starts_with("# EMP-PY-001"), "job description first");
    assert!(prompt.contains("# SKILLS\n\n## Python tests\n\nAlways write unittest cases."));
    assert_eq!(
        registry.resources().skill_packs["python-tests"].description,
        "unittest habits"
    );
}

#[test]
fn resources_are_validated() {
    type Mutation = fn(&Path);
    let cases: [(&str, Mutation); 4] = [
        ("unknown skill pack 'nope'", |company| {
            add_subagent(
                company,
                "EMP-PY-001",
                "EMP-BUILD-001",
                &["write_implementation"],
                "m",
            );
            let path = company.join("employees/EMP-PY-001/contract.yaml");
            let text = fs::read_to_string(&path)
                .unwrap()
                .replace("python-tests", "nope");
            fs::write(path, text).unwrap();
        }),
        ("exceeds manager EMP-BUILD-001's servers", |company| {
            let base = contract_yaml(
                "EMP-PY-001",
                "subagent",
                "specialist",
                "EMP-BUILD-001",
                &["write_implementation"],
                Some("m"),
            );
            write_employee(
                &company.join("employees"),
                "EMP-PY-001",
                &contract_with(base, &[], &["pydoc"]),
            );
        }),
        ("unknown MCP server 'github'", |company| {
            let base = contract_yaml(
                "EMP-PY-001",
                "subagent",
                "specialist",
                "EMP-BUILD-001",
                &["write_implementation"],
                Some("m"),
            );
            write_employee(
                &company.join("employees"),
                "EMP-PY-001",
                &contract_with(base, &[], &["github"]),
            );
        }),
        ("veto_review is only for a reviewer's subagent", |company| {
            add_subagent(
                company,
                "EMP-PY-001",
                "EMP-BUILD-001",
                &["write_implementation", "veto_review"],
                "m",
            );
        }),
    ];
    for (expected, mutate) in cases {
        let dir = tempfile::tempdir().unwrap();
        let company = sample_company(dir.path());
        write_resources(&company);
        mutate(&company);
        let err = match load(&company) {
            Ok(_) => panic!("expected rejection: {expected}"),
            Err(err) => format!("{err:#}"),
        };
        assert!(err.contains(expected), "expected '{expected}' in:\n{err}");
    }

    // A reviewer's specialist may hold the veto even though the reviewer does not.
    let dir = tempfile::tempdir().unwrap();
    let company = sample_company(dir.path());
    write_resources(&company);
    add_subagent(
        &company,
        "EMP-SEC-001",
        "EMP-REV-001",
        &["review_work", "veto_review"],
        "m",
    );
    load(&company).unwrap();
}

// ---------------------------------------------------------------------------
// MCP
// ---------------------------------------------------------------------------

fn pydoc_toolbox(company: &Path) -> Toolbox {
    write_resources(company);
    let registry_like: ai_team::mcp::McpCatalog =
        serde_yaml_ng::from_str(&fs::read_to_string(company.join("mcp.yaml")).unwrap()).unwrap();
    let config = registry_like.servers["pydoc"].clone();
    Toolbox::new(vec![("pydoc".to_string(), config)], company.to_path_buf())
}

#[tokio::test]
async fn mcp_client_lists_and_calls_tools() {
    let dir = tempfile::tempdir().unwrap();
    let toolbox = pydoc_toolbox(dir.path());

    let names: Vec<String> = toolbox
        .specs()
        .await
        .unwrap()
        .iter()
        .map(|s| s.name.clone())
        .collect();
    assert_eq!(names, ["pydoc__lookup", "pydoc__module_members"]);

    let doc = toolbox
        .call("pydoc__lookup", serde_json::json!({ "name": "json.dumps" }))
        .await
        .unwrap();
    assert!(!doc.is_error && doc.text.contains("dumps"), "{}", doc.text);

    let refused = toolbox
        .call("pydoc__lookup", serde_json::json!({ "name": "requests" }))
        .await
        .unwrap();
    assert!(refused.is_error && refused.text.contains("not in the Python standard library"));

    let unknown = toolbox
        .call("pydoc__nope", serde_json::json!({}))
        .await
        .unwrap();
    assert!(unknown.is_error);
}

/// An Ollama-shaped server: first asks for a tool, then answers; records every request.
async fn ollama_with_tool_call() -> (String, Arc<Mutex<Vec<serde_json::Value>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = requests.clone();
    tokio::spawn(async move {
        let replies = [
            serde_json::json!({
                "message": {"role": "assistant", "content": "", "tool_calls": [
                    {"function": {"name": "pydoc__lookup", "arguments": {"name": "json.dumps"}}}
                ]},
                "done": true, "prompt_eval_count": 10, "eval_count": 5
            }),
            serde_json::json!({
                "message": {"role": "assistant", "content": "use json.dumps(obj, indent=2)"},
                "done": true, "prompt_eval_count": 50, "eval_count": 8
            }),
        ];
        for reply in replies {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = Vec::new();
            let mut chunk = [0u8; 65536];
            loop {
                let read = socket.read(&mut chunk).await.unwrap();
                buffer.extend_from_slice(&chunk[..read]);
                let text = String::from_utf8_lossy(&buffer).to_string();
                if let Some(end) = text.find("\r\n\r\n") {
                    let length: usize = text[..end]
                        .lines()
                        .find_map(|l| {
                            l.to_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse().unwrap())
                        })
                        .unwrap_or(0);
                    if buffer.len() >= end + 4 + length {
                        seen.lock()
                            .unwrap()
                            .push(serde_json::from_str(&text[end + 4..]).unwrap());
                        break;
                    }
                }
            }
            let body = reply.to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        }
    });
    (format!("http://{addr}"), requests)
}

#[tokio::test]
async fn tool_loop_runs_mcp_calls_and_audits_them() {
    let dir = tempfile::tempdir().unwrap();
    let toolbox = pydoc_toolbox(dir.path());
    let (origin, requests) = ollama_with_tool_call().await;

    let mut config = fast_model("qwen3-coder:30b");
    config.provider = ProviderKind::Ollama;
    config.base_url = Some(origin);
    let llm = provider_for(&config).unwrap();
    let audit = AuditTrail::create(dir.path().join("audit"), "run").unwrap();
    let ctx = AgentContext::root("run", audit.clone());

    let text = answer(
        llm.as_ref(),
        "sys",
        "How do I pretty-print JSON?",
        Some(&toolbox),
        &ctx,
    )
    .await
    .unwrap();
    assert_eq!(text, "use json.dumps(obj, indent=2)");

    let requests = requests.lock().unwrap();
    assert_eq!(requests[0]["tools"][0]["function"]["name"], "pydoc__lookup");
    let tool_message = requests[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "tool")
        .expect("the tool result goes back to the model");
    assert_eq!(tool_message["tool_name"], "pydoc__lookup");
    assert!(tool_message["content"].as_str().unwrap().contains("dumps"));

    let events = audit_events(audit.path());
    let call = events.iter().find(|e| e["event"] == "TOOL_CALL").unwrap();
    assert_eq!(call["payload"]["tool"], "pydoc__lookup");
    assert_eq!(call["payload"]["is_error"], false);
}

// ---------------------------------------------------------------------------
// Delegation and veto inside a project run
// ---------------------------------------------------------------------------

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
    write_resources(&company);
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
        Orchestrator::from_registry_with_providers(load(&self.company).unwrap(), providers)
            .unwrap()
            .with_audit_dir(self.root.join("audit"))
    }

    async fn planned(&self, max_iterations: u32) {
        let mut config: ProjectConfig = project(max_iterations);
        config.project_id = "app".to_string();
        let orchestrator = self.orchestrator(common::test_providers());
        let manager = ProjectManager::new(&self.store, &orchestrator);
        self.store.add_project(&config).unwrap();
        manager.plan("app", None).await.unwrap();
        manager.approve("app", None).unwrap();
    }

    fn file(&self, relative: &str) -> PathBuf {
        self.root.join("data/workspaces/app").join(relative)
    }

    fn audit(&self) -> Vec<serde_json::Value> {
        fs::read_dir(self.root.join("audit"))
            .unwrap()
            .flat_map(|entry| audit_events(&entry.unwrap().path()))
            .collect()
    }
}

/// TestLlm everywhere, except models listed with a scripted provider.
fn providers_with(overrides: Vec<(&'static str, ScriptedProvider)>) -> ProviderFactory {
    Arc::new(move |role, config| {
        Ok(
            match overrides.iter().find(|(model, _)| *model == config.model) {
                Some((_, scripted)) => Arc::new(scripted.clone()) as Arc<dyn LlmProvider>,
                None => Arc::new(TestLlm::new(role, config.model.clone())),
            },
        )
    })
}

#[tokio::test]
async fn lead_delegates_and_the_platform_combines_owned_files() {
    let env = env();
    add_subagent(
        &env.company,
        "EMP-PY-001",
        "EMP-BUILD-001",
        &["write_implementation"],
        "test-py",
    );
    add_subagent(
        &env.company,
        "EMP-QA-001",
        "EMP-BUILD-001",
        &["write_implementation"],
        "test-qa",
    );
    // EMP-QA-001 also tries to write a file it does not own.
    let qa = ScriptedProvider::new([
        "### FILE: work/emp-qa-001.txt\n```\nqa\n```\n### FILE: src/stolen.py\n```\nx\n```\n",
        "### FILE: work/emp-qa-001.txt\n```\nqa v2\n```\n",
    ]);
    env.planned(3).await;

    let orchestrator = env.orchestrator(providers_with(vec![("test-qa", qa.clone())]));
    let summary = ProjectManager::new(&env.store, &orchestrator)
        .run("app", Some(1))
        .await
        .unwrap();
    assert_eq!(
        summary.executed,
        [("app-T01".to_string(), TaskStatus::Done)]
    );

    assert_eq!(
        fs::read_to_string(env.file("work/emp-py-001.txt")).unwrap(),
        "written by a specialist\n"
    );
    assert_eq!(
        fs::read_to_string(env.file("work/emp-qa-001.txt")).unwrap(),
        "qa v2\n"
    );
    assert!(
        !env.file("src/stolen.py").exists(),
        "files outside a subtask are discarded"
    );

    let qa_prompt = &qa.prompts()[0];
    assert!(qa_prompt.contains("FILES YOU OWN"));
    assert!(qa_prompt.contains("- work/emp-qa-001.txt"));
    assert!(
        !qa_prompt.contains("work/emp-py-001.txt"),
        "only its own subtask"
    );

    let events = env.audit();
    let lead_span = events
        .iter()
        .find(|e| e["actor"] == "EMP-BUILD-001" && e["event"] == "AGENT_STARTED")
        .unwrap()["span_id"]
        .clone();
    let done = events
        .iter()
        .find(|e| e["actor"] == "EMP-QA-001" && e["event"] == "SUBTASK_DONE")
        .unwrap();
    assert_eq!(
        done["parent_span_id"], lead_span,
        "subagents nest under the lead"
    );
    assert_eq!(done["payload"]["dropped"][0], "src/stolen.py");
}

#[tokio::test]
async fn invalid_split_falls_back_to_the_lead_working_alone() {
    let env = env();
    add_subagent(
        &env.company,
        "EMP-PY-001",
        "EMP-BUILD-001",
        &["write_implementation"],
        "test-py",
    );
    let bad = r#"{"subtasks":[{"assignee":"EMP-GHOST-001","title":"x","instructions":"y","files":["a.txt"]}]}"#;
    let lead = ScriptedProvider::new([
        bad.to_string(),
        bad.to_string(),
        "### FILE: solo.txt\n```\nby the lead\n```\n".to_string(),
    ]);
    env.planned(3).await;

    let reviewer = ScriptedProvider::new([r#"{"decision":"APPROVED","feedback":"ok"}"#]);
    let orchestrator = env.orchestrator(providers_with(vec![
        ("test-builder", lead.clone()),
        ("test-reviewer", reviewer),
    ]));
    ProjectManager::new(&env.store, &orchestrator)
        .run("app", Some(1))
        .await
        .unwrap();

    assert_eq!(
        fs::read_to_string(env.file("solo.txt")).unwrap(),
        "by the lead\n"
    );
    assert!(lead.prompts()[1].contains("REJECTED BY THE CONTROL PLANE"));
    assert!(lead.prompts()[1].contains("not in your team"));
    let events = env.audit();
    assert!(events.iter().any(|e| e["event"] == "DELEGATION_SKIPPED"));
}

#[tokio::test]
async fn security_veto_cannot_be_overruled() {
    let env = env();
    // The reviewer's security specialist holds the veto.
    add_subagent(
        &env.company,
        "EMP-SEC-001",
        "EMP-REV-001",
        &["review_work", "veto_review"],
        "test-sec",
    );
    let security = ScriptedProvider::new([
        r#"{"decision":"CHANGES_REQUIRED","feedback":"secrets are logged"}"#,
        r#"{"decision":"CHANGES_REQUIRED","feedback":"secrets are still logged"}"#,
    ]);
    env.planned(2).await;

    // The Reviewer itself approves every time.
    let reviewer = ScriptedProvider::new([
        r#"{"decision":"APPROVED","feedback":"great"}"#,
        r#"{"decision":"APPROVED","feedback":"great"}"#,
    ]);
    let orchestrator = env.orchestrator(providers_with(vec![
        ("test-sec", security),
        ("test-reviewer", reviewer.clone()),
    ]));
    let summary = ProjectManager::new(&env.store, &orchestrator)
        .run("app", Some(1))
        .await
        .unwrap();

    assert_eq!(summary.executed[0].1, TaskStatus::HumanReviewRequired);
    let state = env.store.load_task_state("app-T01").unwrap().unwrap();
    assert_eq!(state.stage, Stage::HumanReviewRequired);
    assert!(
        state
            .review
            .unwrap()
            .feedback
            .starts_with("Veto by EMP-SEC-001: secrets are still logged")
    );
    assert!(
        reviewer.prompts()[0].contains("EMP-SEC-001 [VETO]: ChangesRequired - secrets are logged")
    );
    assert!(env.audit().iter().any(|e| e["event"] == "VETO_APPLIED"));
}

#[tokio::test]
async fn architect_consultants_advise_before_the_specification() {
    let env = env();
    add_subagent(
        &env.company,
        "EMP-DATA-001",
        "EMP-ARCH-001",
        &["write_specification"],
        "test-data",
    );
    add_subagent(
        &env.company,
        "EMP-INTEG-001",
        "EMP-ARCH-001",
        &["write_specification"],
        "test-integ",
    );
    let data = ScriptedProvider::new([
        "Store orders in one table keyed by (tenant_id, id).",
        "Store orders in one table keyed by (tenant_id, id).",
    ]);
    let integration = ScriptedProvider::new(["NOT RELEVANT", "Not relevant."]);
    let architect = ScriptedProvider::new([
        "Specification: orders table.",
        "Specification: orders table, revised.",
    ]);
    env.planned(2).await;

    let orchestrator = env.orchestrator(providers_with(vec![
        ("test-data", data.clone()),
        ("test-integ", integration.clone()),
        ("test-architect", architect.clone()),
    ]));
    ProjectManager::new(&env.store, &orchestrator)
        .run("app", Some(1))
        .await
        .unwrap();

    let prompt = &architect.prompts()[0];
    assert!(
        prompt.contains("NOTES FROM YOUR DESIGN CONSULTANTS"),
        "{prompt}"
    );
    assert!(
        prompt.contains("### EMP-DATA-001\nStore orders in one table"),
        "{prompt}"
    );
    assert!(!prompt.contains("EMP-INTEG-001"), "{prompt}");
    // Consultants see the task, not the Architect's or Builder's work.
    assert!(data.prompts()[0].contains("Give your design notes for your domain only"));
    let outcomes: Vec<String> = env
        .audit()
        .iter()
        .filter(|event| event["event"] == "CONSULTATION_DONE")
        .map(|event| {
            event["payload"]["outcome"]
                .as_str()
                .unwrap_or("")
                .to_string()
        })
        .collect();
    assert!(outcomes.contains(&"notes".to_string()), "{outcomes:?}");
    assert!(
        outcomes.contains(&"not relevant".to_string()),
        "{outcomes:?}"
    );
}

#[tokio::test]
async fn planner_hiring_suggestions_become_owner_proposals() {
    let env = env();
    env.planned(2).await;
    let team =
        r#""team":{"architect":"EMP-ARCH-001","builder":"EMP-BUILD-001","reviewer":"EMP-REV-001"}"#;
    let suggestion = format!(
        r#"{{{team},"rationale":"usual team","hire":{{"title":"Elixir Developer","manager":"EMP-BUILD-001","skills":["elixir","otp"],"reason":"the task is in Elixir"}}}}"#
    );
    let planner = ScriptedProvider::new([suggestion.clone(), suggestion]);
    let orchestrator = env.orchestrator(providers_with(vec![("test-planner", planner)]));
    let summary = ProjectManager::new(&env.store, &orchestrator)
        .run("app", Some(2))
        .await
        .unwrap();

    // The run goes on with the chosen team.
    assert_eq!(summary.executed.len(), 2);
    let proposal = env.company.join("proposals/EMP-ELIXIR-001");
    let contract = fs::read_to_string(proposal.join("contract.yaml")).unwrap();
    assert!(contract.contains("manager_id: EMP-BUILD-001"), "{contract}");
    assert!(contract.contains("  - elixir\n  - otp\n"), "{contract}");
    let job = fs::read_to_string(proposal.join("job_description.md")).unwrap();
    assert!(
        job.contains("suggested by the Planner: the task is in Elixir"),
        "{job}"
    );
    // Nothing is hired without the Owner, and a pending proposal is not written twice.
    assert!(!env.company.join("employees/EMP-ELIXIR-001").exists());
    assert!(!env.company.join("proposals/EMP-ELIXIR-002").exists());
    let state = env.store.load_task_state("app-T01").unwrap().unwrap();
    assert!(
        state
            .history
            .iter()
            .any(|line| line.contains("suggests hiring Elixir Developer (EMP-ELIXIR-001)"))
    );
    let events: Vec<_> = env
        .audit()
        .into_iter()
        .map(|e| e["event"].as_str().unwrap_or("").to_string())
        .collect();
    assert!(events.contains(&"HIRE_SUGGESTED".to_string()));
    assert!(events.contains(&"HIRE_SUGGESTION_IGNORED".to_string()));
}

#[tokio::test]
async fn files_a_specialist_did_not_deliver_stop_the_iteration() {
    let env = env();
    add_subagent(
        &env.company,
        "EMP-PY-001",
        "EMP-BUILD-001",
        &["write_implementation"],
        "test-py",
    );
    let split = r#"{"subtasks":[{"assignee":"EMP-PY-001","title":"crate","instructions":"Write Cargo.toml and src/lib.rs","files":["Cargo.toml","src/lib.rs"]}]}"#;
    let lead = ScriptedProvider::new([split]);
    // The specialist forgets Cargo.toml, as qwen3-coder did in a real run.
    let specialist = ScriptedProvider::new(["### FILE: src/lib.rs\n```\npub fn f() {}\n```\n"]);
    env.planned(1).await;

    let orchestrator = env.orchestrator(providers_with(vec![
        ("test-builder", lead),
        ("test-py", specialist),
    ]));
    let summary = ProjectManager::new(&env.store, &orchestrator)
        .run("app", Some(1))
        .await
        .unwrap();

    assert_eq!(summary.executed[0].1, TaskStatus::HumanReviewRequired);
    let state = env.store.load_task_state("app-T01").unwrap().unwrap();
    let verification = state.verification.unwrap();
    assert!(
        verification
            .problems
            .iter()
            .any(|p| p.starts_with("Cargo.toml was assigned")),
        "{:?}",
        verification.problems
    );
    // What the specialist did deliver is kept on the task branch.
    assert_eq!(verification.files_written, ["src/lib.rs"]);
}
