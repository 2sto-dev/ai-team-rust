mod common;

use std::{fs, path::Path};

use ai_team::registry::{self, EmployeeFunction, EmployeeStatus, HireRequest, Registry};
use common::{audit_events, contract_yaml, edit_contract, sample_company, write_employee};

fn load(company: &Path) -> anyhow::Result<Registry> {
    Registry::load(company.join("employees"))
}

fn assert_rejected(company: &Path, expected: &str) {
    let err = match load(company) {
        Ok(_) => panic!("registry should be rejected (expected: {expected})"),
        Err(err) => format!("{err:#}"),
    };
    assert!(err.contains(expected), "expected '{expected}' in:\n{err}");
}

#[test]
fn shipped_registry_is_valid_and_ready() {
    let registry = Registry::load(Path::new(env!("CARGO_MANIFEST_DIR")).join("company/employees"))
        .expect("company/employees must stay valid");
    assert!(
        registry.readiness_issues().is_empty(),
        "{:?}",
        registry.readiness_issues()
    );
}

#[test]
fn sample_company_loads() {
    let dir = tempfile::tempdir().unwrap();
    let registry = load(&sample_company(dir.path())).unwrap();

    assert_eq!(registry.employees().count(), 5);
    assert_eq!(
        registry.active(EmployeeFunction::Builder)[0].id(),
        "EMP-BUILD-001"
    );
    assert_eq!(registry.reports_of("EMP-ORCH-001").len(), 4);
}

/// Each case breaks exactly one rule of the company and names the expected error.
#[test]
fn rejects_invalid_contracts() {
    type Mutation = fn(&Path);
    let cases: Vec<(&str, Mutation, &str)> = vec![
        (
            "unknown permission",
            |c| {
                edit_contract(
                    c,
                    "EMP-BUILD-001",
                    "  - delegate_subtasks",
                    "  - deploy_production",
                )
            },
            "invalid contract.yaml",
        ),
        (
            "misspelled field",
            |c| edit_contract(c, "EMP-BUILD-001", "skills:", "skils:"),
            "invalid contract.yaml",
        ),
        (
            "folder name differs from id",
            |c| {
                let employees = c.join("employees");
                write_employee(
                    &employees,
                    "EMP-X-001",
                    &contract_yaml(
                        "EMP-Y-001",
                        "agent",
                        "builder",
                        "EMP-ORCH-001",
                        &["write_implementation"],
                        Some("m"),
                    ),
                )
            },
            "must match employee_id",
        ),
        (
            "separation of duties",
            |c| {
                edit_contract(
                    c,
                    "EMP-BUILD-001",
                    "  - delegate_subtasks",
                    "  - review_work",
                )
            },
            "separation of duties",
        ),
        (
            "duplicate permission",
            |c| {
                edit_contract(
                    c,
                    "EMP-BUILD-001",
                    "  - delegate_subtasks
",
                    "  - delegate_subtasks
  - delegate_subtasks
",
                )
            },
            "listed twice",
        ),
        (
            "function without its capability",
            |c| edit_contract(c, "EMP-REV-001", "  - review_work\n", ""),
            "requires permission review_work",
        ),
        (
            "agent reporting to an agent",
            |c| {
                edit_contract(
                    c,
                    "EMP-ARCH-001",
                    "manager_id: EMP-ORCH-001",
                    "manager_id: EMP-BUILD-001",
                )
            },
            "agents report to the orchestrator",
        ),
        (
            "unknown manager",
            |c| {
                edit_contract(
                    c,
                    "EMP-ARCH-001",
                    "manager_id: EMP-ORCH-001",
                    "manager_id: EMP-NOPE-001",
                )
            },
            "unknown manager_id",
        ),
        (
            "subagent with more permissions than its manager",
            |c| {
                write_employee(
                    &c.join("employees"),
                    "EMP-SEC-001",
                    &contract_yaml(
                        "EMP-SEC-001",
                        "subagent",
                        "specialist",
                        "EMP-BUILD-001",
                        &["review_work"],
                        Some("m"),
                    ),
                )
            },
            "exceeds manager EMP-BUILD-001",
        ),
        (
            "subagent that delegates",
            |c| {
                write_employee(
                    &c.join("employees"),
                    "EMP-PY-001",
                    &contract_yaml(
                        "EMP-PY-001",
                        "subagent",
                        "specialist",
                        "EMP-BUILD-001",
                        &["delegate_subtasks"],
                        Some("m"),
                    ),
                )
            },
            "subagents cannot delegate",
        ),
        (
            "second orchestrator",
            |c| {
                write_employee(
                    &c.join("employees"),
                    "EMP-ORCH-002",
                    &contract_yaml("EMP-ORCH-002", "system", "orchestrator", "OWNER", &[], None),
                )
            },
            "exactly one orchestrator",
        ),
        (
            "two active planners",
            |c| {
                write_employee(
                    &c.join("employees"),
                    "EMP-PLAN-002",
                    &contract_yaml(
                        "EMP-PLAN-002",
                        "agent",
                        "planner",
                        "EMP-ORCH-001",
                        &["propose_plan"],
                        Some("m"),
                    ),
                )
            },
            "at most one active planner",
        ),
        (
            "openai without base_url",
            |c| edit_contract(c, "EMP-BUILD-001", "  base_url: http://127.0.0.1:9\n", ""),
            "base_url is required",
        ),
        (
            "system employee with a model",
            |c| {
                edit_contract(
                    c,
                    "EMP-ORCH-001",
                    "skills:",
                    "model:\n  provider: ollama\n  model: m\n  base_url: http://127.0.0.1:9\nskills:",
                )
            },
            "system employees have no model",
        ),
        (
            "empty job description",
            |c| fs::write(c.join("employees/EMP-ARCH-001/job_description.md"), "  \n").unwrap(),
            "job_description.md is empty",
        ),
    ];

    for (name, mutate, expected) in cases {
        let dir = tempfile::tempdir().unwrap();
        let company = sample_company(dir.path());
        mutate(&company);
        let result = std::panic::catch_unwind(|| assert_rejected(&company, expected));
        assert!(result.is_ok(), "case '{name}' failed");
    }
}

/// Contracts saved by Windows tools (Notepad, PowerShell 5.1) start with a UTF-8 BOM.
#[test]
fn contracts_with_a_utf8_bom_load() {
    let dir = tempfile::tempdir().unwrap();
    let company = sample_company(dir.path());
    let path = company.join("employees/EMP-BUILD-001/contract.yaml");
    let text = fs::read_to_string(&path).unwrap();
    fs::write(&path, format!("\u{feff}{text}")).unwrap();

    let registry = load(&company).unwrap();
    assert_eq!(
        registry.get("EMP-BUILD-001").unwrap().contract.name,
        "builder EMP-BUILD-001"
    );
}

#[test]
fn reports_every_violation_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let company = sample_company(dir.path());
    edit_contract(
        &company,
        "EMP-BUILD-001",
        "  - delegate_subtasks",
        "  - review_work",
    );
    edit_contract(
        &company,
        "EMP-ARCH-001",
        "manager_id: EMP-ORCH-001",
        "manager_id: EMP-NOPE-001",
    );

    let err = format!("{:#}", load(&company).err().unwrap());
    assert!(err.contains("separation of duties"));
    assert!(err.contains("unknown manager_id"));
}

fn python_request() -> HireRequest {
    HireRequest {
        employee_id: "EMP-PY-001".to_string(),
        name: "Python Developer".to_string(),
        function: EmployeeFunction::Specialist,
        manager_id: "EMP-BUILD-001".to_string(),
        title: None,
        department: None,
    }
}

fn fill_placeholders(proposal: &Path) {
    for file in ["contract.yaml", "job_description.md"] {
        let path = proposal.join(file);
        let text = fs::read_to_string(&path).unwrap();
        fs::write(&path, text.replace("TODO", "python")).unwrap();
    }
}

#[test]
fn hiring_requires_filled_proposal_and_owner_approval() {
    let dir = tempfile::tempdir().unwrap();
    let company = sample_company(dir.path());
    let audit_dir = dir.path().join("audit");

    let proposal = registry::propose_hire(&company, &python_request()).unwrap();
    assert!(proposal.join("contract.yaml").is_file());
    assert!(
        load(&company).unwrap().get("EMP-PY-001").is_none(),
        "not hired yet"
    );

    let err = registry::check_proposal(&company, "EMP-PY-001").unwrap_err();
    assert!(format!("{err:#}").contains("TODO"));

    fill_placeholders(&proposal);
    registry::check_proposal(&company, "EMP-PY-001").unwrap();

    let hired = registry::approve_hire(&company, "EMP-PY-001", &audit_dir).unwrap();
    assert_eq!(hired.contract.manager_id, "EMP-BUILD-001");
    assert!(!proposal.exists(), "proposal must move into the registry");

    let registry = load(&company).unwrap();
    let python = registry.get("EMP-PY-001").unwrap();
    assert_eq!(python.contract.function, EmployeeFunction::Specialist);
    assert_eq!(
        python.contract.model.as_ref().unwrap().model,
        "test-builder"
    );
    assert!(python.has(registry::Capability::WriteImplementation));

    let events = audit_events(&audit_dir.join("hr.jsonl"));
    assert_eq!(events[0]["event"], "HIRE_APPROVED");
    assert_eq!(events[0]["actor"], "OWNER");
}

#[test]
fn invalid_proposal_is_not_hired() {
    let dir = tempfile::tempdir().unwrap();
    let company = sample_company(dir.path());

    let proposal = registry::propose_hire(&company, &python_request()).unwrap();
    fill_placeholders(&proposal);
    let contract = proposal.join("contract.yaml");
    let text = fs::read_to_string(&contract).unwrap();
    fs::write(
        &contract,
        text.replace("- write_implementation", "- review_work"),
    )
    .unwrap();

    let err =
        registry::approve_hire(&company, "EMP-PY-001", &dir.path().join("audit")).unwrap_err();
    assert!(format!("{err:#}").contains("exceeds manager EMP-BUILD-001"));
    assert!(proposal.exists(), "rejected proposal stays in proposals/");
    assert!(load(&company).unwrap().get("EMP-PY-001").is_none());
}

#[test]
fn proposals_cannot_duplicate_or_hire_an_orchestrator() {
    let dir = tempfile::tempdir().unwrap();
    let company = sample_company(dir.path());

    let mut existing = python_request();
    existing.employee_id = "EMP-BUILD-001".to_string();
    assert!(registry::propose_hire(&company, &existing).is_err());

    let mut orchestrator = python_request();
    orchestrator.function = EmployeeFunction::Orchestrator;
    assert!(registry::propose_hire(&company, &orchestrator).is_err());

    let mut unknown_manager = python_request();
    unknown_manager.manager_id = "EMP-NOPE-001".to_string();
    assert!(registry::propose_hire(&company, &unknown_manager).is_err());
}

#[test]
fn status_change_keeps_contract_layout_and_is_audited() {
    let dir = tempfile::tempdir().unwrap();
    let company = sample_company(dir.path());
    let audit_dir = dir.path().join("audit");

    registry::set_status(
        &company,
        "EMP-BUILD-001",
        EmployeeStatus::Suspended,
        &audit_dir,
    )
    .unwrap();

    let registry = load(&company).unwrap();
    assert_eq!(
        registry.get("EMP-BUILD-001").unwrap().contract.status,
        EmployeeStatus::Suspended
    );
    assert!(registry.active(EmployeeFunction::Builder).is_empty());
    let text = fs::read_to_string(company.join("employees/EMP-BUILD-001/contract.yaml")).unwrap();
    assert!(text.starts_with("# test contract"), "comments must survive");

    let events = audit_events(&audit_dir.join("hr.jsonl"));
    assert_eq!(events[0]["event"], "STATUS_CHANGED");
    assert_eq!(events[0]["payload"]["to"], "suspended");

    assert!(
        registry::set_status(
            &company,
            "EMP-ORCH-001",
            EmployeeStatus::Disabled,
            &audit_dir
        )
        .is_err()
    );
}

#[test]
fn models_can_be_changed_and_bad_ones_are_reverted() {
    let dir = tempfile::tempdir().unwrap();
    let company = sample_company(dir.path());
    let audit = dir.path().join("hr");
    let contract = company.join("employees/EMP-REV-001/contract.yaml");
    let before = fs::read_to_string(&contract).unwrap();

    let mut claude =
        ai_team::config::ModelConfig::new(ai_team::config::ProviderKind::Claude, "claude-opus-5-5");
    claude.effort = Some("medium".to_string());
    claude.cost_per_mtok_input = Some(4.0);
    registry::set_model(&company, "EMP-REV-001", &claude, &audit).unwrap();

    let reviewer = Registry::load(company.join("employees")).unwrap();
    let model = reviewer
        .get("EMP-REV-001")
        .unwrap()
        .contract
        .model
        .clone()
        .unwrap();
    assert_eq!(model.provider, ai_team::config::ProviderKind::Claude);
    assert_eq!(model.model, "claude-opus-5-5");
    assert_eq!(model.effort.as_deref(), Some("medium"));
    // Everything outside the model block is kept.
    let after = fs::read_to_string(&contract).unwrap();
    for line in before
        .lines()
        .filter(|line| !line.starts_with(' ') && *line != "model:")
    {
        assert!(after.contains(line), "lost line {line:?}");
    }
    assert!(
        audit_events(&audit.join("hr.jsonl"))
            .iter()
            .any(|e| e["event"] == "MODEL_CHANGED")
    );

    // An option the provider ignores is refused and nothing changes.
    let mut wrong = claude.clone();
    wrong.num_ctx = Some(65536);
    assert!(registry::set_model(&company, "EMP-REV-001", &wrong, &audit).is_err());
    assert_eq!(fs::read_to_string(&contract).unwrap(), after);
    assert!(registry::set_model(&company, "EMP-ORCH-001", &claude, &audit).is_err());
}
