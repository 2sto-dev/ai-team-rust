use std::{fs, path::Path};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::domain::ProjectConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// OpenAI chat-completions API (`{base_url}/chat/completions`), also any compatible server.
    #[serde(alias = "openai_compatible")]
    Openai,
    /// Native Ollama API (`{base_url}/api/chat`); `base_url` without `/v1`.
    Ollama,
    /// Anthropic Messages API (`{base_url}/v1/messages`).
    Claude,
}

pub const CLAUDE_DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
pub const CLAUDE_DEFAULT_API_KEY_ENV: &str = "ANTHROPIC_API_KEY";
pub const CLAUDE_DEFAULT_MAX_TOKENS: u32 = 16_000;
const CLAUDE_EFFORT_LEVELS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// The AI model an employee is authorised to use (the `model:` block of a contract).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelConfig {
    pub provider: ProviderKind,
    pub model: String,
    /// Required for `openai` and `ollama`; `claude` defaults to the Anthropic API.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Name of the environment variable holding the API key; `claude` defaults to
    /// `ANTHROPIC_API_KEY`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    /// Sent to `openai` and `ollama`. Not sent to `claude`: current Claude models reject
    /// sampling parameters; use `effort` there.
    #[serde(default = "default_temperature")]
    pub temperature: f32,
    /// `ollama` only: context window in tokens (the server default is much smaller).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub num_ctx: Option<u32>,
    /// `ollama` only: maximum tokens generated per answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub num_predict: Option<u32>,
    /// `openai` and `claude`: maximum tokens generated per answer (`claude` defaults to 16000).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// `claude` only: low | medium | high | xhigh | max.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Optional prices in USD per million tokens, for cost metering.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_per_mtok_input: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_per_mtok_output: Option<f64>,
    /// Whole-request timeout for one LLM call.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// Extra attempts on timeouts, connection errors, 429 and 5xx.
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    /// First retry delay; doubles on every following attempt.
    #[serde(default = "default_retry_backoff_ms")]
    pub retry_backoff_ms: u64,
}

fn default_temperature() -> f32 {
    0.1
}

fn default_timeout_secs() -> u64 {
    120
}

fn default_max_retries() -> u32 {
    2
}

fn default_retry_backoff_ms() -> u64 {
    1000
}

impl ModelConfig {
    /// A model on `provider` with every optional field at its default.
    pub fn new(provider: ProviderKind, model: impl Into<String>) -> Self {
        Self {
            provider,
            model: model.into(),
            base_url: None,
            api_key_env: None,
            temperature: default_temperature(),
            num_ctx: None,
            num_predict: None,
            max_tokens: None,
            effort: None,
            cost_per_mtok_input: None,
            cost_per_mtok_output: None,
            timeout_secs: default_timeout_secs(),
            max_retries: default_max_retries(),
            retry_backoff_ms: default_retry_backoff_ms(),
        }
    }

    pub fn validate(&self) -> Result<()> {
        use ProviderKind as P;

        anyhow::ensure!(!self.model.trim().is_empty(), "model name cannot be empty");
        anyhow::ensure!(
            self.timeout_secs > 0,
            "timeout_secs must be greater than zero"
        );

        // Options a provider would silently ignore are configuration mistakes.
        if self.provider != P::Ollama {
            anyhow::ensure!(
                self.num_ctx.is_none() && self.num_predict.is_none(),
                "num_ctx and num_predict are only supported by the ollama provider"
            );
        }
        if !matches!(self.provider, P::Openai | P::Claude) {
            anyhow::ensure!(
                self.max_tokens.is_none(),
                "max_tokens is only supported by the openai and claude providers \
                 (ollama uses num_predict)"
            );
        }
        match (&self.effort, self.provider) {
            (None, _) => {}
            (Some(effort), P::Claude) => anyhow::ensure!(
                CLAUDE_EFFORT_LEVELS.contains(&effort.as_str()),
                "effort must be one of {}",
                CLAUDE_EFFORT_LEVELS.join(", ")
            ),
            (Some(_), _) => anyhow::bail!("effort is only supported by the claude provider"),
        }

        if matches!(self.provider, P::Openai | P::Ollama) {
            anyhow::ensure!(
                self.base_url
                    .as_deref()
                    .is_some_and(|url| !url.trim().is_empty()),
                "base_url is required for the {:?} provider",
                self.provider
            );
        }
        Ok(())
    }

    /// Same backend and model, i.e. no model-level independence between two employees.
    pub fn same_model_as(&self, other: &ModelConfig) -> bool {
        self.provider == other.provider
            && self.model == other.model
            && self.base_url == other.base_url
    }
}

pub fn load_project(path: impl AsRef<Path>) -> Result<ProjectConfig> {
    let path = path.as_ref();
    let raw = fs::read_to_string(path)
        .with_context(|| format!("cannot read project config: {}", path.display()))?;
    parse_project(&raw).with_context(|| format!("invalid project config: {}", path.display()))
}

/// A project definition as JSON (a `projects/*.json` file, or pasted in the web interface).
pub fn parse_project(raw: &str) -> Result<ProjectConfig> {
    let project: ProjectConfig = serde_json::from_str(raw).context("invalid project JSON")?;

    anyhow::ensure!(
        project.max_iterations > 0,
        "project max_iterations must be greater than zero"
    );
    anyhow::ensure!(
        !project.project_id.trim().is_empty(),
        "project_id cannot be empty"
    );
    anyhow::ensure!(
        !project.objective.trim().is_empty(),
        "project objective cannot be empty"
    );

    Ok(project)
}
