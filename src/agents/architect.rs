use std::sync::Arc;

use anyhow::Result;

use crate::{
    BoxFuture,
    domain::{AgentOutput, Artifact, ArtifactKind, Role, WorkOrder},
    llm::LlmProvider,
};

use super::{Agent, AgentContext, prompt::project_context};

pub struct Architect {
    name: String,
    system_prompt: String,
    llm: Arc<dyn LlmProvider>,
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
        }
    }

    async fn specify(&self, order: &WorkOrder) -> Result<Artifact> {
        let context = project_context(&order.project);

        let (prompt, revision) = match (&order.specification, &order.review) {
            (Some(current), Some(review)) => (
                format!(
                    r#"{context}

TASK:
{task}

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
                    "{context}\n\nTASK:\n{}\n\nProduce the technical specification for the Builder.\n",
                    order.task
                ),
                1,
            ),
        };

        let content = self.llm.complete(&self.system_prompt, &prompt).await?;

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
        _ctx: &'a AgentContext,
        order: &'a WorkOrder,
    ) -> BoxFuture<'a, Result<AgentOutput>> {
        Box::pin(async move { self.specify(order).await.map(AgentOutput::Artifact) })
    }
}
