//! Claude Code as an employee's backend (`provider: claude_code`): the CLI runs headless in a
//! disposable git clone of the task's workspace.
//!
//! - `Edit` (Builder, specialists): Claude Code edits the clone, runs commands and fixes its
//!   own failures; the platform then turns the clone's changes into `### FILE` / `### DELETE`
//!   blocks, so path rules, the Owner's test command, review and merge apply exactly as for any
//!   other Builder. Nothing reaches the real workspace except through those gates.
//! - `ReadOnly` (Reviewer, advisors, others): it may read files and run commands (tests, the
//!   program itself) in the clone but not edit; the clone is thrown away.
//!
//! The run is bounded by `timeout_secs` and `max_budget_usd`; its reported tokens and cost go
//! to the usage ledger like any other call.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use serde_json::Value;
use tokio::{io::AsyncWriteExt, process::Command};

use crate::{
    BoxFuture,
    config::ModelConfig,
    domain::Role,
    llm::{ChatMessage, ChatTurn, Completion, LlmProvider, ToolSpec, Usage},
    workspace::filter_snapshot,
};

tokio::task_local! {
    /// The task workspace an agent works on; set by the orchestrator around a run.
    pub static WORKSPACE_ROOT: PathBuf;
}

/// What Claude Code may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Edit,
    ReadOnly,
    Plan,
}

impl Mode {
    pub fn for_role(role: Role) -> Self {
        match role {
            Role::Builder | Role::Specialist => Mode::Edit,
            _ => Mode::ReadOnly,
        }
    }
}

const EDIT_TOOLS: [&str; 7] = ["Read", "Edit", "Write", "MultiEdit", "Glob", "Grep", "Bash"];
const READ_TOOLS: [&str; 4] = ["Read", "Glob", "Grep", "Bash"];
const WRITE_TOOLS: [&str; 4] = ["Edit", "Write", "MultiEdit", "NotebookEdit"];

/// Which agentic CLI runs the work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentCli {
    /// `claude -p` (Claude Code; its login, e.g. a Claude subscription, or an API key).
    ClaudeCode,
    /// `codex exec` (OpenAI Codex; its login, e.g. a ChatGPT subscription).
    Codex,
}

impl AgentCli {
    fn name(self) -> &'static str {
        match self {
            AgentCli::ClaudeCode => "Claude Code",
            AgentCli::Codex => "Codex",
        }
    }
}

pub struct ClaudeCodeProvider {
    kind: AgentCli,
    model: String,
    cli: Vec<String>,
    timeout: Duration,
    max_budget_usd: Option<f64>,
    api_key_env: Option<String>,
    mode: Mode,
}

impl ClaudeCodeProvider {
    pub fn new(config: &ModelConfig, mode: Mode) -> Result<Self> {
        let cli = config
            .cli
            .clone()
            .filter(|cli| !cli.is_empty())
            .unwrap_or_else(|| vec![default_cli(config.provider).to_string()]);
        let kind = match config.provider {
            crate::config::ProviderKind::Codex => AgentCli::Codex,
            _ => AgentCli::ClaudeCode,
        };
        Ok(Self {
            kind,
            model: config.model.clone(),
            cli,
            timeout: Duration::from_secs(config.timeout_secs),
            max_budget_usd: config.max_budget_usd,
            api_key_env: config.api_key_env.clone(),
            mode,
        })
    }

    async fn run(&self, system_prompt: &str, prompt: &str, mode: Mode) -> Result<ChatTurn> {
        let scratch = tempfile::tempdir().context("cannot create a folder for Claude Code")?;
        let work = scratch.path().join("work");
        let root = WORKSPACE_ROOT.try_with(Clone::clone).ok();
        match &root {
            Some(root) if root.join(".git").exists() => clone(root, &work).await?,
            _ => std::fs::create_dir_all(&work)?,
        }

        // The platform's snapshot is for models that cannot read files; Claude Code reads
        // what it needs itself, so only the file listing stays.
        let prompt = filter_snapshot(prompt, &[]);
        let instructions = match mode {
            Mode::Plan => {
                "\n\nHOW TO PLAN:\nRead files only. Do not edit files, execute commands, or implement the work. Return exactly the requested JSON plan, without a diff or summary."
            }
            Mode::Edit => {
                "\n\nHOW TO WORK (this overrides any instruction about writing `### FILE` blocks):\n\
                 - The current folder is a copy of the project. Edit, create and delete files \
                 in place; the platform collects your changes.\n\
                 - Run the project's tests and fix failures before you finish.\n\
                 - Do not commit, push or change git settings.\n\
                 - Finish with a short summary of what you changed and why."
            }
            Mode::ReadOnly => {
                "\n\nHOW TO WORK:\n\
                 - The current folder is a disposable copy of the project. Read files and run \
                 commands (the tests, the program itself) to check how it really behaves.\n\
                 - Do not modify files.\n\
                 - Your final message must be exactly the answer format requested above."
            }
        };

        let input = format!("{prompt}{instructions}");
        let (result, usage) = match self.kind {
            AgentCli::ClaudeCode => self.run_claude(&work, system_prompt, &input, mode).await?,
            AgentCli::Codex => {
                self.run_codex(&work, scratch.path(), system_prompt, &input, mode)
                    .await?
            }
        };
        let text = match mode {
            Mode::Edit => format!("{result}\n{}", collect_changes(&work).await?),
            Mode::ReadOnly | Mode::Plan => result,
        };
        Ok(ChatTurn {
            text,
            tool_calls: Vec::new(),
            usage,
        })
    }

    async fn run_claude(
        &self,
        work: &Path,
        system_prompt: &str,
        input: &str,
        mode: Mode,
    ) -> Result<(String, Option<Usage>)> {
        let mut command = Command::new(&self.cli[0]);
        command
            .args(&self.cli[1..])
            .args(["-p", "--output-format", "json", "--no-session-persistence"])
            .args(["--model", &self.model])
            .args(["--append-system-prompt", system_prompt]);
        match mode {
            Mode::Plan => {
                command.args([
                    "--permission-mode",
                    "dontAsk",
                    "--allowedTools",
                    "Read",
                    "Glob",
                    "Grep",
                    "--disallowedTools",
                ]);
                command.args(WRITE_TOOLS).arg("Bash");
            }
            Mode::Edit => {
                command.args(["--permission-mode", "acceptEdits", "--allowedTools"]);
                command.args(EDIT_TOOLS);
            }
            Mode::ReadOnly => {
                command.args(["--permission-mode", "dontAsk", "--allowedTools"]);
                command.args(READ_TOOLS);
                command.arg("--disallowedTools");
                command.args(WRITE_TOOLS);
            }
        }
        if let Some(budget) = self.max_budget_usd {
            command.args(["--max-budget-usd", &budget.to_string()]);
        }
        // Auth: with `api_key_env`, the API key (billed to API credit). Without it, the CLI's own
        // login (`claude` -> /login, e.g. a Claude subscription): the keys the platform loaded
        // from .env are removed, or the CLI would prefer them.
        match &self.api_key_env {
            Some(variable) => {
                let key = std::env::var(variable)
                    .ok()
                    .filter(|key| !key.trim().is_empty())
                    .with_context(|| format!("missing API key: set {variable} in .env"))?;
                command.env("ANTHROPIC_API_KEY", key);
                if let Some(workspace) = crate::llm::anthropic_workspace_id() {
                    command.env(
                        "ANTHROPIC_CUSTOM_HEADERS",
                        format!("anthropic-workspace-id: {workspace}"),
                    );
                }
            }
            None => {
                for variable in [
                    "ANTHROPIC_API_KEY",
                    "ANTHROPIC_AUTH_TOKEN",
                    "ANTHROPIC_CUSTOM_HEADERS",
                ] {
                    command.env_remove(variable);
                }
            }
        }
        let mut child = command
            .current_dir(work)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("cannot start Claude Code (`{}`)", self.cli.join(" ")))?;
        // The prompt goes through stdin: it can be longer than a command line allows.
        let mut stdin = child.stdin.take().context("no stdin for Claude Code")?;
        stdin.write_all(input.as_bytes()).await?;
        drop(stdin);

        let pid = child.id();
        let mut tree = crate::workspace::KillTreeOnDrop::new(pid);
        let output = match tokio::time::timeout(self.timeout, child.wait_with_output()).await {
            Ok(output) => {
                tree.disarm();
                output?
            }
            Err(_) => {
                if let Some(pid) = pid {
                    crate::workspace::kill_process_tree(pid).await;
                }
                tree.disarm();
                bail!(
                    "Claude Code gave no answer within timeout_secs {}; not retried",
                    self.timeout.as_secs()
                );
            }
        };
        let stdout = String::from_utf8_lossy(&output.stdout);
        let parsed: Value = serde_json::from_str(stdout.trim()).with_context(|| {
            format!(
                "Claude Code did not return JSON (exit {:?}): {}",
                output.status.code(),
                String::from_utf8_lossy(&output.stderr)
                    .chars()
                    .take(400)
                    .collect::<String>()
            )
        })?;
        let result = parsed["result"].as_str().unwrap_or("").to_string();
        if parsed["is_error"] == true {
            bail!(
                "Claude Code failed: {}",
                if result.is_empty() {
                    "no details"
                } else {
                    &result
                }
            );
        }

        let mut usage = usage_of(&parsed);
        // On a subscription (no API key) the reported cost is only what the API would have
        // charged: nothing is billed, so it must not count against budgets or statistics.
        if self.api_key_env.is_none()
            && let Some(usage) = usage.as_mut()
        {
            usage.reported_cost_micros = 0;
        }
        Ok((result, usage))
    }

    /// `codex exec` in the clone: sandboxed to the folder (`workspace-write`) or `read-only`,
    /// events as JSONL, the final message written to a file.
    async fn run_codex(
        &self,
        work: &Path,
        scratch: &Path,
        system_prompt: &str,
        input: &str,
        mode: Mode,
    ) -> Result<(String, Option<Usage>)> {
        let last = scratch.join("last-message.txt");
        let mut command = Command::new(&self.cli[0]);
        command
            .args(&self.cli[1..])
            .args([
                "exec",
                "--json",
                "--ephemeral",
                "--skip-git-repo-check",
                "--color",
                "never",
            ])
            .args([
                "--sandbox",
                match mode {
                    Mode::Edit => "workspace-write",
                    Mode::ReadOnly | Mode::Plan => "read-only",
                },
            ])
            .arg("--cd")
            .arg(work)
            .arg("--output-last-message")
            .arg(&last);
        if !self.model.is_empty() && self.model != "default" {
            command.args(["--model", &self.model]);
        }
        // Codex has no separate system prompt: the job description leads the prompt.
        command.arg("-");
        let output = self
            .spawn_and_wait(command, work, &format!("{system_prompt}\n\n{input}"))
            .await?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut usage = Usage::default();
        let mut failure = None;
        for line in stdout.lines() {
            let Ok(event) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            match event["type"].as_str() {
                Some("turn.completed") => {
                    let u = &event["usage"];
                    let n = |key: &str| u[key].as_u64().unwrap_or(0);
                    usage.input_tokens += n("input_tokens");
                    usage.cache_read_tokens += n("cached_input_tokens");
                    usage.output_tokens += n("output_tokens") + n("reasoning_output_tokens");
                }
                Some("turn.failed") | Some("error") => {
                    failure = Some(
                        event["error"]["message"]
                            .as_str()
                            .or(event["message"].as_str())
                            .unwrap_or("unknown error")
                            .to_string(),
                    );
                }
                _ => {}
            }
        }
        if let Some(message) = failure {
            bail!("Codex failed: {message}");
        }
        anyhow::ensure!(
            output.status.success(),
            "Codex exited with {:?}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
                .chars()
                .take(400)
                .collect::<String>()
        );
        let result = std::fs::read_to_string(&last).unwrap_or_default();
        Ok((result.trim().to_string(), Some(usage)))
    }

    /// Starts the CLI in `work`, sends `input` on stdin and waits, within the timeout (the
    /// whole process tree is killed when it expires).
    async fn spawn_and_wait(
        &self,
        mut command: Command,
        work: &Path,
        input: &str,
    ) -> Result<std::process::Output> {
        let mut child = command
            .current_dir(work)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| {
                format!(
                    "cannot start {} (`{}`)",
                    self.kind.name(),
                    self.cli.join(" ")
                )
            })?;
        let mut stdin = child.stdin.take().context("no stdin")?;
        stdin.write_all(input.as_bytes()).await?;
        drop(stdin);
        let pid = child.id();
        let mut tree = crate::workspace::KillTreeOnDrop::new(pid);
        match tokio::time::timeout(self.timeout, child.wait_with_output()).await {
            Ok(output) => {
                tree.disarm();
                Ok(output?)
            }
            Err(_) => {
                if let Some(pid) = pid {
                    crate::workspace::kill_process_tree(pid).await;
                }
                tree.disarm();
                bail!(
                    "{} gave no answer within timeout_secs {}; not retried",
                    self.kind.name(),
                    self.timeout.as_secs()
                );
            }
        }
    }
}

/// Tokens and the exact cost Claude Code reports (cached prompt tokens included).
fn usage_of(parsed: &Value) -> Option<Usage> {
    let usage = &parsed["usage"];
    let number = |key: &str| usage[key].as_u64().unwrap_or(0);
    let read = number("cache_read_input_tokens");
    let write = number("cache_creation_input_tokens");
    let cost = parsed["total_cost_usd"].as_f64().unwrap_or(0.0);
    Some(Usage {
        input_tokens: number("input_tokens") + read + write,
        output_tokens: number("output_tokens"),
        cache_read_tokens: read,
        cache_write_tokens: write,
        reported_cost_micros: (cost * 1e6).round() as u64,
    })
}

async fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .await
        .context("cannot run git")?;
    anyhow::ensure!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// A throwaway clone of the workspace at its current branch.
async fn clone(root: &Path, into: &Path) -> Result<()> {
    let output = Command::new("git")
        .args(["clone", "--quiet", "--no-hardlinks"])
        .arg(root)
        .arg(into)
        .output()
        .await
        .context("cannot run git clone")?;
    anyhow::ensure!(
        output.status.success(),
        "cannot copy the workspace for Claude Code: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

/// The clone's changes as the Builder answer format the platform applies.
async fn collect_changes(work: &Path) -> Result<String> {
    if !work.join(".git").exists() {
        git(work, &["init", "--quiet"]).await?;
    }
    git(work, &["add", "-A"]).await?;
    let has_head = git(work, &["rev-parse", "--verify", "HEAD"]).await.is_ok();
    let listing = if has_head {
        git(
            work,
            &["diff", "--cached", "--name-status", "--no-renames", "HEAD"],
        )
        .await?
    } else {
        git(
            work,
            &[
                "diff",
                "--cached",
                "--name-status",
                "--no-renames",
                "--root",
            ],
        )
        .await
        .or_else(|_| Ok::<_, anyhow::Error>(String::new()))?
    };
    let mut answer = String::new();
    for line in listing.lines() {
        let Some((status, path)) = line.split_once('\t') else {
            continue;
        };
        let path = path.trim();
        if status.starts_with('D') {
            answer.push_str(&format!("\n### DELETE: {path}\n"));
            continue;
        }
        match std::fs::read(work.join(path)).map(String::from_utf8) {
            Ok(Ok(content)) => {
                let fence = if content.contains("```") {
                    "````"
                } else {
                    "```"
                };
                let body = if content.ends_with('\n') {
                    content
                } else {
                    format!("{content}\n")
                };
                answer.push_str(&format!("\n### FILE: {path}\n{fence}\n{body}{fence}\n"));
            }
            _ => answer.push_str(&format!(
                "\nPlatform note: {path} is binary or unreadable and was not applied\n"
            )),
        }
    }
    if answer.is_empty() {
        answer.push_str("\n(Claude Code changed no files.)\n");
    }
    Ok(answer)
}

impl LlmProvider for ClaudeCodeProvider {
    fn model_name(&self) -> &str {
        &self.model
    }

    fn generate_plan<'a>(
        &'a self,
        system_prompt: &'a str,
        user_prompt: &'a str,
    ) -> BoxFuture<'a, Result<Completion>> {
        Box::pin(async move {
            let turn = self.run(system_prompt, user_prompt, Mode::Plan).await?;
            Ok(Completion {
                text: turn.text,
                usage: turn.usage,
            })
        })
    }

    fn generate<'a>(
        &'a self,
        system_prompt: &'a str,
        user_prompt: &'a str,
    ) -> BoxFuture<'a, Result<Completion>> {
        Box::pin(async move {
            let turn = self.run(system_prompt, user_prompt, self.mode).await?;
            Ok(Completion {
                text: turn.text,
                usage: turn.usage,
            })
        })
    }

    /// MCP tools are not passed on: Claude Code has its own tools. The conversation is sent
    /// as one prompt.
    fn chat<'a>(
        &'a self,
        system_prompt: &'a str,
        messages: &'a [ChatMessage],
        _tools: &'a [ToolSpec],
    ) -> BoxFuture<'a, Result<ChatTurn>> {
        Box::pin(async move {
            let prompt: String = messages
                .iter()
                .filter_map(|message| match message {
                    ChatMessage::User(text) => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n\n");
            self.run(system_prompt, &prompt, self.mode).await
        })
    }
}

/// The command that starts each CLI by default.
pub fn default_cli(provider: crate::config::ProviderKind) -> &'static str {
    match provider {
        crate::config::ProviderKind::Codex => "codex",
        _ => "claude",
    }
}

/// The provider for `provider: claude_code` and `provider: codex` contracts.
pub fn provider(config: &ModelConfig, role: Role) -> Result<Arc<dyn LlmProvider>> {
    Ok(Arc::new(ClaudeCodeProvider::new(
        config,
        Mode::for_role(role),
    )?))
}
