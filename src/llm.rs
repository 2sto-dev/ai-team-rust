use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    BoxFuture,
    config::{
        CLAUDE_DEFAULT_API_KEY_ENV, CLAUDE_DEFAULT_BASE_URL, CLAUDE_DEFAULT_MAX_TOKENS,
        ModelConfig, ProviderKind,
    },
    domain::Role,
};

/// Tokens a backend reported for one call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// One model answer: the text, and the usage when the backend reports it.
#[derive(Debug, Clone)]
pub struct Completion {
    pub text: String,
    pub usage: Option<Usage>,
}

/// A chat-completion backend. Agents depend on this trait, so tests and future
/// providers can be swapped in without touching agent code.
pub trait LlmProvider: Send + Sync {
    fn model_name(&self) -> &str;

    fn generate<'a>(
        &'a self,
        system_prompt: &'a str,
        user_prompt: &'a str,
    ) -> BoxFuture<'a, Result<Completion>>;

    /// The answer text only.
    fn complete<'a>(
        &'a self,
        system_prompt: &'a str,
        user_prompt: &'a str,
    ) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move { Ok(self.generate(system_prompt, user_prompt).await?.text) })
    }
}

/// Builds the backend for an employee's `model:` block. The orchestrator takes one, so tests
/// inject offline providers in code; production uses [`http_providers`]. Contracts can only
/// name real providers.
pub type ProviderFactory =
    Arc<dyn Fn(Role, &ModelConfig) -> Result<Arc<dyn LlmProvider>> + Send + Sync>;

pub fn provider_for(config: &ModelConfig) -> Result<Arc<dyn LlmProvider>> {
    Ok(Arc::new(HttpChatProvider::new(config)?))
}

/// The production factory: every role talks to the HTTP provider its contract names.
pub fn http_providers() -> ProviderFactory {
    Arc::new(|_role, config| provider_for(config))
}

// ---------------------------------------------------------------------------
// Metering
// ---------------------------------------------------------------------------

/// One metered model call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageRecord {
    pub role: Role,
    pub model: String,
    /// `None` when the backend reported no token counts.
    pub usage: Option<Usage>,
    /// Only when the contract sets prices.
    pub cost_usd: Option<f64>,
}

/// Running totals of model calls (per task, per planning session, per project).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageTotals {
    pub calls: u64,
    /// Calls whose backend reported no token counts.
    pub calls_without_usage: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
}

impl UsageTotals {
    pub fn add_record(&mut self, record: &UsageRecord) {
        self.calls += 1;
        match record.usage {
            Some(usage) => {
                self.input_tokens += usage.input_tokens;
                self.output_tokens += usage.output_tokens;
            }
            None => self.calls_without_usage += 1,
        }
        self.cost_usd += record.cost_usd.unwrap_or_default();
    }

    pub fn add(&mut self, other: &UsageTotals) {
        self.calls += other.calls;
        self.calls_without_usage += other.calls_without_usage;
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cost_usd += other.cost_usd;
    }
}

impl std::fmt::Display for UsageTotals {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} call(s), {} input + {} output tokens",
            self.calls, self.input_tokens, self.output_tokens
        )?;
        if self.cost_usd > 0.0 {
            write!(f, ", ${:.4}", self.cost_usd)?;
        }
        if self.calls_without_usage > 0 {
            write!(
                f,
                " ({} call(s) without token counts)",
                self.calls_without_usage
            )?;
        }
        Ok(())
    }
}

/// Collects usage records from metered providers until someone takes them.
#[derive(Clone, Default)]
pub struct UsageLedger(Arc<Mutex<Vec<UsageRecord>>>);

impl UsageLedger {
    fn push(&self, record: UsageRecord) {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(record);
    }

    /// Removes and returns everything recorded so far.
    pub fn take(&self) -> Vec<UsageRecord> {
        std::mem::take(
            &mut *self
                .0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
    }

    pub fn take_totals(&self) -> UsageTotals {
        let mut totals = UsageTotals::default();
        for record in self.take() {
            totals.add_record(&record);
        }
        totals
    }
}

/// Wraps every provider built by `factory` so each call lands in `ledger`.
pub fn metered(factory: ProviderFactory, ledger: UsageLedger) -> ProviderFactory {
    Arc::new(move |role, config| {
        Ok(Arc::new(MeteredProvider {
            inner: factory(role, config)?,
            role,
            prices: (config.cost_per_mtok_input, config.cost_per_mtok_output),
            ledger: ledger.clone(),
        }) as Arc<dyn LlmProvider>)
    })
}

struct MeteredProvider {
    inner: Arc<dyn LlmProvider>,
    role: Role,
    prices: (Option<f64>, Option<f64>),
    ledger: UsageLedger,
}

impl LlmProvider for MeteredProvider {
    fn model_name(&self) -> &str {
        self.inner.model_name()
    }

    fn generate<'a>(
        &'a self,
        system_prompt: &'a str,
        user_prompt: &'a str,
    ) -> BoxFuture<'a, Result<Completion>> {
        Box::pin(async move {
            let completion = self.inner.generate(system_prompt, user_prompt).await?;
            let cost_usd = match (completion.usage, self.prices) {
                (Some(usage), (input, output)) if input.is_some() || output.is_some() => Some(
                    usage.input_tokens as f64 / 1e6 * input.unwrap_or_default()
                        + usage.output_tokens as f64 / 1e6 * output.unwrap_or_default(),
                ),
                _ => None,
            };
            self.ledger.push(UsageRecord {
                role: self.role,
                model: self.inner.model_name().to_string(),
                usage: completion.usage,
                cost_usd,
            });
            Ok(completion)
        })
    }
}

// ---------------------------------------------------------------------------
// HTTP chat providers: OpenAI, Ollama, Claude
// ---------------------------------------------------------------------------

const ANTHROPIC_VERSION: &str = "2023-06-01";

#[derive(Debug, Clone, Copy)]
enum Dialect {
    /// `POST {base_url}/chat/completions`, bearer token.
    OpenAi,
    /// `POST {base_url}/api/chat`; honours `num_ctx` / `num_predict`, which the
    /// OpenAI-compatible endpoint of Ollama ignores.
    Ollama,
    /// `POST {base_url}/v1/messages`, `x-api-key` + `anthropic-version` headers.
    Claude,
}

struct AttemptError {
    error: anyhow::Error,
    retryable: bool,
}

impl AttemptError {
    fn fatal(error: anyhow::Error) -> Self {
        Self {
            error,
            retryable: false,
        }
    }
}

pub struct HttpChatProvider {
    http: Client,
    dialect: Dialect,
    endpoint: String,
    model: String,
    api_key_env: Option<String>,
    temperature: f32,
    num_ctx: Option<u32>,
    num_predict: Option<u32>,
    max_tokens: Option<u32>,
    effort: Option<String>,
    max_retries: u32,
    retry_backoff: Duration,
}

impl HttpChatProvider {
    pub fn new(config: &ModelConfig) -> Result<Self> {
        let dialect = match config.provider {
            ProviderKind::Openai => Dialect::OpenAi,
            ProviderKind::Ollama => Dialect::Ollama,
            ProviderKind::Claude => Dialect::Claude,
        };

        let base_url = match (dialect, config.base_url.as_deref()) {
            (_, Some(url)) => url,
            (Dialect::Claude, None) => CLAUDE_DEFAULT_BASE_URL,
            (_, None) => bail!("base_url is required for {:?} provider", config.provider),
        }
        .trim_end_matches('/');

        let api_key_env = match dialect {
            Dialect::Claude => Some(
                config
                    .api_key_env
                    .clone()
                    .unwrap_or_else(|| CLAUDE_DEFAULT_API_KEY_ENV.to_string()),
            ),
            _ => config.api_key_env.clone(),
        };

        let http = Client::builder()
            .timeout(Duration::from_secs(config.timeout_secs))
            .build()
            .context("cannot build HTTP client")?;

        Ok(Self {
            http,
            dialect,
            endpoint: match dialect {
                Dialect::OpenAi => format!("{base_url}/chat/completions"),
                Dialect::Ollama => format!("{base_url}/api/chat"),
                Dialect::Claude => format!("{base_url}/v1/messages"),
            },
            model: config.model.clone(),
            api_key_env,
            temperature: config.temperature,
            num_ctx: config.num_ctx,
            num_predict: config.num_predict,
            max_tokens: config.max_tokens,
            effort: config.effort.clone(),
            max_retries: config.max_retries,
            retry_backoff: Duration::from_millis(config.retry_backoff_ms),
        })
    }

    fn request_body(&self, system_prompt: &str, user_prompt: &str) -> Value {
        match self.dialect {
            Dialect::OpenAi => {
                let mut body = json!({
                    "model": self.model,
                    "messages": [
                        { "role": "system", "content": system_prompt },
                        { "role": "user", "content": user_prompt },
                    ],
                    "temperature": self.temperature,
                });
                if let Some(max_tokens) = self.max_tokens {
                    body["max_tokens"] = json!(max_tokens);
                }
                body
            }
            Dialect::Ollama => {
                let mut options = json!({ "temperature": self.temperature });
                if let Some(num_ctx) = self.num_ctx {
                    options["num_ctx"] = json!(num_ctx);
                }
                if let Some(num_predict) = self.num_predict {
                    options["num_predict"] = json!(num_predict);
                }
                json!({
                    "model": self.model,
                    "messages": [
                        { "role": "system", "content": system_prompt },
                        { "role": "user", "content": user_prompt },
                    ],
                    "stream": false,
                    "options": options,
                })
            }
            Dialect::Claude => {
                // No temperature: current Claude models reject sampling parameters.
                let mut body = json!({
                    "model": self.model,
                    "max_tokens": self.max_tokens.unwrap_or(CLAUDE_DEFAULT_MAX_TOKENS),
                    "system": system_prompt,
                    "messages": [{ "role": "user", "content": user_prompt }],
                });
                if let Some(effort) = &self.effort {
                    body["output_config"] = json!({ "effort": effort });
                }
                body
            }
        }
    }

    async fn send_once(
        &self,
        body: &Value,
        api_key: Option<&str>,
    ) -> Result<Completion, AttemptError> {
        let mut request = self.http.post(&self.endpoint).json(body);
        match (self.dialect, api_key) {
            (Dialect::Claude, Some(api_key)) => {
                request = request
                    .header("x-api-key", api_key)
                    .header("anthropic-version", ANTHROPIC_VERSION);
            }
            (_, Some(api_key)) => request = request.bearer_auth(api_key),
            (_, None) => {}
        }

        let response = request.send().await.map_err(|err| AttemptError {
            retryable: err.is_timeout() || err.is_connect(),
            error: anyhow::Error::new(err).context("LLM HTTP request failed"),
        })?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            let snippet: String = body.chars().take(500).collect();
            return Err(AttemptError {
                retryable: status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error(),
                error: anyhow!("LLM endpoint returned {status}: {snippet}"),
            });
        }

        let parsed: Value = response.json().await.map_err(|err| AttemptError {
            retryable: err.is_timeout(),
            error: anyhow::Error::new(err).context("cannot parse LLM response JSON"),
        })?;

        // A cut-off answer is an error in every dialect: a half-written specification or
        // implementation must not reach review, where a weak model may approve it.
        let content = match self.dialect {
            Dialect::OpenAi => {
                if parsed["choices"][0]["finish_reason"] == "length" {
                    return Err(AttemptError::fatal(anyhow!(
                        "answer was cut off at max_tokens{}; raise max_tokens in the contract",
                        limit_note(self.max_tokens)
                    )));
                }
                parsed["choices"][0]["message"]["content"]
                    .as_str()
                    .map(str::to_string)
            }
            Dialect::Ollama => {
                if parsed["done_reason"] == "length" {
                    return Err(AttemptError::fatal(anyhow!(
                        "answer was cut off at num_predict{}; raise num_predict in the contract",
                        limit_note(self.num_predict)
                    )));
                }
                parsed["message"]["content"].as_str().map(str::to_string)
            }
            Dialect::Claude => claude_text(&parsed)?,
        };

        let text = content
            .map(|content| strip_reasoning(&content))
            .filter(|content| !content.is_empty())
            .ok_or_else(|| {
                AttemptError::fatal(anyhow!("LLM response did not contain message content"))
            })?;
        Ok(Completion {
            text,
            usage: self.usage_of(&parsed),
        })
    }

    /// Token counts as each dialect reports them.
    fn usage_of(&self, parsed: &Value) -> Option<Usage> {
        let (input, output) = match self.dialect {
            Dialect::OpenAi => (
                &parsed["usage"]["prompt_tokens"],
                &parsed["usage"]["completion_tokens"],
            ),
            Dialect::Ollama => (&parsed["prompt_eval_count"], &parsed["eval_count"]),
            Dialect::Claude => (
                &parsed["usage"]["input_tokens"],
                &parsed["usage"]["output_tokens"],
            ),
        };
        Some(Usage {
            input_tokens: input.as_u64()?,
            output_tokens: output.as_u64()?,
        })
    }
}

fn limit_note(limit: Option<u32>) -> String {
    limit
        .map(|tokens| format!(" ({tokens} tokens)"))
        .unwrap_or_default()
}

/// Joins the `text` blocks of a Messages API response. A refusal or a cut-off answer is an
/// error: a partial JSON verdict must never reach the Reviewer or Planner parser.
fn claude_text(parsed: &Value) -> Result<Option<String>, AttemptError> {
    match parsed["stop_reason"].as_str() {
        Some("refusal") => {
            return Err(AttemptError::fatal(anyhow!(
                "Claude declined the request (stop_reason: refusal, category: {})",
                parsed["stop_details"]["category"]
                    .as_str()
                    .unwrap_or("unspecified")
            )));
        }
        Some("max_tokens") => {
            return Err(AttemptError::fatal(anyhow!(
                "Claude answer was cut off at max_tokens; raise max_tokens in the contract"
            )));
        }
        _ => {}
    }

    let text: String = parsed["content"]
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter(|block| block["type"] == "text")
                .filter_map(|block| block["text"].as_str())
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default();
    Ok(Some(text))
}

/// Drops a leading `<think>...</think>` block that reasoning models may emit, so it cannot
/// leak braces into the JSON a Reviewer or Planner must return.
fn strip_reasoning(content: &str) -> String {
    let trimmed = content.trim();
    match trimmed
        .strip_prefix("<think>")
        .and_then(|rest| rest.split_once("</think>"))
    {
        Some((_, answer)) => answer.trim().to_string(),
        None => trimmed.to_string(),
    }
}

impl LlmProvider for HttpChatProvider {
    fn model_name(&self) -> &str {
        &self.model
    }

    fn generate<'a>(
        &'a self,
        system_prompt: &'a str,
        user_prompt: &'a str,
    ) -> BoxFuture<'a, Result<Completion>> {
        Box::pin(async move {
            let api_key = match &self.api_key_env {
                Some(env_name) => Some(std::env::var(env_name).with_context(|| {
                    format!("missing API key environment variable: {env_name}")
                })?),
                None => None,
            };

            let body = self.request_body(system_prompt, user_prompt);

            let mut attempt: u32 = 0;
            loop {
                match self.send_once(&body, api_key.as_deref()).await {
                    Ok(content) => return Ok(content),
                    Err(failure) if failure.retryable && attempt < self.max_retries => {
                        let delay = self.retry_backoff * 2u32.saturating_pow(attempt);
                        attempt += 1;
                        tracing::warn!(
                            model = %self.model,
                            attempt,
                            max_retries = self.max_retries,
                            delay_ms = delay.as_millis() as u64,
                            error = %format!("{:#}", failure.error),
                            "retrying LLM request"
                        );
                        tokio::time::sleep(delay).await;
                    }
                    Err(failure) => {
                        return Err(failure.error.context(format!(
                            "LLM request to {} failed after {} attempt(s)",
                            self.endpoint,
                            attempt + 1
                        )));
                    }
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_leading_think_block() {
        assert_eq!(
            strip_reasoning("<think>{a} hmm</think>\n {\"x\":1}"),
            "{\"x\":1}"
        );
        assert_eq!(strip_reasoning("  plain answer "), "plain answer");
    }

    #[test]
    fn claude_text_joins_text_blocks_and_rejects_refusals() {
        let ok = json!({
            "stop_reason": "end_turn",
            "content": [
                { "type": "thinking", "thinking": "" },
                { "type": "text", "text": "{\"a\":" },
                { "type": "text", "text": "1}" },
            ]
        });
        assert_eq!(
            claude_text(&ok).ok().flatten().as_deref(),
            Some("{\"a\":1}")
        );

        let refusal = json!({ "stop_reason": "refusal", "stop_details": { "category": "cyber" } });
        assert!(claude_text(&refusal).is_err());

        let truncated = json!({ "stop_reason": "max_tokens", "content": [] });
        assert!(claude_text(&truncated).is_err());
    }
}
