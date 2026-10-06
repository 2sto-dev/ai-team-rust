mod common;

use std::sync::Arc;

use ai_team::{
    BoxFuture,
    agents::{Agent, AgentContext, architect::Architect, builder::Builder, reviewer::Reviewer},
    domain::{AgentOutput, Artifact, ArtifactKind, Role, Stage, WorkOrder},
    orchestrator::Orchestrator,
};
use ai_team::{
    domain::TeamAssignment,
    planner::{Planner, candidates},
    registry::Registry,
};
use anyhow::{Result, bail};
use common::{
    ScriptedProvider, audit_events, contract_yaml, edit_contract, project, sample_company,
    single_audit_file, test_providers, write_employee,
};

const APPROVE: &str = r#"{"decision":"APPROVED","feedback":"looks good"}"#;
const FIX_BUILDER: &str =
    r#"{"decision":"CHANGES_REQUIRED","target":"builder","feedback":"add retry tests"}"#;
const FIX_SPEC: &str = r#"{"decision":"CHANGES_REQUIRED","target":"architect","feedback":"spec has no timeout policy"}"#;

/// Orchestrator staffed from a fresh sample company; audit goes to `<dir>/audit`.
fn registry_orchestrator(dir: &std::path::Path) -> Orchestrator {
    let company = sample_company(dir);
    Orchestrator::from_registry_with_providers(
        Registry::load(company.join("employees")).unwrap(),
        test_providers(),
    )
    .unwrap()
    .with_audit_dir(dir.join("audit"))
}

fn scripted_orchestrator(
    architect: &ScriptedProvider,
    builder: &ScriptedProvider,
    reviewer: &ScriptedProvider,
    audit_dir: &std::path::Path,
) -> Orchestrator {
    Orchestrator::with_agents(
        Box::new(Architect::new(
            "Architect",
            "sys",
            Arc::new(architect.clone()),
        )),
        Box::new(Builder::new("Builder", "sys", Arc::new(builder.clone()))),
        Box::new(Reviewer::new("Reviewer", "sys", Arc::new(reviewer.clone()))),
    )
    .with_audit_dir(audit_dir)
}

#[tokio::test]
async fn correction_cycle_retries_then_approves() {
    let dir = tempfile::tempdir().unwrap();
    let orchestrator = registry_orchestrator(dir.path());

    let state = orchestrator
        .run(&project(4), "Build a test component".to_string())
        .await
        .expect("workflow should succeed");

    assert_eq!(state.stage, Stage::Done);
    assert_eq!(state.iteration, 2);
    assert!(
        state
            .history
            .iter()
            .any(|item| item.contains("CHANGES_REQUIRED"))
    );
    assert!(state.history.iter().any(|item| item.contains("APPROVED")));
    assert!(std::path::Path::new(&state.audit_file).starts_with(dir.path().join("audit")));
}

#[tokio::test]
async fn iteration_limit_escalates_to_human() {
    let dir = tempfile::tempdir().unwrap();
    let orchestrator = registry_orchestrator(dir.path());

    let state = orchestrator
        .run(&project(1), "Build a test component".to_string())
        .await
        .expect("workflow should finish with escalation");

    assert_eq!(state.stage, Stage::HumanReviewRequired);
    assert_eq!(state.iteration, 1);
}

#[tokio::test]
async fn builder_revises_previous_implementation_with_full_project_context() {
    let dir = tempfile::tempdir().unwrap();
    let architect = ScriptedProvider::new(["SPEC-V1"]);
    let builder = ScriptedProvider::new(["IMPL-V1", "IMPL-V2"]);
    let reviewer = ScriptedProvider::new([FIX_BUILDER, APPROVE]);

    let state = scripted_orchestrator(&architect, &builder, &reviewer, dir.path())
        .run(&project(3), "Build it".to_string())
        .await
        .unwrap();

    assert_eq!(state.stage, Stage::Done);
    assert_eq!(state.implementation.as_ref().unwrap().content, "IMPL-V2");

    let builder_prompts = builder.prompts();
    assert!(builder_prompts[0].contains("none (first iteration)"));
    let second = &builder_prompts[1];
    assert!(
        second.contains("IMPL-V1"),
        "builder must see its previous implementation"
    );
    assert!(
        second.contains("add retry tests"),
        "builder must see reviewer feedback"
    );
    assert!(
        second.contains("Keep contracts explicit."),
        "builder must see project rules"
    );
    assert!(
        second.contains("Reviewer must approve."),
        "builder must see acceptance criteria"
    );
    assert!(
        second.contains("Test the reusable team workflow."),
        "builder must see objective"
    );

    let reviewer_prompts = reviewer.prompts();
    assert!(reviewer_prompts[0].contains("Test the reusable team workflow."));
    assert!(
        reviewer_prompts[1].contains("add retry tests"),
        "reviewer sees its previous feedback"
    );
}

#[tokio::test]
async fn reviewer_can_send_specification_back_to_architect() {
    let dir = tempfile::tempdir().unwrap();
    let architect = ScriptedProvider::new(["SPEC-V1", "SPEC-V2"]);
    let builder = ScriptedProvider::new(["IMPL-V1", "IMPL-V2"]);
    let reviewer = ScriptedProvider::new([FIX_SPEC, APPROVE]);

    let state = scripted_orchestrator(&architect, &builder, &reviewer, dir.path())
        .run(&project(3), "Build it".to_string())
        .await
        .unwrap();

    assert_eq!(state.stage, Stage::Done);
    assert_eq!(state.architecture_revisions, 1);
    let spec = state.specification.as_ref().unwrap();
    assert_eq!((spec.revision, spec.content.as_str()), (2, "SPEC-V2"));

    let revision_prompt = &architect.prompts()[1];
    assert!(revision_prompt.contains("SPEC-V1"));
    assert!(revision_prompt.contains("spec has no timeout policy"));
    assert!(builder.prompts()[1].contains("SPECIFICATION (revision 2)"));
}

#[tokio::test]
async fn architecture_revision_limit_falls_back_to_builder() {
    let dir = tempfile::tempdir().unwrap();
    let architect = ScriptedProvider::new(["SPEC-V1", "SPEC-V2"]);
    let builder = ScriptedProvider::new(["IMPL-V1", "IMPL-V2", "IMPL-V3"]);
    let reviewer = ScriptedProvider::new([FIX_SPEC, FIX_SPEC, APPROVE]);

    let state = scripted_orchestrator(&architect, &builder, &reviewer, dir.path())
        .run(&project(3), "Build it".to_string())
        .await
        .unwrap();

    assert_eq!(state.stage, Stage::Done);
    assert_eq!(state.architecture_revisions, 1, "only one revision allowed");
    assert_eq!(architect.prompts().len(), 2);
    assert!(
        state
            .history
            .iter()
            .any(|item| item.contains("revision limit reached"))
    );
}

#[tokio::test]
async fn agent_failure_is_recorded_in_audit() {
    let dir = tempfile::tempdir().unwrap();
    let architect = ScriptedProvider::new(["SPEC-V1"]);
    let builder = ScriptedProvider::new(Vec::<String>::new()); // fails on first call
    let reviewer = ScriptedProvider::new([APPROVE]);

    let err = scripted_orchestrator(&architect, &builder, &reviewer, dir.path())
        .run(&project(3), "Build it".to_string())
        .await
        .expect_err("builder failure must fail the run");
    assert!(format!("{err:#}").contains("Builder failed"));

    let events = audit_events(&single_audit_file(dir.path()));
    let last = events.last().unwrap();
    assert_eq!(last["event"], "RUN_FAILED");
    assert_eq!(last["payload"]["stage"], "failed");
    assert!(
        last["payload"]["error"]
            .as_str()
            .unwrap()
            .contains("no answer left")
    );
}

#[tokio::test]
async fn invalid_reviewer_output_never_approves() {
    let dir = tempfile::tempdir().unwrap();
    let architect = ScriptedProvider::new(["SPEC-V1"]);
    let builder = ScriptedProvider::new(["IMPL-V1"]);
    // Initial reply plus both format retries are malformed.
    let reviewer = ScriptedProvider::new(["Sure, APPROVED!", "APPROVED", "{approved: yes}"]);

    let err = scripted_orchestrator(&architect, &builder, &reviewer, dir.path())
        .run(&project(3), "Build it".to_string())
        .await
        .expect_err("non-JSON review must fail closed");
    assert!(format!("{err:#}").contains("approval is denied"));
    assert_eq!(
        reviewer.prompts().len(),
        3,
        "asked again twice, then gave up"
    );
}

#[tokio::test]
async fn reviewer_is_asked_again_after_a_malformed_reply() {
    let dir = tempfile::tempdir().unwrap();
    let architect = ScriptedProvider::new(["SPEC-V1"]);
    let builder = ScriptedProvider::new(["IMPL-V1"]);
    let reviewer = ScriptedProvider::new(["Looks good to me!", APPROVE]);

    let state = scripted_orchestrator(&architect, &builder, &reviewer, dir.path())
        .run(&project(3), "Build it".to_string())
        .await
        .unwrap();

    assert_eq!(state.stage, Stage::Done);
    let retry_prompt = &reviewer.prompts()[1];
    assert!(retry_prompt.contains("YOUR PREVIOUS REPLY WAS REJECTED"));
    assert!(retry_prompt.contains("Looks good to me!"));
}

/// A department lead that splits work across its own subagents.
struct BuilderLead {
    subagents: Vec<Box<dyn Agent>>,
}

impl Agent for BuilderLead {
    fn role(&self) -> Role {
        Role::Builder
    }

    fn name(&self) -> &str {
        "BuilderLead"
    }

    fn model_name(&self) -> &str {
        "composite"
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a AgentContext,
        order: &'a WorkOrder,
    ) -> BoxFuture<'a, Result<AgentOutput>> {
        Box::pin(async move {
            let mut parts = Vec::new();
            for subagent in &self.subagents {
                let sub_ctx = ctx.child(subagent.name());
                match subagent.execute(&sub_ctx, order).await? {
                    AgentOutput::Artifact(artifact) => {
                        sub_ctx.record("SUBTASK_DONE", &artifact)?;
                        parts.push(artifact.content);
                    }
                    other => bail!("subagent returned {}", other.describe()),
                }
            }
            Ok(AgentOutput::Artifact(Artifact {
                kind: ArtifactKind::Implementation,
                author: self.name().to_string(),
                revision: order.iteration,
                content: parts.join("\n---\n"),
            }))
        })
    }
}

#[tokio::test]
async fn lead_can_delegate_to_subagents_with_nested_audit_spans() {
    let dir = tempfile::tempdir().unwrap();
    let lead = BuilderLead {
        subagents: vec![
            Box::new(Builder::new(
                "Backend",
                "sys",
                Arc::new(ScriptedProvider::new(["API"])),
            )),
            Box::new(Builder::new(
                "Frontend",
                "sys",
                Arc::new(ScriptedProvider::new(["UI"])),
            )),
        ],
    };
    let orchestrator = Orchestrator::with_agents(
        Box::new(Architect::new(
            "Architect",
            "sys",
            Arc::new(ScriptedProvider::new(["SPEC"])),
        )),
        Box::new(lead),
        Box::new(Reviewer::new(
            "Reviewer",
            "sys",
            Arc::new(ScriptedProvider::new([APPROVE])),
        )),
    )
    .with_audit_dir(dir.path());

    let state = orchestrator
        .run(&project(2), "Build it".to_string())
        .await
        .unwrap();
    assert_eq!(state.stage, Stage::Done);
    assert_eq!(
        state.implementation.as_ref().unwrap().content,
        "API\n---\nUI"
    );

    let events = audit_events(&single_audit_file(dir.path()));
    let lead_span = events
        .iter()
        .find(|event| event["actor"] == "BuilderLead" && event["event"] == "AGENT_STARTED")
        .unwrap()["span_id"]
        .clone();
    let root_span = events[0]["span_id"].clone();

    for name in ["Backend", "Frontend"] {
        let sub = events
            .iter()
            .find(|event| event["actor"] == name && event["event"] == "SUBTASK_DONE")
            .unwrap_or_else(|| panic!("missing audit event for {name}"));
        assert_eq!(
            sub["parent_span_id"], lead_span,
            "{name} must be a child of the lead"
        );
    }
    let lead_started = events
        .iter()
        .find(|event| event["actor"] == "BuilderLead")
        .unwrap();
    assert_eq!(lead_started["parent_span_id"], root_span);
}

// ---------------------------------------------------------------------------
// Staffing from the employee registry
// ---------------------------------------------------------------------------

fn load_registry(company: &std::path::Path) -> Registry {
    Registry::load(company.join("employees")).unwrap()
}

fn sample_team() -> TeamAssignment {
    TeamAssignment {
        architect: "EMP-ARCH-001".to_string(),
        builder: "EMP-BUILD-001".to_string(),
        reviewer: "EMP-REV-001".to_string(),
    }
}

#[tokio::test]
async fn planner_staffs_the_run_from_the_registry() {
    let dir = tempfile::tempdir().unwrap();
    let state = registry_orchestrator(dir.path())
        .run(&project(3), "Build it".to_string())
        .await
        .unwrap();

    assert_eq!(state.stage, Stage::Done);
    assert_eq!(state.team, Some(sample_team()));

    let events = audit_events(&single_audit_file(&dir.path().join("audit")));
    let names: Vec<_> = events
        .iter()
        .map(|event| event["event"].as_str().unwrap())
        .collect();
    let proposed = names
        .iter()
        .position(|name| *name == "PLAN_PROPOSED")
        .unwrap();
    let accepted = names
        .iter()
        .position(|name| *name == "TEAM_ACCEPTED")
        .unwrap();
    let first_spec = names
        .iter()
        .position(|name| *name == "ARCHITECTURE_READY")
        .unwrap();
    assert!(
        proposed < accepted && accepted < first_spec,
        "plan → team → work: {names:?}"
    );

    // Audit actors are employee ids, as required by the company audit rules.
    for actor in [
        "EMP-PLAN-001",
        "EMP-ARCH-001",
        "EMP-BUILD-001",
        "EMP-REV-001",
    ] {
        assert!(
            events.iter().any(|event| event["actor"] == actor),
            "missing {actor}"
        );
    }
}

#[tokio::test]
async fn owner_assigned_team_needs_no_planner() {
    let dir = tempfile::tempdir().unwrap();
    let company = sample_company(dir.path());
    std::fs::remove_dir_all(company.join("employees/EMP-PLAN-001")).unwrap();
    let orchestrator = || {
        Orchestrator::from_registry_with_providers(load_registry(&company), test_providers())
            .unwrap()
            .with_audit_dir(dir.path().join("audit"))
    };

    let err = orchestrator()
        .run(&project(3), "Build it".to_string())
        .await
        .expect_err("no planner and no assigned team");
    assert!(format!("{err:#}").contains("no active planner"));

    let mut assigned = project(3);
    assigned.assigned_team = Some(sample_team());
    let state = orchestrator()
        .run(&assigned, "Build it".to_string())
        .await
        .unwrap();
    assert_eq!(state.stage, Stage::Done);
    assert!(
        state
            .history
            .iter()
            .any(|item| item.contains("owner: team assigned"))
    );
}

#[tokio::test]
async fn control_plane_rejects_inactive_or_wrong_employees() {
    let dir = tempfile::tempdir().unwrap();
    let company = sample_company(dir.path());
    edit_contract(
        &company,
        "EMP-REV-001",
        "status: active",
        "status: suspended",
    );
    write_employee(
        &company.join("employees"),
        "EMP-REV-002",
        &contract_yaml(
            "EMP-REV-002",
            "agent",
            "reviewer",
            "EMP-ORCH-001",
            &["review_work"],
            Some("m"),
        ),
    );
    let audit = dir.path().join("audit");

    let mut project = project(3);
    project.assigned_team = Some(TeamAssignment {
        architect: "EMP-BUILD-001".to_string(), // wrong function
        builder: "EMP-BUILD-001".to_string(),
        reviewer: "EMP-REV-001".to_string(), // suspended
    });

    let err = Orchestrator::from_registry_with_providers(load_registry(&company), test_providers())
        .unwrap()
        .with_audit_dir(&audit)
        .run(&project, "Build it".to_string())
        .await
        .expect_err("invalid team must not run");
    let message = format!("{err:#}");
    assert!(message.contains("rejected by control plane"));
    assert!(message.contains("EMP-BUILD-001 is a builder"));
    assert!(message.contains("EMP-REV-001 is suspended"));

    let events = audit_events(&single_audit_file(&audit));
    let names: Vec<_> = events
        .iter()
        .map(|event| event["event"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"PLAN_REJECTED"));
    assert_eq!(names.last(), Some(&"RUN_FAILED"));
    assert!(
        !names.contains(&"AGENT_STARTED"),
        "no agent may work for a rejected team"
    );
}

#[tokio::test]
async fn shared_builder_reviewer_model_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let company = sample_company(dir.path());
    edit_contract(
        &company,
        "EMP-BUILD-001",
        "model: test-builder",
        "model: shared",
    );
    edit_contract(
        &company,
        "EMP-REV-001",
        "model: test-reviewer",
        "model: shared",
    );

    let state =
        Orchestrator::from_registry_with_providers(load_registry(&company), test_providers())
            .unwrap()
            .with_audit_dir(dir.path().join("audit"))
            .run(&project(3), "Build it".to_string())
            .await
            .unwrap();

    assert_eq!(state.stage, Stage::Done);
    assert!(
        state
            .history
            .iter()
            .any(|item| item.starts_with("policy warning"))
    );
}

#[tokio::test]
async fn planner_sees_candidates_and_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let registry = load_registry(&sample_company(dir.path()));
    let candidates = candidates(&registry);
    assert_eq!(
        candidates.len(),
        3,
        "planner and orchestrator are not candidates"
    );

    let scripted = ScriptedProvider::new([
        r#"Plan: {"team":{"architect":"EMP-ARCH-001","builder":"EMP-BUILD-001","reviewer":"EMP-REV-001"},"rationale":"skills match"}"#,
        "Use the usual team.",
    ]);
    let planner = Planner::new("EMP-PLAN-001", "sys", Arc::new(scripted.clone()));

    let plan = planner
        .propose(&project(3), "Build it", &candidates, None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(plan.team, sample_team());
    assert!(scripted.prompts()[0].contains("- EMP-REV-001 | reviewer"));

    let invalid = planner
        .propose(&project(3), "Build it", &candidates, None)
        .await
        .expect("model reachable")
        .expect_err("prose is not a plan");
    assert!(invalid.contains("planner returned invalid JSON"));
}

#[tokio::test]
async fn rejected_plan_is_sent_back_to_the_planner() {
    let dir = tempfile::tempdir().unwrap();
    let company = sample_company(dir.path());
    // The exact mistake a small model made in a real run: a mangled employee_id.
    let scripted = ScriptedProvider::new([
        r#"{"team":{"architect":"EMP-ARCH-001","builder":"EMP-BUILD-0-01","reviewer":"EMP-REV-001"}}"#,
        r#"{"team":{"architect":"EMP-ARCH-001","builder":"EMP-BUILD-001","reviewer":"EMP-REV-001"}}"#,
    ]);

    let state =
        Orchestrator::from_registry_with_providers(load_registry(&company), test_providers())
            .unwrap()
            .with_planner(Planner::new(
                "EMP-PLAN-001",
                "sys",
                Arc::new(scripted.clone()),
            ))
            .with_audit_dir(dir.path().join("audit"))
            .run(&project(3), "Build it".to_string())
            .await
            .unwrap();

    assert_eq!(state.stage, Stage::Done);
    assert_eq!(state.team, Some(sample_team()));
    assert!(
        state
            .history
            .iter()
            .any(|item| item.contains("plan rejected (attempt 1/3)"))
    );
    let second_prompt = &scripted.prompts()[1];
    assert!(second_prompt.contains("REJECTED BY THE CONTROL PLANE"));
    assert!(second_prompt.contains("EMP-BUILD-0-01"));
}

#[tokio::test]
async fn planner_gives_up_after_three_rejected_plans() {
    let dir = tempfile::tempdir().unwrap();
    let company = sample_company(dir.path());
    let bad = r#"{"team":{"architect":"EMP-ARCH-001","builder":"EMP-NOPE-001","reviewer":"EMP-REV-001"}}"#;
    let scripted = ScriptedProvider::new([bad, "not json", bad]);
    let audit = dir.path().join("audit");

    let err = Orchestrator::from_registry_with_providers(load_registry(&company), test_providers())
        .unwrap()
        .with_planner(Planner::new("EMP-PLAN-001", "sys", Arc::new(scripted)))
        .with_audit_dir(&audit)
        .run(&project(3), "Build it".to_string())
        .await
        .expect_err("three invalid plans must fail the run");
    assert!(format!("{err:#}").contains("no valid plan after 3 attempts"));

    let events = audit_events(&single_audit_file(&audit));
    let rejected = events
        .iter()
        .filter(|event| event["event"] == "PLAN_REJECTED")
        .count();
    assert_eq!(rejected, 3);
    assert!(
        !events.iter().any(|event| event["event"] == "TEAM_ACCEPTED"),
        "no team may be accepted"
    );
}
