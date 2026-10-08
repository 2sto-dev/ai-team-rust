use std::sync::Arc;

use anyhow::{Context, Result, bail};
use serde_json::json;

use crate::{
    BoxFuture,
    domain::{AgentOutput, ReviewDecision, ReviewResult, ReviewTarget, Role, WorkOrder},
    llm::LlmProvider,
    mcp::Toolbox,
};

use super::{
    Agent, AgentContext,
    prompt::{parse_json_reply, project_context},
    specialist::Specialist,
    tooling::answer,
};

/// A reviewer's subagent. Its verdict is advisory, unless it holds a veto.
pub struct Advisor {
    pub specialist: Specialist,
    pub veto: bool,
}

struct AdvisorVerdict {
    name: String,
    veto: bool,
    review: ReviewResult,
}

pub struct Reviewer {
    name: String,
    system_prompt: String,
    llm: Arc<dyn LlmProvider>,
    toolbox: Option<Arc<Toolbox>>,
    advisors: Vec<Advisor>,
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
            toolbox: None,
            advisors: Vec::new(),
        }
    }

    pub fn with_toolbox(mut self, toolbox: Option<Arc<Toolbox>>) -> Self {
        self.toolbox = toolbox;
        self
    }

    pub fn with_advisors(mut self, advisors: Vec<Advisor>) -> Self {
        self.advisors = advisors;
        self
    }

    /// Advisory reviews from the reviewer's subagents. A veto holder that fails or answers
    /// badly counts as a rejection: a veto never fails open.
    async fn consult(&self, ctx: &AgentContext, prompt: &str) -> Result<Vec<AdvisorVerdict>> {
        let mut verdicts = Vec::new();
        for advisor in &self.advisors {
            let name = advisor.specialist.name().to_string();
            let sub_ctx = ctx.child(&name);
            let review = match advisor.specialist.work(&sub_ctx, prompt).await {
                Ok(raw) => parse_json_reply::<ReviewResult>(&raw),
                Err(err) => {
                    sub_ctx.record("ADVISOR_FAILED", &json!({ "error": format!("{err:#}") }))?;
                    None
                }
            };
            let review = match (review, advisor.veto) {
                (Some(review), _) => review,
                (None, true) => ReviewResult {
                    decision: ReviewDecision::ChangesRequired,
                    target: ReviewTarget::Builder,
                    feedback: format!("{name} gave no valid verdict; a veto holder fails closed"),
                },
                (None, false) => continue,
            };
            sub_ctx.record(
                "ADVISOR_REVIEW",
                &json!({ "veto": advisor.veto, "review": review }),
            )?;
            verdicts.push(AdvisorVerdict {
                name,
                veto: advisor.veto,
                review,
            });
        }
        Ok(verdicts)
    }

    async fn review(&self, ctx: &AgentContext, order: &WorkOrder) -> Result<ReviewResult> {
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

        // Advisors judge the code for their specialty: they need the task and the code, not
        // the project context and specification the Reviewer weighs (about half the tokens).
        let mut advisor_prompt = format!(
            "TASK:
{task}

IMPLEMENTATION (iteration {iteration}):
{implementation}
",
            task = order.task,
            iteration = order.iteration,
            implementation = implementation.content,
        );
        if let Some(verification) = &order.verification {
            advisor_prompt.push_str(&format!(
                "
AUTOMATED VERIFICATION (by the platform):
{}
",
                verification.report()
            ));
        }
        advisor_prompt.push_str(
            "
Review the implementation for your specialty only. Return ONLY valid JSON:
             {\"decision\":\"APPROVED\"|\"CHANGES_REQUIRED\",\"target\":\"builder\",\"feedback\":\"specific actionable feedback\"}
",
        );
        let advisory = self.consult(ctx, &advisor_prompt).await?;
        if !advisory.is_empty() {
            prompt.push_str(
                "\nADVISORY REVIEWS (from your specialists; a VETO rejection cannot be overruled):\n",
            );
            for verdict in &advisory {
                prompt.push_str(&format!(
                    "- {}{}: {:?} - {}\n",
                    verdict.name,
                    if verdict.veto { " [VETO]" } else { "" },
                    verdict.review.decision,
                    verdict.review.feedback
                ));
            }
        }

        // A malformed reply is asked again, never interpreted: only a well-formed verdict counts.
        let mut raw = answer(
            self.llm.as_ref(),
            &self.system_prompt,
            &prompt,
            self.toolbox.as_deref(),
            ctx,
        )
        .await?;
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
        let review = parse_review(&raw)?;

        // The veto is enforced by the platform, whatever the Reviewer decided.
        if let Some(veto) = advisory.iter().find(|verdict| {
            verdict.veto && verdict.review.decision == ReviewDecision::ChangesRequired
        }) {
            ctx.record(
                "VETO_APPLIED",
                &json!({ "by": veto.name, "feedback": veto.review.feedback }),
            )?;
            return Ok(ReviewResult {
                decision: ReviewDecision::ChangesRequired,
                target: ReviewTarget::Builder,
                feedback: format!(
                    "Veto by {}: {}\n\nReviewer: {}",
                    veto.name, veto.review.feedback, review.feedback
                ),
            });
        }
        Ok(review)
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
        ctx: &'a AgentContext,
        order: &'a WorkOrder,
    ) -> BoxFuture<'a, Result<AgentOutput>> {
        Box::pin(async move { self.review(ctx, order).await.map(AgentOutput::Review) })
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
