use std::sync::Arc;

use anyhow::{Context, Result};

use crate::{
    BoxFuture,
    domain::{AgentOutput, Artifact, ArtifactKind, Role, WorkOrder},
    llm::LlmProvider,
    mcp::Toolbox,
};

use super::{Agent, AgentContext, prompt::project_context, tooling::answer};

pub struct Builder {
    name: String,
    system_prompt: String,
    llm: Arc<dyn LlmProvider>,
    toolbox: Option<Arc<Toolbox>>,
}

impl Builder {
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
        }
    }

    /// MCP tools the model may call while working.
    pub fn with_toolbox(mut self, toolbox: Option<Arc<Toolbox>>) -> Self {
        self.toolbox = toolbox;
        self
    }

    async fn build(&self, ctx: &AgentContext, order: &WorkOrder) -> Result<Artifact> {
        let spec = order
            .specification
            .as_ref()
            .context("builder needs a specification")?;

        let instruction = if order.implementation.is_some() {
            "Revise the PREVIOUS IMPLEMENTATION: keep what the reviewer did not criticise, fix every \
             reviewer item explicitly, and adapt to the specification if its revision changed. \
             Return the complete updated implementation, not only the changes."
        } else {
            "Produce the implementation proposal for this specification."
        };

        let mut prompt = format!(
            r#"{context}

TASK:
{task}

SPECIFICATION (revision {spec_revision}):
{spec}

ITERATION:
{iteration}

PREVIOUS IMPLEMENTATION:
{previous}

REVIEWER FEEDBACK ON THE PREVIOUS IMPLEMENTATION:
{feedback}

{instruction}
"#,
            context = project_context(&order.project),
            task = order.task,
            spec_revision = spec.revision,
            spec = spec.content,
            iteration = order.iteration,
            // Its files are already in the WORKSPACE section: sending them twice only costs.
            previous = match (&order.implementation, &order.workspace) {
                (None, _) => "none (first iteration)",
                (Some(_), Some(_)) =>
                    "written to the workspace: its files are in the WORKSPACE section below",
                (Some(artifact), None) => artifact.content.as_str(),
            },
            feedback = order
                .review
                .as_ref()
                .map_or("none", |review| review.feedback.as_str()),
        );

        if let Some(verification) = &order.verification {
            prompt.push_str(&format!(
                "\nLAST AUTOMATED VERIFICATION (by the platform, for the previous implementation):\n{}\n",
                verification.report()
            ));
        }
        if let Some(workspace) = &order.workspace {
            prompt.push_str(&format!("\n{workspace}\n"));
        }

        let content = answer(
            self.llm.as_ref(),
            &self.system_prompt,
            &prompt,
            self.toolbox.as_deref(),
            ctx,
        )
        .await?;

        Ok(Artifact {
            kind: ArtifactKind::Implementation,
            author: self.name.clone(),
            revision: order.iteration,
            content,
        })
    }
}

impl Agent for Builder {
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
        Box::pin(async move { self.build(ctx, order).await.map(AgentOutput::Artifact) })
    }
}
