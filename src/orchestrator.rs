use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde_json::json;
use uuid::Uuid;

use crate::{
    agents::{Agent, AgentContext},
    audit::{AuditTrail, default_audit_dir},
    domain::{
        AgentOutput, Artifact, ArtifactKind, ProjectConfig, ReviewDecision, ReviewResult,
        ReviewTarget, Role, Stage, TeamAssignment, TeamState, WorkOrder,
    },
    llm::{ProviderFactory, UsageLedger, UsageTotals, http_providers, metered},
    planner::{Planner, candidates},
    registry::{EmployeeFunction, Registry},
    staffing::{self, Team, model_of},
    workbench::Workbench,
};

/// Planner proposals per run before the run fails (each rejection is fed back).
const MAX_PLAN_ATTEMPTS: u32 = 3;

/// Called after every step of a run with the current state (the executor persists it).
pub type Checkpoint<'a> = &'a (dyn Fn(&TeamState) -> Result<()> + Sync);

/// Outcome of a persisted run: the final state is kept even when `error` is set.
pub struct RunResult {
    pub state: TeamState,
    pub error: Option<anyhow::Error>,
}

pub struct Orchestrator {
    staffing: Staffing,
    audit_dir: PathBuf,
}

enum Staffing {
    /// Same agents for every run (tests, hand-built composite leads).
    Fixed(Team),
    /// Team chosen per run from the employee registry.
    Company(CompanyStaff),
}

struct CompanyStaff {
    registry: Registry,
    planner: Option<Planner>,
    providers: ProviderFactory,
    /// Every model call made through `providers` (and the planner) lands here.
    ledger: UsageLedger,
}

impl Orchestrator {
    /// Staffs every run from the registry: the project's `assigned_team` (Owner decision) or
    /// else the Planner's proposal, validated by the control plane.
    pub fn from_registry(registry: Registry) -> Result<Self> {
        Self::from_registry_with_providers(registry, http_providers())
    }

    /// Same as [`Orchestrator::from_registry`], with the LLM backends built by `providers`
    /// (tests pass offline providers here).
    pub fn from_registry_with_providers(
        registry: Registry,
        providers: ProviderFactory,
    ) -> Result<Self> {
        let ledger = UsageLedger::default();
        let providers = metered(providers, ledger.clone());
        let planner = match registry.active(EmployeeFunction::Planner).first() {
            Some(employee) => Some(Planner::new(
                employee.id(),
                employee.job_description.clone(),
                providers(Role::Planner, model_of(employee)?)?,
            )),
            None => None,
        };

        Ok(Self {
            staffing: Staffing::Company(CompanyStaff {
                registry,
                planner,
                providers,
                ledger,
            }),
            audit_dir: default_audit_dir(),
        })
    }

    /// Plug in any `Agent` implementation per role, e.g. a lead that delegates to subagents.
    pub fn with_agents(
        architect: Box<dyn Agent>,
        builder: Box<dyn Agent>,
        reviewer: Box<dyn Agent>,
    ) -> Self {
        Self {
            staffing: Staffing::Fixed(Team {
                architect,
                builder,
                reviewer,
            }),
            audit_dir: default_audit_dir(),
        }
    }

    /// Replaces the registry's planner (tests, or a planner on a different model).
    /// Has no effect on an orchestrator built with `with_agents`.
    pub fn with_planner(mut self, replacement: Planner) -> Self {
        if let Staffing::Company(company) = &mut self.staffing {
            company.planner = Some(replacement);
        }
        self
    }

    /// Model usage recorded since the last call (planning outside a run, for instance).
    pub fn take_usage(&self) -> UsageTotals {
        match &self.staffing {
            Staffing::Company(company) => company.ledger.take_totals(),
            Staffing::Fixed(_) => UsageTotals::default(),
        }
    }

    fn collect_usage(&self, state: &mut TeamState) {
        state.usage.add(&self.take_usage());
    }

    /// Records a step in the audit trail (with usage so far), then lets the caller persist it.
    fn mark(
        &self,
        ctx: &AgentContext,
        event: &str,
        state: &mut TeamState,
        checkpoint: Checkpoint<'_>,
    ) -> Result<()> {
        self.collect_usage(state);
        ctx.record(event, state)?;
        checkpoint(state)
    }

    /// The employee registry, if the orchestrator staffs from one.
    pub fn registry(&self) -> Option<&Registry> {
        match &self.staffing {
            Staffing::Company(company) => Some(&company.registry),
            Staffing::Fixed(_) => None,
        }
    }

    /// The registry's planner, if the orchestrator staffs from a registry and has one.
    pub fn planner(&self) -> Option<&Planner> {
        match &self.staffing {
            Staffing::Company(company) => company.planner.as_ref(),
            Staffing::Fixed(_) => None,
        }
    }

    pub fn with_audit_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.audit_dir = dir.into();
        self
    }

    /// Runs one task from scratch. Ends in `Done` or `HumanReviewRequired`; any agent or
    /// protocol error is recorded as `RUN_FAILED` in the audit trail and returned as `Err`.
    pub async fn run(&self, project: &ProjectConfig, task: String) -> Result<TeamState> {
        let task_id = format!(
            "{}-{}",
            project.project_id,
            &Uuid::new_v4().simple().to_string()[..8]
        );
        let result = self.start(project, task_id, task, &|_| Ok(()), None).await;
        match result.error {
            None => Ok(result.state),
            Some(err) => Err(err),
        }
    }

    /// Starts a persisted task. `checkpoint` is called after every step so the caller can
    /// save progress. The final state is returned even when the run fails.
    pub async fn start(
        &self,
        project: &ProjectConfig,
        task_id: String,
        task: String,
        checkpoint: Checkpoint<'_>,
        workbench: Option<&dyn Workbench>,
    ) -> RunResult {
        let state = TeamState::new(
            String::new(),
            project.project_id.clone(),
            task_id,
            task,
            String::new(),
        );
        self.execute(project, state, "RUN_CREATED", checkpoint, workbench)
            .await
    }

    /// Continues a saved task (after an interruption, an Owner decision or a failure). Keeps
    /// its team, specification and last implementation; runs under a new run id and audit file.
    pub async fn resume(
        &self,
        project: &ProjectConfig,
        mut state: TeamState,
        checkpoint: Checkpoint<'_>,
        workbench: Option<&dyn Workbench>,
    ) -> RunResult {
        state.error = None;
        state.history.push("orchestrator: resumed".to_string());
        self.execute(project, state, "RUN_RESUMED", checkpoint, workbench)
            .await
    }

    async fn execute(
        &self,
        project: &ProjectConfig,
        mut state: TeamState,
        first_event: &str,
        checkpoint: Checkpoint<'_>,
        workbench: Option<&dyn Workbench>,
    ) -> RunResult {
        let run_id = Uuid::new_v4().to_string();
        let audit = match AuditTrail::create(&self.audit_dir, &run_id) {
            Ok(audit) => audit,
            Err(err) => {
                return RunResult {
                    state,
                    error: Some(err),
                };
            }
        };
        let audit_file = audit.path().display().to_string();
        let root = AgentContext::root(&run_id, audit);
        state.run_id = run_id.clone();
        state.audit_file = audit_file.clone();

        let outcome = match root.record(first_event, &state) {
            Ok(()) => {
                self.drive(&root, project, &mut state, checkpoint, workbench)
                    .await
            }
            Err(err) => Err(err),
        };

        match outcome {
            Ok(()) => RunResult { state, error: None },
            Err(err) => {
                self.collect_usage(&mut state);
                state.stage = Stage::Failed;
                state.error = Some(format!("{err:#}"));
                state.history.push(format!("orchestrator: FAILED ({err})"));
                if let Err(audit_err) = root.record("RUN_FAILED", &state) {
                    tracing::error!(error = %format!("{audit_err:#}"), "cannot record RUN_FAILED");
                }
                if let Err(save_err) = checkpoint(&state) {
                    tracing::error!(error = %format!("{save_err:#}"), "cannot save failed state");
                }
                RunResult {
                    state,
                    error: Some(err.context(format!("run {run_id} failed; audit: {audit_file}"))),
                }
            }
        }
    }

    async fn drive(
        &self,
        root: &AgentContext,
        project: &ProjectConfig,
        state: &mut TeamState,
        checkpoint: Checkpoint<'_>,
        workbench: Option<&dyn Workbench>,
    ) -> Result<()> {
        let mut staffed = None;
        let team: &Team = match &self.staffing {
            Staffing::Fixed(team) => team,
            Staffing::Company(company) => staffed.insert(
                self.staff(root, company, project, state, checkpoint)
                    .await?,
            ),
        };

        // The platform acts in a workspace only for a Builder entitled to it.
        if let (Some(workbench), Staffing::Company(company), Some(team)) =
            (workbench, &self.staffing, &state.team)
        {
            let builder = company
                .registry
                .get(&team.builder)
                .with_context(|| format!("unknown builder {}", team.builder))?;
            for capability in workbench.required_capabilities() {
                anyhow::ensure!(
                    builder.has(capability),
                    "builder {} lacks {capability}, which this task's workspace requires",
                    team.builder
                );
            }
        }

        // A resumed task keeps its specification unless a revision was interrupted.
        if state.specification.is_none() || state.stage == Stage::ArchitectureRevisionRequired {
            self.specify(team, root, project, state, checkpoint).await?;
        }

        while state.iteration < project.max_iterations {
            // The iteration counts only once the Builder delivered: a failed call (model down,
            // answer cut off) must not use up the task's budget.
            let mut order = work_order(project, state);
            order.iteration = state.iteration + 1;
            if let Some(workbench) = workbench {
                order.workspace = Some(workbench.briefing()?);
            }
            let (ctx, output) = self.invoke(team.builder.as_ref(), root, &order).await?;
            let implementation =
                expect_artifact(output, ArtifactKind::Implementation, Role::Builder)?;
            state.iteration = order.iteration;
            state.implementation = Some(implementation);
            state.stage = Stage::Implemented;
            state
                .history
                .push(format!("builder: iteration {} ready", state.iteration));
            self.mark(&ctx, "IMPLEMENTATION_READY", state, checkpoint)?;

            // Approval gate 1: the platform applies the work and runs the tests. Work that
            // fails never reaches the Reviewer; the Builder gets the evidence instead.
            if let Some(workbench) = workbench {
                let implementation = state
                    .implementation
                    .as_ref()
                    .context("orchestrator invariant failed: implementation missing")?;
                let verification = workbench
                    .verify(implementation)
                    .await
                    .context("verification could not run")?;
                for file in &verification.files_written {
                    if !state.written_files.contains(file) {
                        state.written_files.push(file.clone());
                    }
                }
                let passed = verification.passed();
                state.verification = Some(verification.clone());

                if !passed {
                    state.review = Some(ReviewResult {
                        decision: ReviewDecision::ChangesRequired,
                        target: ReviewTarget::Builder,
                        feedback: format!(
                            "Automated verification by the platform failed; fix this first:\n{}",
                            verification.report()
                        ),
                    });
                    state.stage = Stage::ChangesRequired;
                    state
                        .history
                        .push("platform: verification FAILED".to_string());
                    self.mark(root, "VERIFICATION_FAILED", state, checkpoint)?;
                    continue;
                }
                state.history.push(format!(
                    "platform: verification passed ({} file(s){})",
                    verification.files_written.len(),
                    if verification.test.is_some() {
                        ", tests green"
                    } else {
                        ""
                    }
                ));
                self.mark(root, "VERIFICATION_PASSED", state, checkpoint)?;
            }

            state.stage = Stage::Reviewing;
            let (ctx, output) = self
                .invoke(team.reviewer.as_ref(), root, &work_order(project, state))
                .await?;
            let review = expect_review(output)?;
            state.review = Some(review.clone());
            self.mark(&ctx, "REVIEW_COMPLETED", state, checkpoint)?;

            let can_revise_architecture = state.architecture_revisions
                < project.max_architecture_revisions
                && state.iteration < project.max_iterations;

            match (review.decision, review.target) {
                (ReviewDecision::Approved, _) => {
                    state.stage = Stage::Approved;
                    state.history.push("reviewer: APPROVED".to_string());
                    root.record("APPROVED", state)?;

                    state.stage = Stage::Done;
                    state.history.push("orchestrator: DONE".to_string());
                    self.mark(root, "DONE", state, checkpoint)?;
                    return Ok(());
                }
                (ReviewDecision::ChangesRequired, ReviewTarget::Architect)
                    if can_revise_architecture =>
                {
                    state.stage = Stage::ArchitectureRevisionRequired;
                    state
                        .history
                        .push("reviewer: CHANGES_REQUIRED (architecture)".to_string());
                    state.architecture_revisions += 1;
                    self.mark(root, "ARCHITECTURE_REVISION_REQUIRED", state, checkpoint)?;
                    self.specify(team, root, project, state, checkpoint).await?;
                }
                (ReviewDecision::ChangesRequired, target) => {
                    state.stage = Stage::ChangesRequired;
                    state.history.push(match target {
                        ReviewTarget::Builder => "reviewer: CHANGES_REQUIRED".to_string(),
                        ReviewTarget::Architect => {
                            "reviewer: CHANGES_REQUIRED (architecture revision limit reached, sent to builder)"
                                .to_string()
                        }
                    });
                    self.mark(root, "CHANGES_REQUIRED", state, checkpoint)?;
                }
            }
        }

        state.stage = Stage::HumanReviewRequired;
        state
            .history
            .push("orchestrator: HUMAN_REVIEW_REQUIRED".to_string());
        self.mark(root, "HUMAN_REVIEW_REQUIRED", state, checkpoint)?;
        Ok(())
    }

    /// Asks the Planner for a team until the control plane accepts one. Each rejection reason
    /// is sent back to the Planner; a rejected plan is never executed.
    async fn plan_team(
        &self,
        root: &AgentContext,
        registry: &Registry,
        planner: &Planner,
        project: &ProjectConfig,
        state: &mut TeamState,
    ) -> Result<TeamAssignment> {
        let ctx = root.child(planner.employee_id());
        ctx.record(
            "AGENT_STARTED",
            &json!({ "role": Role::Planner, "model": planner.model_name() }),
        )?;

        let candidates = candidates(registry);
        let mut rejection: Option<String> = None;

        for attempt in 1..=MAX_PLAN_ATTEMPTS {
            let proposal = planner
                .propose(project, &state.task, &candidates, rejection.as_deref())
                .await
                .context("Planner failed")?;

            let error = match proposal {
                Ok(plan) => {
                    ctx.record("PLAN_PROPOSED", &plan)?;
                    state.history.push(format!(
                        "planner: team proposed ({}, {}, {})",
                        plan.team.architect, plan.team.builder, plan.team.reviewer
                    ));
                    match staffing::validate_assignment(registry, &plan.team) {
                        Ok(()) => return Ok(plan.team),
                        Err(err) => {
                            let error = format!("{err:#}");
                            ctx.record(
                                "PLAN_REJECTED",
                                &json!({ "attempt": attempt, "team": plan.team, "error": error }),
                            )?;
                            error
                        }
                    }
                }
                Err(invalid) => {
                    ctx.record(
                        "PLAN_REJECTED",
                        &json!({ "attempt": attempt, "error": invalid }),
                    )?;
                    invalid
                }
            };

            state.history.push(format!(
                "orchestrator: plan rejected (attempt {attempt}/{MAX_PLAN_ATTEMPTS}): {error}"
            ));
            rejection = Some(error);
        }

        bail!(
            "no valid plan after {MAX_PLAN_ATTEMPTS} attempts; last rejection: {}",
            rejection.unwrap_or_default()
        )
    }

    /// Chooses the team for this run and builds its agents.
    async fn staff(
        &self,
        root: &AgentContext,
        company: &CompanyStaff,
        project: &ProjectConfig,
        state: &mut TeamState,
        checkpoint: Checkpoint<'_>,
    ) -> Result<Team> {
        let CompanyStaff {
            registry,
            planner,
            providers,
            ..
        } = company;
        let planner = planner.as_ref();

        // A resumed task keeps its team while every member is still eligible.
        if let Some(saved) = state.team.clone() {
            match staffing::validate_assignment(registry, &saved) {
                Ok(()) => {
                    state
                        .history
                        .push("orchestrator: team kept from the previous run".to_string());
                    root.record("TEAM_KEPT", &saved)?;
                    return staffing::build_team(registry, &saved, providers);
                }
                Err(err) => {
                    state.history.push(format!(
                        "orchestrator: previous team no longer valid ({err:#}); staffing again"
                    ));
                    state.team = None;
                }
            }
        }

        let assignment = match &project.assigned_team {
            // The Owner's decision is validated but never renegotiated.
            Some(assignment) => {
                state
                    .history
                    .push("owner: team assigned in project".to_string());
                if let Err(err) = staffing::validate_assignment(registry, assignment) {
                    root.record(
                        "PLAN_REJECTED",
                        &json!({ "team": assignment, "error": format!("{err:#}") }),
                    )?;
                    return Err(err);
                }
                assignment.clone()
            }
            None => {
                let planner = planner.context(
                    "no active planner in the registry and the project has no assigned_team",
                )?;
                self.plan_team(root, registry, planner, project, state)
                    .await?
            }
        };

        if let Some(warning) = staffing::same_model_warning(registry, &assignment) {
            tracing::warn!(%warning, "independence policy");
            state.history.push(format!("policy warning: {warning}"));
            root.record("POLICY_WARNING", &json!({ "warning": warning }))?;
        }

        let team = staffing::build_team(registry, &assignment, providers)?;
        state.team = Some(assignment);
        state
            .history
            .push("orchestrator: team accepted".to_string());
        self.mark(root, "TEAM_ACCEPTED", state, checkpoint)?;
        Ok(team)
    }

    /// Initial specification, or a revision when one already exists.
    async fn specify(
        &self,
        team: &Team,
        root: &AgentContext,
        project: &ProjectConfig,
        state: &mut TeamState,
        checkpoint: Checkpoint<'_>,
    ) -> Result<()> {
        let (ctx, output) = self
            .invoke(team.architect.as_ref(), root, &work_order(project, state))
            .await?;
        let spec = expect_artifact(output, ArtifactKind::Specification, Role::Architect)?;

        state.history.push(if spec.revision == 1 {
            "architect: specification ready".to_string()
        } else {
            format!("architect: specification revision {} ready", spec.revision)
        });
        state.specification = Some(spec);
        state.stage = Stage::ArchitectureReady;
        self.mark(&ctx, "ARCHITECTURE_READY", state, checkpoint)?;
        Ok(())
    }

    async fn invoke(
        &self,
        agent: &dyn Agent,
        root: &AgentContext,
        order: &WorkOrder,
    ) -> Result<(AgentContext, AgentOutput)> {
        let ctx = root.child(agent.name());
        ctx.record(
            "AGENT_STARTED",
            &json!({
                "role": agent.role(),
                "model": agent.model_name(),
                "iteration": order.iteration,
            }),
        )?;

        let output = agent
            .execute(&ctx, order)
            .await
            .with_context(|| format!("{} failed", agent.role()))?;
        Ok((ctx, output))
    }
}

fn work_order(project: &ProjectConfig, state: &TeamState) -> WorkOrder {
    WorkOrder {
        project: project.clone(),
        task: state.task.clone(),
        iteration: state.iteration,
        specification: state.specification.clone(),
        implementation: state.implementation.clone(),
        review: state.review.clone(),
        workspace: None,
        verification: state.verification.clone(),
    }
}

fn expect_artifact(output: AgentOutput, kind: ArtifactKind, role: Role) -> Result<Artifact> {
    match output {
        AgentOutput::Artifact(artifact) if artifact.kind == kind => Ok(artifact),
        other => bail!(
            "protocol violation: {role} returned {} instead of a {kind:?} artifact",
            other.describe()
        ),
    }
}

fn expect_review(output: AgentOutput) -> Result<ReviewResult> {
    match output {
        AgentOutput::Review(review) => Ok(review),
        other => bail!(
            "protocol violation: Reviewer returned {} instead of a review",
            other.describe()
        ),
    }
}
