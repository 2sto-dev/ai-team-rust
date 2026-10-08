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
    /// Every prompt token, cached or not (budgets and KPIs count the whole prompt).
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Part of `input_tokens` read from the provider's prompt cache (billed at a fraction).
    #[serde(default)]
    pub cache_read_tokens: u64,
    /// Part of `input_tokens` written to the prompt cache (billed slightly above input).
    #[serde(default)]
    pub cache_write_tokens: u64,
    /// Exact cost the backend reported (Claude Code), in millionths of a dollar; 0 = none,
    /// then the contract's prices apply.
    #[serde(default)]
    pub reported_cost_micros: u64,
}

/// Anthropic prompt-cache prices relative to input: reads are 0.1x or less (0.05x on Claude
/// Opus 5.5), writes with the 5-minute TTL 1.25x. The read factor is the conservative 0.1.
const CACHE_READ_PRICE: f64 = 0.1;
const CACHE_WRITE_PRICE: f64 = 1.25;

/// One model answer: the text, and the usage when the backend reports it.
#[derive(Debug, Clone)]
pub struct Completion {
    pub text: String,
    pub usage: Option<Usage>,
}

/// A tool the model may call (from an MCP server), described with a JSON Schema.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

/// A tool call requested by the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

/// One message of a multi-turn exchange (the system prompt travels separately).
#[derive(Debug, Clone, PartialEq)]
pub enum ChatMessage {
    User(String),
    Assistant {
        text: String,
        tool_calls: Vec<ToolCall>,
    },
    Tool {
        call_id: String,
        name: String,
        content: String,
    },
}

/// One model turn: text and/or tool calls.
#[derive(Debug, Clone)]
pub struct ChatTurn {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Option<Usage>,
}

/// A chat-completion backend. Agents depend on this trait, so tests and future
/// providers can be swapped in without touching agent code.
pub trait LlmProvider: Send + Sync {
    fn model_name(&self) -> &str;

    /// Multi-turn exchange with optional tools. Providers without tool support only handle
    /// a single user message and no tools.
    fn chat<'a>(
        &'a self,
        system_prompt: &'a str,
        messages: &'a [ChatMessage],
        tools: &'a [ToolSpec],
    ) -> BoxFuture<'a, Result<ChatTurn>> {
        Box::pin(async move {
            match (messages, tools.is_empty()) {
                ([ChatMessage::User(prompt)], true) => {
                    let completion = self.generate(system_prompt, prompt).await?;
                    Ok(ChatTurn {
                        text: completion.text,
                        tool_calls: Vec::new(),
                        usage: completion.usage,
                    })
                }
                _ => bail!("{} does not support tool calling", self.model_name()),
            }
        })
    }

    fn generate<'a>(
        &'a self,
        system_prompt: &'a str,
        user_prompt: &'a str,
    ) -> BoxFuture<'a, Result<Completion>>;

    /// A lead's planning decision. Agentic backends must not edit files or append a diff.
    fn generate_plan<'a>(
        &'a self,
        system_prompt: &'a str,
        user_prompt: &'a str,
    ) -> BoxFuture<'a, Result<Completion>> {
        self.generate(system_prompt, user_prompt)
    }

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
/// `ANTHROPIC_WORKSPACE_ID` (from `.env`), for API keys that are not scoped to a workspace.
pub fn anthropic_workspace_id() -> Option<String> {
    std::env::var("ANTHROPIC_WORKSPACE_ID")
        .ok()
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty())
}

/// The backend for a contract's model block; `claude_code` depends on the role (a Builder
/// edits, everyone else only reads and runs).
pub fn provider_for_role(role: Role, config: &ModelConfig) -> Result<Arc<dyn LlmProvider>> {
    match config.provider {
        crate::config::ProviderKind::ClaudeCode | crate::config::ProviderKind::Codex => {
            crate::claude_code::provider(config, role)
        }
        _ => provider_for(config),
    }
}

pub fn http_providers() -> ProviderFactory {
    Arc::new(provider_for_role)
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

    fn generate_plan<'a>(
        &'a self,
        system_prompt: &'a str,
        user_prompt: &'a str,
    ) -> BoxFuture<'a, Result<Completion>> {
        Box::pin(async move {
            let completion = self.inner.generate_plan(system_prompt, user_prompt).await?;
            self.record(completion.usage);
            Ok(completion)
        })
    }

    fn chat<'a>(
        &'a self,
        system_prompt: &'a str,
        messages: &'a [ChatMessage],
        tools: &'a [ToolSpec],
    ) -> BoxFuture<'a, Result<ChatTurn>> {
        Box::pin(async move {
            let turn = self.inner.chat(system_prompt, messages, tools).await?;
            self.record(turn.usage);
            Ok(turn)
        })
    }

    fn generate<'a>(
        &'a self,
        system_prompt: &'a str,
        user_prompt: &'a str,
    ) -> BoxFuture<'a, Result<Completion>> {
        Box::pin(async move {
            let completion = self.inner.generate(system_prompt, user_prompt).await?;
            self.record(completion.usage);
            Ok(completion)
        })
    }
}

impl MeteredProvider {
    fn record(&self, usage: Option<Usage>) {
        {
            let completion = Completion {
                text: String::new(),
                usage,
            };
            let cost_usd = match (completion.usage, self.prices) {
                (Some(usage), _) if usage.reported_cost_micros > 0 => {
                    Some(usage.reported_cost_micros as f64 / 1e6)
                }
                (Some(usage), (input, output)) if input.is_some() || output.is_some() => {
                    let uncached = usage
                        .input_tokens
                        .saturating_sub(usage.cache_read_tokens + usage.cache_write_tokens);
                    let input_units = uncached as f64
                        + usage.cache_read_tokens as f64 * CACHE_READ_PRICE
                        + usage.cache_write_tokens as f64 * CACHE_WRITE_PRICE;
                    Some(
                        input_units / 1e6 * input.unwrap_or_default()
                            + usage.output_tokens as f64 / 1e6 * output.unwrap_or_default(),
                    )
                }
                _ => None,
            };
            self.ledger.push(UsageRecord {
                role: self.role,
                model: self.inner.model_name().to_string(),
                usage: completion.usage,
                cost_usd,
            });
        }
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
            ProviderKind::ClaudeCode | ProviderKind::Codex => {
                bail!(
                    "{:?} is not an HTTP backend; use provider_for_role",
                    config.provider
                )
            }
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

    fn request_body(
        &self,
        system_prompt: &str,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
    ) -> Value {
        match self.dialect {
            Dialect::OpenAi => {
                let mut wire = vec![json!({ "role": "system", "content": system_prompt })];
                for message in messages {
                    wire.push(match message {
                        ChatMessage::User(text) => json!({ "role": "user", "content": text }),
                        ChatMessage::Assistant { text, tool_calls } if tool_calls.is_empty() => {
                            json!({ "role": "assistant", "content": text })
                        }
                        ChatMessage::Assistant { text, tool_calls } => json!({
                            "role": "assistant",
                            "content": text,
                            "tool_calls": tool_calls.iter().map(|call| json!({
                                "id": call.id,
                                "type": "function",
                                "function": { "name": call.name, "arguments": call.arguments.to_string() },
                            })).collect::<Vec<_>>(),
                        }),
                        ChatMessage::Tool { call_id, content, .. } => {
                            json!({ "role": "tool", "tool_call_id": call_id, "content": content })
                        }
                    });
                }
                let mut body = json!({
                    "model": self.model,
                    "messages": wire,
                    "temperature": self.temperature,
                });
                if !tools.is_empty() {
                    body["tools"] = json!(
                        tools
                            .iter()
                            .map(|tool| json!({
                                "type": "function",
                                "function": {
                                    "name": tool.name,
                                    "description": tool.description,
                                    "parameters": tool.parameters,
                                },
                            }))
                            .collect::<Vec<_>>()
                    );
                }
                if let Some(max_tokens) = self.max_tokens {
                    body["max_tokens"] = json!(max_tokens);
                }
                body
            }
            Dialect::Ollama => {
                let mut wire = vec![json!({ "role": "system", "content": system_prompt })];
                for message in messages {
                    wire.push(match message {
                        ChatMessage::User(text) => json!({ "role": "user", "content": text }),
                        ChatMessage::Assistant { text, tool_calls } if tool_calls.is_empty() => {
                            json!({ "role": "assistant", "content": text })
                        }
                        ChatMessage::Assistant { text, tool_calls } => json!({
                            "role": "assistant",
                            "content": text,
                            "tool_calls": tool_calls.iter().map(|call| json!({
                                "function": { "name": call.name, "arguments": call.arguments },
                            })).collect::<Vec<_>>(),
                        }),
                        ChatMessage::Tool { name, content, .. } => {
                            json!({ "role": "tool", "tool_name": name, "content": content })
                        }
                    });
                }
                let mut options = json!({ "temperature": self.temperature });
                if let Some(num_ctx) = self.num_ctx {
                    options["num_ctx"] = json!(num_ctx);
                }
                if let Some(num_predict) = self.num_predict {
                    options["num_predict"] = json!(num_predict);
                }
                let mut body = json!({
                    "model": self.model,
                    "messages": wire,
                    "stream": false,
                    "options": options,
                });
                if !tools.is_empty() {
                    body["tools"] = json!(
                        tools
                            .iter()
                            .map(|tool| json!({
                                "type": "function",
                                "function": {
                                    "name": tool.name,
                                    "description": tool.description,
                                    "parameters": tool.parameters,
                                },
                            }))
                            .collect::<Vec<_>>()
                    );
                }
                body
            }
            Dialect::Claude => {
                // Consecutive tool results travel together in one user message.
                let mut wire: Vec<Value> = Vec::new();
                for message in messages {
                    match message {
                        ChatMessage::User(text) => {
                            wire.push(json!({ "role": "user", "content": text }));
                        }
                        ChatMessage::Assistant { text, tool_calls } => {
                            let mut blocks = Vec::new();
                            if !text.is_empty() {
                                blocks.push(json!({ "type": "text", "text": text }));
                            }
                            for call in tool_calls {
                                blocks.push(json!({
                                    "type": "tool_use",
                                    "id": call.id,
                                    "name": call.name,
                                    "input": call.arguments,
                                }));
                            }
                            wire.push(json!({ "role": "assistant", "content": blocks }));
                        }
                        ChatMessage::Tool {
                            call_id, content, ..
                        } => {
                            let block = json!({
                                "type": "tool_result",
                                "tool_use_id": call_id,
                                "content": content,
                            });
                            match wire.last_mut() {
                                Some(last)
                                    if last["role"] == "user"
                                        && last["content"].as_array().is_some_and(|blocks| {
                                            blocks.iter().all(|b| b["type"] == "tool_result")
                                        }) =>
                                {
                                    last["content"]
                                        .as_array_mut()
                                        .expect("checked above")
                                        .push(block);
                                }
                                _ => wire.push(json!({ "role": "user", "content": [block] })),
                            }
                        }
                    }
                }
                // No temperature: current Claude models reject sampling parameters.
                let mut body = json!({
                    "model": self.model,
                    "max_tokens": self.max_tokens.unwrap_or(CLAUDE_DEFAULT_MAX_TOKENS),
                    // Prompt caching: the system prompt (and the tools rendered before it) is the
                    // stable prefix of every call of an employee; the top-level marker caches the
                    // growing conversation of a tool loop. Below the model's minimum nothing is
                    // cached and nothing is charged extra.
                    "system": [{
                        "type": "text",
                        "text": system_prompt,
                        "cache_control": { "type": "ephemeral" },
                    }],
                    "cache_control": { "type": "ephemeral" },
                    "messages": wire,
                });
                if !tools.is_empty() {
                    body["tools"] = json!(
                        tools
                            .iter()
                            .map(|tool| json!({
                                "name": tool.name,
                                "description": tool.description,
                                "input_schema": tool.parameters,
                            }))
                            .collect::<Vec<_>>()
                    );
                }
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
    ) -> Result<ChatTurn, AttemptError> {
        let mut request = self.http.post(&self.endpoint).json(body);
        match (self.dialect, api_key) {
            (Dialect::Claude, Some(api_key)) => {
                request = request
                    .header("x-api-key", api_key)
                    .header("anthropic-version", ANTHROPIC_VERSION);
                // Keys not scoped to a workspace must name one.
                if let Some(workspace) = anthropic_workspace_id() {
                    request = request.header("anthropic-workspace-id", workspace);
                }
            }
            (_, Some(api_key)) => request = request.bearer_auth(api_key),
            (_, None) => {}
        }

        // A timed-out generation is not retried: the server keeps working on it, and the same
        // long request would time out again (a real run spent 3 x 180 s this way; on a paid API
        // every attempt is billed). Connection failures are retried.
        let response = request.send().await.map_err(|err| AttemptError {
            retryable: err.is_connect() && !err.is_timeout(),
            error: if err.is_timeout() {
                anyhow::Error::new(err).context(
                    "LLM HTTP request failed: no answer within timeout_secs; not retried (it \
                     would repeat the same generation). Raise timeout_secs, or check that the \
                     model fits the GPU (ollama ps)",
                )
            } else {
                anyhow::Error::new(err).context("LLM HTTP request failed")
            },
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
            retryable: false,
            error: anyhow::Error::new(err).context("cannot parse LLM response JSON"),
        })?;

        // A cut-off answer is an error in every dialect: a half-written specification or
        // implementation must not reach review, where a weak model may approve it.
        let (content, tool_calls) = match self.dialect {
            Dialect::OpenAi => {
                if parsed["choices"][0]["finish_reason"] == "length" {
                    return Err(AttemptError::fatal(anyhow!(
                        "answer was cut off at max_tokens{}; raise max_tokens in the contract",
                        limit_note(self.max_tokens)
                    )));
                }
                let message = &parsed["choices"][0]["message"];
                let calls: Vec<ToolCall> = message["tool_calls"]
                    .as_array()
                    .map(|calls| {
                        calls
                            .iter()
                            .enumerate()
                            .map(|(index, call)| ToolCall {
                                id: call["id"]
                                    .as_str()
                                    .map_or_else(|| format!("call_{index}"), str::to_string),
                                name: call["function"]["name"].as_str().unwrap_or("").to_string(),
                                arguments: call["function"]["arguments"]
                                    .as_str()
                                    .and_then(|raw| serde_json::from_str(raw).ok())
                                    .unwrap_or_else(|| json!({})),
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                (message["content"].as_str().map(str::to_string), calls)
            }
            Dialect::Ollama => {
                if parsed["done_reason"] == "length" {
                    return Err(AttemptError::fatal(anyhow!(
                        "answer was cut off at num_predict{}; raise num_predict in the contract",
                        limit_note(self.num_predict)
                    )));
                }
                // A prompt that filled the context was truncated by Ollama (it keeps the end).
                if let (Some(num_ctx), Some(prompt)) =
                    (self.num_ctx, parsed["prompt_eval_count"].as_u64())
                {
                    let room =
                        u64::from(num_ctx).saturating_sub(u64::from(self.num_predict.unwrap_or(0)));
                    if prompt >= room {
                        return Err(AttemptError::fatal(anyhow!(
                            "prompt filled the context ({prompt} tokens, num_ctx {num_ctx}); Ollama \
                             dropped its beginning. Raise num_ctx in the contract or send less input"
                        )));
                    }
                }
                let message = &parsed["message"];
                let calls: Vec<ToolCall> = message["tool_calls"]
                    .as_array()
                    .map(|calls| {
                        calls
                            .iter()
                            .enumerate()
                            .map(|(index, call)| ToolCall {
                                id: call["id"]
                                    .as_str()
                                    .map_or_else(|| format!("call_{index}"), str::to_string),
                                name: call["function"]["name"].as_str().unwrap_or("").to_string(),
                                arguments: match &call["function"]["arguments"] {
                                    Value::String(raw) => {
                                        serde_json::from_str(raw).unwrap_or_else(|_| json!({}))
                                    }
                                    Value::Null => json!({}),
                                    other => other.clone(),
                                },
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                (message["content"].as_str().map(str::to_string), calls)
            }
            Dialect::Claude => {
                let text = claude_text(&parsed)?;
                let calls: Vec<ToolCall> = parsed["content"]
                    .as_array()
                    .map(|blocks| {
                        blocks
                            .iter()
                            .filter(|block| block["type"] == "tool_use")
                            .map(|block| ToolCall {
                                id: block["id"].as_str().unwrap_or("").to_string(),
                                name: block["name"].as_str().unwrap_or("").to_string(),
                                arguments: block["input"].clone(),
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                (text, calls)
            }
        };

        let text = content
            .map(|content| strip_reasoning(&content))
            .unwrap_or_default();
        if text.is_empty() && tool_calls.is_empty() {
            return Err(AttemptError::fatal(anyhow!(
                "LLM response did not contain message content"
            )));
        }
        Ok(ChatTurn {
            text,
            tool_calls,
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
        // Claude reports cached prompt tokens apart from `input_tokens`.
        let (read, write) = match self.dialect {
            Dialect::Claude => (
                parsed["usage"]["cache_read_input_tokens"]
                    .as_u64()
                    .unwrap_or(0),
                parsed["usage"]["cache_creation_input_tokens"]
                    .as_u64()
                    .unwrap_or(0),
            ),
            _ => (0, 0),
        };
        Some(Usage {
            input_tokens: input.as_u64()? + read + write,
            output_tokens: output.as_u64()?,
            cache_read_tokens: read,
            cache_write_tokens: write,
            reported_cost_micros: 0,
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

/// Tokens a prompt certainly needs: about 4 characters per token for English and code (fewer
/// for Romanian), so this underestimates and only flags prompts that cannot fit.
const CHARS_PER_TOKEN_UPPER: usize = 4;

impl HttpChatProvider {
    /// Ollama keeps only the end of a prompt longer than `num_ctx`, silently dropping the
    /// system prompt and instructions. Refuse such prompts instead of sending them.
    fn check_prompt_fits(&self, system_prompt: &str, messages: &[ChatMessage]) -> Result<()> {
        let Some(num_ctx) = self.num_ctx else {
            return Ok(());
        };
        let chars = system_prompt.chars().count()
            + messages
                .iter()
                .map(|message| match message {
                    ChatMessage::User(text) => text.chars().count(),
                    ChatMessage::Assistant { text, .. } => text.chars().count(),
                    ChatMessage::Tool { content, .. } => content.chars().count(),
                })
                .sum::<usize>();
        let room = (num_ctx as usize).saturating_sub(self.num_predict.unwrap_or(0) as usize);
        let tokens = chars / CHARS_PER_TOKEN_UPPER;
        anyhow::ensure!(
            tokens <= room,
            "prompt too large for the model's context: at least {tokens} tokens, but num_ctx {num_ctx} \
             leaves {room} after num_predict; raise num_ctx in the contract or send less input"
        );
        Ok(())
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
            let messages = [ChatMessage::User(user_prompt.to_string())];
            let turn = self.chat(system_prompt, &messages, &[]).await?;
            Ok(Completion {
                text: turn.text,
                usage: turn.usage,
            })
        })
    }

    fn chat<'a>(
        &'a self,
        system_prompt: &'a str,
        messages: &'a [ChatMessage],
        tools: &'a [ToolSpec],
    ) -> BoxFuture<'a, Result<ChatTurn>> {
        Box::pin(async move {
            self.check_prompt_fits(system_prompt, messages)?;
            let api_key = match &self.api_key_env {
                // An empty `KEY=` line in .env is a missing key, not an empty one.
                Some(env_name) => Some(
                    std::env::var(env_name)
                        .ok()
                        .filter(|key| !key.trim().is_empty())
                        .with_context(|| {
                            format!("missing API key: set {env_name} in .env (see .env.example)")
                        })?,
                ),
                None => None,
            };

            let body = self.request_body(system_prompt, messages, tools);

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
