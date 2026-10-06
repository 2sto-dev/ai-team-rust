//! A subagent: a specialist with its own model, skill packs and MCP tools, working on a
//! subtask a department lead gives it.

use std::sync::Arc;

use anyhow::Result;

use super::{AgentContext, tooling::answer};
use crate::{llm::LlmProvider, mcp::Toolbox};

pub struct Specialist {
    name: String,
    system_prompt: String,
    llm: Arc<dyn LlmProvider>,
    toolbox: Option<Arc<Toolbox>>,
    /// Shown to the lead when it delegates: skills, skill packs and tools.
    profile: String,
}

impl Specialist {
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
            profile: String::new(),
        }
    }

    pub fn with_toolbox(mut self, toolbox: Option<Arc<Toolbox>>) -> Self {
        self.toolbox = toolbox;
        self
    }

    pub fn with_profile(mut self, profile: impl Into<String>) -> Self {
        self.profile = profile.into();
        self
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn profile(&self) -> &str {
        &self.profile
    }

    pub async fn work(&self, ctx: &AgentContext, prompt: &str) -> Result<String> {
        answer(
            self.llm.as_ref(),
            &self.system_prompt,
            prompt,
            self.toolbox.as_deref(),
            ctx,
        )
        .await
    }
}
