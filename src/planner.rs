//! The Planner is the AI half of the orchestrator: it proposes, the control plane decides.
//! In this phase it proposes the team (one lead per department) for a run.

use std::sync::Arc;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::{
    agents::prompt::{parse_json_reply, project_context},
    domain::{ProjectConfig, TeamAssignment},
    llm::LlmProvider,
    project::plan::{MAX_PLAN_TASKS, TaskPlan},
    registry::{EmployeeFunction, Registry},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Candidate {
    pub employee_id: String,
    pub name: String,
    pub title: String,
    pub function: EmployeeFunction,
    pub skills: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub team: TeamAssignment,
    #[serde(default)]
    pub rationale: String,
    /// A specialist the task needs and nobody on staff covers. Only a suggestion: the control
    /// plane turns it into a hiring proposal the Owner completes and approves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hire: Option<HireSuggestion>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HireSuggestion {
    pub title: String,
    /// The lead the specialist would report to.
    pub manager: String,
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub reason: String,
}

/// The specialists already on staff (subagents), one line each, for the Planner's view of
/// the company. Deliberately not in the `- EMP-... | function |` candidate format: they are
/// not choosable as leads.
pub fn staff(registry: &Registry) -> Vec<String> {
    let mut lines: Vec<String> = registry
        .employees()
        .filter(|employee| {
            employee.is_active() && employee.contract.function == EmployeeFunction::Specialist
        })
        .map(|employee| {
            format!(
                "* {} (under {}): {} - skills: {}",
                employee.id(),
                employee.contract.manager_id,
                employee.contract.title,
                employee.contract.skills.join(", ")
            )
        })
        .collect();
    lines.sort();
    lines
}

/// Active department leads the Planner may choose from, ordered by `employee_id`.
pub fn candidates(registry: &Registry) -> Vec<Candidate> {
    [
        EmployeeFunction::Architect,
        EmployeeFunction::Builder,
        EmployeeFunction::Reviewer,
    ]
    .into_iter()
    .flat_map(|function| registry.active(function))
    .map(|employee| Candidate {
        employee_id: employee.id().to_string(),
        name: employee.contract.name.clone(),
        title: employee.contract.title.clone(),
        function: employee.contract.function,
        skills: employee.contract.skills.clone(),
    })
    .collect()
}

pub struct Planner {
    employee_id: String,
    system_prompt: String,
    llm: Arc<dyn LlmProvider>,
}

impl Planner {
    pub fn new(
        employee_id: impl Into<String>,
        system_prompt: impl Into<String>,
        llm: Arc<dyn LlmProvider>,
    ) -> Self {
        Self {
            employee_id: employee_id.into(),
            system_prompt: system_prompt.into(),
            llm,
        }
    }

    pub fn employee_id(&self) -> &str {
        &self.employee_id
    }

    pub fn model_name(&self) -> &str {
        self.llm.model_name()
    }

    /// Asks for a team. `rejection` carries the control plane's reason when a previous
    /// proposal was refused, so the Planner can correct it.
    ///
    /// Outer `Err`: the model could not be reached (fatal). Inner `Err`: the reply is not a
    /// usable plan; the caller may ask again, but never executes it.
    pub async fn propose(
        &self,
        project: &ProjectConfig,
        task: &str,
        candidates: &[Candidate],
        staff: &[String],
        rejection: Option<&str>,
    ) -> Result<Result<Plan, String>> {
        let roster = candidates
            .iter()
            .map(|candidate| {
                format!(
                    "- {} | {} | {} | skills: {}",
                    candidate.employee_id,
                    candidate.function,
                    candidate.title,
                    candidate.skills.join(", ")
                )
            })
            .collect::<Vec<_>>()
            .join("\n");

        let mut prompt = format!(
            r#"{context}

TASK:
{task}

CANDIDATES (employee_id | function | title | skills):
{roster}

SPECIALISTS ALREADY ON STAFF (they work under their lead; not choosable above):
{staff}

Choose exactly one architect, one builder and one reviewer from the candidates above, matching
their skills to the task. Copy each employee_id exactly as written in the list.

Return ONLY valid JSON:
{{"team":{{"architect":"EMP-...","builder":"EMP-...","reviewer":"EMP-..."}},"rationale":"why this team"}}

Only if the task clearly needs expertise that no lead and no specialist above has, add a hiring
suggestion to the same JSON object (the Owner decides; the run goes on with the chosen team):
"hire":{{"title":"Elixir Developer","manager":"EMP-...","skills":["elixir","otp"],"reason":"why"}}
The manager is the lead the new specialist would work under. Leave "hire" out otherwise.
"#,
            context = project_context(project),
            staff = if staff.is_empty() {
                "- none".to_string()
            } else {
                staff.join("\n")
            },
        );
        if let Some(rejection) = rejection {
            prompt.push_str(&format!(
                "\nYOUR PREVIOUS PROPOSAL WAS REJECTED BY THE CONTROL PLANE:\n{rejection}\n\
                 Propose again, using only employee_id values copied exactly from the list.\n"
            ));
        }

        let raw = self.llm.complete(&self.system_prompt, &prompt).await?;
        Ok(match parse_json_reply(&raw) {
            Some(plan) => Ok(plan),
            None => Err(format!(
                "planner returned invalid JSON. Raw output (truncated): {}",
                raw.chars().take(500).collect::<String>()
            )),
        })
    }
}

/// Heading that starts the task-plan request in the Planner prompt.
pub const TASK_PLAN_HEADING: &str = "TASK PLAN REQUEST";

impl Planner {
    /// Asks for a breakdown of the project into milestones and tasks. Same contract as
    /// [`Planner::propose`]: outer `Err` is fatal, inner `Err` is an unusable reply.
    pub async fn propose_tasks(
        &self,
        project: &ProjectConfig,
        team: &[Candidate],
        owner_note: Option<&str>,
        rejection: Option<&str>,
    ) -> Result<Result<TaskPlan, String>> {
        let roster = team
            .iter()
            .map(|candidate| {
                format!(
                    "- {} | {} | skills: {}",
                    candidate.employee_id,
                    candidate.function,
                    candidate.skills.join(", ")
                )
            })
            .collect::<Vec<_>>()
            .join("\n");

        let mut prompt = format!(
            r#"{context}

{TASK_PLAN_HEADING}
Break the project objective into ordered milestones and tasks. Each task is specified by the
Architect, implemented by the Builder and approved by the Reviewer, so keep every task small
enough for one specification and one implementation.

Rules:
- 1 to {max} tasks in total, grouped in ordered milestones.
- Every task has a unique short key (T1, T2, ...), a title, a description and at least one
  testable acceptance criterion.
- depends_on lists the keys of tasks that must be finished first: only tasks of the same or an
  earlier milestone, and no cycles.

TEAM (for sizing the work):
{roster}
"#,
            context = project_context(project),
            max = MAX_PLAN_TASKS,
        );
        if let Some(note) = owner_note {
            prompt.push_str(&format!("\nOWNER NOTE FOR THIS PLAN:\n{note}\n"));
        }
        if let Some(rejection) = rejection {
            prompt.push_str(&format!(
                "\nYOUR PREVIOUS PLAN WAS REJECTED BY THE CONTROL PLANE:\n{rejection}\nFix every point.\n"
            ));
        }
        prompt.push_str(
            r#"
Return ONLY valid JSON:
{"milestones":[{"name":"...","tasks":[{"key":"T1","title":"...","description":"...","acceptance_criteria":["..."],"depends_on":[]}]}],"rationale":"..."}
"#,
        );

        let raw = self.llm.complete(&self.system_prompt, &prompt).await?;
        Ok(match parse_json_reply(&raw) {
            Some(plan) => Ok(plan),
            None => Err(format!(
                "planner returned an invalid task plan. Raw output (truncated): {}",
                raw.chars().take(500).collect::<String>()
            )),
        })
    }
}
