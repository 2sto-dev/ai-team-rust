use std::sync::Arc;

use anyhow::{Context, Result, bail};

use crate::{
    BoxFuture,
    domain::{AgentOutput, ReviewResult, Role, WorkOrder},
    llm::LlmProvider,
};

use super::{
    Agent, AgentContext,
    prompt::{parse_json_reply, project_context},
};

pub struct Reviewer {
    name: String,
    system_prompt: String,
    llm: Arc<dyn LlmProvider>,
}

impl Reviewer {
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

    async fn review(&self, order: &WorkOrder) -> Result<ReviewResult> {
        let spec = order
            .specification
            .as_ref()
            .context("reviewer needs a specification")?;
        let implementation = order
            .implementation
            .as_ref()
            .context("reviewer needs an implementation")?;

        let mut prompt = format!(
            r#"{context}

TASK:
{task}

SPECIFICATION (revision {spec_revision}):
{spec}

IMPLEMENTATION (iteration {iteration}):
{implementation}

YOUR PREVIOUS FEEDBACK:
{previous}

Return ONLY valid JSON:
{{"decision":"APPROVED"|"CHANGES_REQUIRED","target":"builder"|"architect","feedback":"specific actionable feedback"}}

Use "target":"architect" only when the specification itself is wrong or incomplete, so the
Builder cannot meet the acceptance criteria by following it. Otherwise use "builder".
"#,
            context = project_context(&order.project),
            task = order.task,
            spec_revision = spec.revision,
            spec = spec.content,
            iteration = order.iteration,
            implementation = implementation.content,
            previous = order
                .review
                .as_ref()
                .map_or("none (first review)", |review| review.feedback.as_str()),
        );

        if let Some(verification) = &order.verification {
            prompt.push_str(&format!(
                "\nAUTOMATED VERIFICATION (performed by the platform, not by the Builder; trust it \
                 over any claim in the implementation):\n{}\n",
                verification.report()
            ));
        }

        // A malformed reply is asked again, never interpreted: only a well-formed verdict counts.
        let mut raw = self.llm.complete(&self.system_prompt, &prompt).await?;
        for _ in 0..FORMAT_RETRIES {
            if parse_json_reply::<ReviewResult>(&raw).is_some() {
                break;
            }
            let retry = format!(
                "{prompt}\nYOUR PREVIOUS REPLY WAS REJECTED: it was not the required JSON object.\n\
                 Previous reply (truncated):\n{}\n\nReply again with ONLY the JSON object.\n",
                raw.chars().take(500).collect::<String>()
            );
            raw = self.llm.complete(&self.system_prompt, &retry).await?;
        }
        parse_review(&raw)
    }
}

/// Extra attempts when the model's reply is not a well-formed verdict.
const FORMAT_RETRIES: usize = 2;

impl Agent for Reviewer {
    fn role(&self) -> Role {
        Role::Reviewer
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
        Box::pin(async move { self.review(order).await.map(AgentOutput::Review) })
    }
}

/// Fails closed: anything that is not a well-formed decision is an error, never an approval.
fn parse_review(raw: &str) -> Result<ReviewResult> {
    match parse_json_reply(raw) {
        Some(review) => Ok(review),
        None => bail!("reviewer returned invalid JSON; approval is denied. Raw output: {raw}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ReviewDecision, ReviewTarget};

    #[test]
    fn parses_plain_json() {
        let review = parse_review(r#"{"decision":"APPROVED","feedback":"ok"}"#).unwrap();
        assert_eq!(review.decision, ReviewDecision::Approved);
        assert_eq!(review.target, ReviewTarget::Builder);
    }

    #[test]
    fn parses_json_inside_prose_and_fence() {
        let raw = "Here is my verdict:\n```json\n{\"decision\":\"CHANGES_REQUIRED\",\"target\":\"architect\",\"feedback\":\"spec misses retries\"}\n```";
        let review = parse_review(raw).unwrap();
        assert_eq!(review.decision, ReviewDecision::ChangesRequired);
        assert_eq!(review.target, ReviewTarget::Architect);
    }

    #[test]
    fn rejects_free_text_approval() {
        assert!(parse_review("APPROVED, looks great").is_err());
    }

    #[test]
    fn rejects_unknown_decision() {
        assert!(parse_review(r#"{"decision":"APPROVE","feedback":"ok"}"#).is_err());
    }

    #[test]
    fn rejects_missing_feedback() {
        assert!(parse_review(r#"{"decision":"APPROVED"}"#).is_err());
    }
}
