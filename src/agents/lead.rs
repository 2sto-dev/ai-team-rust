//! The Builder as a team lead: it splits an iteration into subtasks for its subagents, the
//! control plane validates the split (known assignees, disjoint file ownership), each
//! subagent works with only its subtask's context, and the platform combines their files.
//! Without a usable split the lead does the work itself.

use std::{collections::HashSet, fmt::Write as _, sync::Arc};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{
    Agent, AgentContext,
    builder::Builder,
    prompt::{parse_json_reply, project_context},
    specialist::Specialist,
    tooling::answer,
};
use crate::{
    BoxFuture,
    domain::{AgentOutput, Artifact, ArtifactKind, Role, WorkOrder},
    llm::LlmProvider,
    mcp::Toolbox,
    workspace::{extract_files, validate_path},
};

/// Heading of the delegation request in the lead's prompt.
pub const DELEGATION_HEADING: &str = "DELEGATION REQUEST";
pub const MAX_SUBTASKS: usize = 4;
const MAX_FILES_PER_SUBTASK: usize = 20;
const DELEGATION_ATTEMPTS: u32 = 2;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Subtask {
    pub assignee: String,
    pub title: String,
    pub instructions: String,
    pub files: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DelegationPlan {
    #[serde(default)]
    subtasks: Vec<Subtask>,
}

/// Control-plane check of a split. Returns the subtasks with normalized paths.
pub fn validate_delegation(subtasks: &[Subtask], team: &[&str]) -> Result<Vec<Subtask>, String> {
    let mut errors = Vec::new();
    if subtasks.len() > MAX_SUBTASKS {
        errors.push(format!(
            "{} subtasks; the maximum is {MAX_SUBTASKS}",
            subtasks.len()
        ));
    }
    let mut owned = HashSet::new();
    let mut normalized = Vec::new();
    for (index, subtask) in subtasks.iter().enumerate() {
        let label = format!("subtask {}", index + 1);
        if !team.contains(&subtask.assignee.as_str()) {
            errors.push(format!(
                "{label}: '{}' is not in your team",
                subtask.assignee
            ));
        }
        if subtask.title.trim().is_empty() || subtask.instructions.trim().is_empty() {
            errors.push(format!("{label}: title and instructions are required"));
        }
        if subtask.files.is_empty() || subtask.files.len() > MAX_FILES_PER_SUBTASK {
            errors.push(format!(
                "{label}: list 1-{MAX_FILES_PER_SUBTASK} files it owns"
            ));
        }
        let mut files = Vec::new();
        for raw in &subtask.files {
            match validate_path(raw) {
                Ok(path) if !owned.insert(path.clone()) => {
                    errors.push(format!(
                        "{label}: '{path}' is already owned by another subtask"
                    ));
                }
                Ok(path) => files.push(path),
                Err(err) => errors.push(format!("{label}: '{raw}': {err}")),
            }
        }
        normalized.push(Subtask {
            files,
            ..subtask.clone()
        });
    }
    if errors.is_empty() {
        Ok(normalized)
    } else {
        Err(errors.join("; "))
    }
}

/// A code fence longer than any backtick run inside `content`.
fn fence_for(content: &str) -> String {
    let longest = content.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    "`".repeat(longest.max(2) + 1)
}

pub struct BuilderLead {
    name: String,
    system_prompt: String,
    llm: Arc<dyn LlmProvider>,
    toolbox: Option<Arc<Toolbox>>,
    solo: Builder,
    team: Vec<Specialist>,
}

impl BuilderLead {
    pub fn new(
        name: impl Into<String>,
        system_prompt: impl Into<String>,
        llm: Arc<dyn LlmProvider>,
        toolbox: Option<Arc<Toolbox>>,
        team: Vec<Specialist>,
    ) -> Self {
        let name = name.into();
        let system_prompt = system_prompt.into();
        let solo = Builder::new(name.clone(), system_prompt.clone(), llm.clone())
            .with_toolbox(toolbox.clone());
        Self {
            name,
            system_prompt,
            llm,
            toolbox,
            solo,
            team,
        }
    }

    fn roster(&self) -> String {
        self.team
            .iter()
            .map(|specialist| format!("- {} | {}", specialist.name(), specialist.profile()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn shared_context(&self, order: &WorkOrder) -> Result<String> {
        let spec = order
            .specification
            .as_ref()
            .context("the lead needs a specification")?;
        let mut text = format!(
            "{}\n\nTASK:\n{}\n\nSPECIFICATION (revision {}):\n{}\n",
            project_context(&order.project),
            order.task,
            spec.revision,
            spec.content
        );
        if let Some(review) = &order.review {
            let _ = write!(
                text,
                "\nREVIEWER FEEDBACK ON THE PREVIOUS ITERATION:\n{}\n",
                review.feedback
            );
        }
        if let Some(verification) = &order.verification {
            let _ = write!(
                text,
                "\nLAST AUTOMATED VERIFICATION (by the platform):\n{}\n",
                verification.report()
            );
        }
        if let Some(workspace) = &order.workspace {
            let _ = write!(text, "\n{workspace}\n");
        }
        Ok(text)
    }

    /// Asks for a split until the control plane accepts one; `None` means the lead works solo.
    async fn plan(&self, ctx: &AgentContext, order: &WorkOrder) -> Result<Option<Vec<Subtask>>> {
        let team: Vec<&str> = self.team.iter().map(Specialist::name).collect();
        let base = format!(
            "{context}\nYOUR TEAM (subagents you may delegate to):\n{roster}\n\n\
             {DELEGATION_HEADING}\n\
             Split this iteration into subtasks for your team, or do the work yourself.\n\
             Rules:\n\
             - At most {MAX_SUBTASKS} subtasks; each names one assignee from YOUR TEAM, a title, \
             precise instructions and the files it owns.\n\
             - Every file belongs to at most one subtask; a subagent may only write the files it owns.\n\
             - Return {{\"subtasks\": []}} to do the work yourself.\n\n\
             Return ONLY valid JSON:\n\
             {{\"subtasks\":[{{\"assignee\":\"EMP-...\",\"title\":\"...\",\"instructions\":\"...\",\"files\":[\"path\"]}}],\"rationale\":\"...\"}}\n",
            context = self.shared_context(order)?,
            roster = self.roster(),
        );

        let mut rejection: Option<String> = None;
        for attempt in 1..=DELEGATION_ATTEMPTS {
            let prompt = match &rejection {
                Some(reason) => format!(
                    "{base}\nYOUR PREVIOUS SPLIT WAS REJECTED BY THE CONTROL PLANE:\n{reason}\n"
                ),
                None => base.clone(),
            };
            let raw = answer(
                self.llm.as_ref(),
                &self.system_prompt,
                &prompt,
                self.toolbox.as_deref(),
                ctx,
            )
            .await?;
            let reason = match parse_json_reply::<DelegationPlan>(&raw) {
                Some(plan) if plan.subtasks.is_empty() => return Ok(None),
                Some(plan) => match validate_delegation(&plan.subtasks, &team) {
                    Ok(subtasks) => return Ok(Some(subtasks)),
                    Err(reason) => reason,
                },
                None => "the reply is not the requested JSON".to_string(),
            };
            ctx.record(
                "DELEGATION_REJECTED",
                &json!({ "attempt": attempt, "reason": reason }),
            )?;
            rejection = Some(reason);
        }
        Ok(None)
    }

    async fn delegate(
        &self,
        ctx: &AgentContext,
        order: &WorkOrder,
        subtasks: &[Subtask],
    ) -> Result<String> {
        let shared = self.shared_context(order)?;
        let mut content = format!(
            "Implementation by {} with {} subtask(s).\n",
            self.name,
            subtasks.len()
        );

        for subtask in subtasks {
            let specialist = self
                .team
                .iter()
                .find(|specialist| specialist.name() == subtask.assignee)
                .context("validated assignee missing")?;
            let sub_ctx = ctx.child(specialist.name());
            sub_ctx.record(
                "SUBTASK_STARTED",
                &json!({ "title": subtask.title, "files": subtask.files }),
            )?;

            // Only this subtask's context: no other subtasks, no full previous implementation.
            let prompt = format!(
                "{shared}\nYOUR SUBTASK (from {lead}): {title}\n{instructions}\n\n\
                 FILES YOU OWN (write or delete only these; anything else is discarded by the platform):\n{files}\n",
                lead = self.name,
                title = subtask.title,
                instructions = subtask.instructions,
                files = subtask
                    .files
                    .iter()
                    .map(|file| format!("- {file}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
            let reply = specialist
                .work(&sub_ctx, &prompt)
                .await
                .with_context(|| format!("subagent {} failed", specialist.name()))?;

            let parsed = extract_files(&reply);
            let owns = |path: &str| {
                validate_path(path).is_ok_and(|normalized| subtask.files.contains(&normalized))
            };
            let kept: Vec<_> = parsed
                .files
                .iter()
                .filter(|file| owns(&file.path))
                .collect();
            let deleted: Vec<_> = parsed.deletions.iter().filter(|path| owns(path)).collect();
            let dropped: Vec<&str> = parsed
                .files
                .iter()
                .map(|file| file.path.as_str())
                .chain(parsed.deletions.iter().map(String::as_str))
                .filter(|path| !owns(path))
                .collect();
            sub_ctx.record(
                "SUBTASK_DONE",
                &json!({
                    "kept": kept.iter().map(|file| &file.path).collect::<Vec<_>>(),
                    "deleted": deleted,
                    "dropped": dropped,
                    "problems": parsed.problems,
                }),
            )?;

            let _ = writeln!(
                content,
                "\n## Subtask: {} ({})",
                subtask.title, subtask.assignee
            );
            for problem in &parsed.problems {
                let _ = writeln!(content, "Platform note: {problem}");
            }
            if !dropped.is_empty() {
                let _ = writeln!(
                    content,
                    "Platform note: discarded files outside the subtask: {}",
                    dropped.join(", ")
                );
            }
            for file in kept {
                let fence = fence_for(&file.content);
                let _ = write!(
                    content,
                    "\n### FILE: {}\n{fence}\n{}{fence}\n",
                    file.path,
                    if file.content.ends_with('\n') {
                        file.content.clone()
                    } else {
                        format!("{}\n", file.content)
                    }
                );
            }
            for path in deleted {
                let _ = writeln!(content, "\n### DELETE: {path}");
            }
        }
        Ok(content)
    }
}

impl Agent for BuilderLead {
    fn role(&self) -> Role {
        Role::Builder
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn model_name(&self) -> &str {
        self.llm.model_name()
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a AgentContext,
        order: &'a WorkOrder,
    ) -> BoxFuture<'a, Result<AgentOutput>> {
        Box::pin(async move {
            // Ownership of files only makes sense when answers land in a workspace.
            if order.workspace.is_none() || self.team.is_empty() {
                return self.solo.execute(ctx, order).await;
            }
            let Some(subtasks) = self.plan(ctx, order).await? else {
                ctx.record("DELEGATION_SKIPPED", &json!({ "lead": self.name }))?;
                return self.solo.execute(ctx, order).await;
            };
            ctx.record("DELEGATION_PLANNED", &subtasks)?;
            let content = self.delegate(ctx, order, &subtasks).await?;
            Ok(AgentOutput::Artifact(Artifact {
                kind: ArtifactKind::Implementation,
                author: self.name.clone(),
                revision: order.iteration,
                content,
            }))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subtask(assignee: &str, files: &[&str]) -> Subtask {
        Subtask {
            assignee: assignee.to_string(),
            title: "t".to_string(),
            instructions: "do it".to_string(),
            files: files.iter().map(|f| f.to_string()).collect(),
        }
    }

    #[test]
    fn validates_assignees_and_disjoint_files() {
        let team = ["EMP-PY-001", "EMP-QA-001"];
        assert!(
            validate_delegation(
                &[
                    subtask("EMP-PY-001", &["a.py"]),
                    subtask("EMP-QA-001", &["tests\\test_a.py"])
                ],
                &team
            )
            .is_ok()
        );

        let err = validate_delegation(
            &[
                subtask("EMP-PY-001", &["a.py"]),
                subtask("EMP-QA-001", &["a.py"]),
                subtask("EMP-X-001", &["../x"]),
            ],
            &team,
        )
        .unwrap_err();
        assert!(err.contains("already owned"), "{err}");
        assert!(err.contains("not in your team"), "{err}");
        assert!(err.contains("'..'"), "{err}");
    }

    #[test]
    fn fence_outgrows_inner_backticks() {
        assert_eq!(fence_for("plain"), "```");
        assert_eq!(fence_for("has ``` inside"), "````");
    }
}
