pub mod architect;
pub mod builder;
pub mod lead;
pub(crate) mod prompt;
pub mod reviewer;
pub mod specialist;
pub mod tooling;

use anyhow::Result;
use serde::Serialize;
use uuid::Uuid;

use crate::{
    BoxFuture,
    audit::AuditTrail,
    domain::{AgentOutput, Role, WorkOrder},
};

/// Contract between the orchestrator and a department lead.
///
/// A lead can be a single LLM call or delegate to its own subagents; the orchestrator only
/// sees the returned `AgentOutput`. A lead calls its subagents with `ctx.child(..)` so each
/// one gets its own span in the audit trail.
pub trait Agent: Send + Sync {
    fn role(&self) -> Role;
    fn name(&self) -> &str;
    fn model_name(&self) -> &str;
    fn execute<'a>(
        &'a self,
        ctx: &'a AgentContext,
        order: &'a WorkOrder,
    ) -> BoxFuture<'a, Result<AgentOutput>>;
}

/// Run identity plus the audit span of the agent currently executing.
#[derive(Clone)]
pub struct AgentContext {
    run_id: String,
    span_id: String,
    parent_span_id: Option<String>,
    actor: String,
    audit: AuditTrail,
}

impl AgentContext {
    pub fn root(run_id: impl Into<String>, audit: AuditTrail) -> Self {
        Self {
            run_id: run_id.into(),
            span_id: new_span_id(),
            parent_span_id: None,
            actor: "orchestrator".to_string(),
            audit,
        }
    }

    pub fn child(&self, actor: impl Into<String>) -> Self {
        Self {
            run_id: self.run_id.clone(),
            span_id: new_span_id(),
            parent_span_id: Some(self.span_id.clone()),
            actor: actor.into(),
            audit: self.audit.clone(),
        }
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    pub fn span_id(&self) -> &str {
        &self.span_id
    }

    pub fn actor(&self) -> &str {
        &self.actor
    }

    pub fn record<T: Serialize>(&self, event: &str, payload: &T) -> Result<()> {
        self.audit.record(
            &self.run_id,
            &self.span_id,
            self.parent_span_id.as_deref(),
            &self.actor,
            event,
            payload,
        )
    }
}

pub(crate) fn new_span_id() -> String {
    Uuid::new_v4().simple().to_string()[..16].to_string()
}
