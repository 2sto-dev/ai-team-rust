use std::{fmt::Write as _, sync::Arc};

use anyhow::Result;
use serde_json::json;

use crate::{
    BoxFuture,
    domain::{AgentOutput, Artifact, ArtifactKind, Role, WorkOrder},
    llm::LlmProvider,
    mcp::Toolbox,
};

use super::{
    Agent, AgentContext, prompt::project_context, specialist::Specialist, tooling::answer,
};

/// A consultant's answer when the task is outside its domain.
pub const NOT_RELEVANT: &str = "NOT RELEVANT";

pub struct Architect {
    name: String,
    system_prompt: String,
    llm: Arc<dyn LlmProvider>,
    toolbox: Option<Arc<Toolbox>>,
    /// Design consultants (the Architect's subagents) asked for notes before each
    /// specification. They advise; the Architect decides.
    consultants: Vec<Specialist>,
}

impl Architect {
    pub fn new(
        name: impl Into<String>,
        system_prompt: impl Into<String>,
        llm: Arc<dyn LlmProvider>,
    ) -> Self {
        Self {
            name: name.into(),
            system_prompt: system_prompt.into(),
            llm,
            toolbox: None,
            consultants: Vec::new(),
        }
    }

    pub fn with_consultants(mut self, consultants: Vec<Specialist>) -> Self {
        self.consultants = consultants;
        self
    }

    /// Notes from every consultant whose domain the task touches. A consultant that fails or
    /// answers `NOT RELEVANT` is left out (and audited); consultation never fails the task.
    async fn consult(
        &self,
        ctx: &AgentContext,
        context: &str,
        task: &str,
        existing: &str,
    ) -> Result<String> {
        let mut notes = String::new();
        for consultant in &self.consultants {
            let sub_ctx = ctx.child(consultant.name());
            sub_ctx.record("CONSULTATION_STARTED", &json!({ "lead": self.name }))?;
            let prompt = format!(
                "{context}\n\nTASK:\n{task}{existing}\n\nThe Architect ({lead}) will write the \
                 specification for this task. Give your design notes for your domain only. If \
                 the task does not touch your domain, answer exactly {NOT_RELEVANT}.\n",
                lead = self.name
            );
            let outcome = match consultant.work(&sub_ctx, &prompt).await {
                Ok(reply)
                    if reply
                        .trim()
                        .trim_matches('.')
                        .eq_ignore_ascii_case(NOT_RELEVANT) =>
                {
                    "not relevant".to_string()
                }
                Ok(reply) => {
                    let _ = write!(notes, "\n### {}\n{}\n", consultant.name(), reply.trim());
                    "notes".to_string()
                }
                Err(err) => format!("failed: {err:#}"),
            };
            sub_ctx.record("CONSULTATION_DONE", &json!({ "outcome": outcome }))?;
        }
        Ok(notes)
    }

    /// MCP tools the model may call while working.
    pub fn with_toolbox(mut self, toolbox: Option<Arc<Toolbox>>) -> Self {
        self.toolbox = toolbox;
        self
    }

    async fn specify(&self, ctx: &AgentContext, order: &WorkOrder) -> Result<Artifact> {
        let context = project_context(&order.project);
        let existing = order
            .workspace
            .as_deref()
            .map(|files| {
                format!(
                    "\n\nEXISTING PROJECT FILES (work the Owner already approved):\n{files}\n\n\
                     Build on these files. Keep existing behaviour, functions and tests unless \
                     the task explicitly asks to change them."
                )
            })
            .unwrap_or_default();
        let advice = if self.consultants.is_empty() {
            String::new()
        } else {
            let notes = self.consult(ctx, &context, &order.task, &existing).await?;
            if notes.is_empty() {
                String::new()
            } else {
                format!(
                    "\n\nNOTES FROM YOUR DESIGN CONSULTANTS (advice; you decide what goes into the \
                     specification):{notes}"
                )
            }
        };

        let (prompt, revision) = match (&order.specification, &order.review) {
            (Some(current), Some(review)) => (
                format!(
                    r#"{context}

TASK:
{task}{existing}{advice}

CURRENT SPECIFICATION (revision {revision}):
{spec}

LATEST IMPLEMENTATION ATTEMPT:
{implementation}

REVIEWER FEEDBACK ON THE SPECIFICATION:
{feedback}

The Reviewer found that the specification itself must change. Return the complete revised
specification for the Builder. Address every reviewer item and do not drop requirements
that were not criticised.
"#,
                    task = order.task,
                    revision = current.revision,
                    spec = current.content,
                    implementation = order
                        .implementation
                        .as_ref()
                        .map_or("none", |artifact| artifact.content.as_str()),
                    feedback = review.feedback,
                ),
                current.revision + 1,
            ),
            _ => (
                format!(
                    "{context}\n\nTASK:\n{}{existing}{advice}\n\nProduce the technical specification for the Builder.\n",
                    order.task
                ),
                1,
            ),
        };

        let content = answer(
            self.llm.as_ref(),
            &self.system_prompt,
            &prompt,
            self.toolbox.as_deref(),
            ctx,
        )
        .await?;

        Ok(Artifact {
            kind: ArtifactKind::Specification,
            author: self.name.clone(),
            revision,
            content,
        })
    }
}

impl Agent for Architect {
    fn role(&self) -> Role {
        Role::Architect
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
        Box::pin(async move { self.specify(ctx, order).await.map(AgentOutput::Artifact) })
    }
}
