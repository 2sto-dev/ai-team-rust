//! Model Context Protocol client: starts the MCP servers an employee may use (stdio
//! transport, JSON-RPC 2.0), lists their tools and calls them on the model's behalf.
//!
//! Servers are defined once by the Owner in `company/mcp.yaml`; a contract only names them.
//! A server gets a minimal environment plus the variables its definition allows.

use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::{Mutex, OnceCell},
};

use crate::{llm::ToolSpec, workspace::allowed_env};

const PROTOCOL_VERSION: &str = "2025-06-18";
/// Longest tool result passed back to the model.
pub const TOOL_RESULT_CHARS: usize = 8000;

/// How to start one MCP server (an entry of `company/mcp.yaml`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerConfig {
    #[serde(default)]
    pub description: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Working directory, relative to the company folder (default: the company folder).
    #[serde(default)]
    pub cwd: Option<String>,
    /// Literal, non-secret variables.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Variables copied from the platform's environment (how secrets reach a server).
    #[serde(default)]
    pub env_from: Vec<String>,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
}

fn default_timeout_secs() -> u64 {
    30
}

/// `company/mcp.yaml`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpCatalog {
    #[serde(default)]
    pub servers: BTreeMap<String, McpServerConfig>,
}

impl McpCatalog {
    pub fn validate(&self) -> Vec<String> {
        let mut errors = Vec::new();
        for (name, server) in &self.servers {
            if name.is_empty()
                || !name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            {
                errors.push(format!(
                    "MCP server name '{name}' must use lowercase letters, digits and '-'"
                ));
            }
            if server.command.trim().is_empty() {
                errors.push(format!("MCP server {name}: command is empty"));
            }
            if server.timeout_secs == 0 {
                errors.push(format!(
                    "MCP server {name}: timeout_secs must be at least 1"
                ));
            }
        }
        errors
    }
}

/// What a tool returned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToolOutcome {
    pub text: String,
    pub is_error: bool,
}

/// A running MCP server.
pub struct McpClient {
    name: String,
    _child: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
    next_id: u64,
    timeout: Duration,
    tools: Vec<Value>,
}

impl McpClient {
    /// Starts the server, performs the MCP handshake and lists its tools.
    pub async fn connect(name: &str, config: &McpServerConfig, company_dir: &Path) -> Result<Self> {
        let cwd: PathBuf = match &config.cwd {
            Some(dir) => company_dir.join(dir),
            None => company_dir.to_path_buf(),
        };
        let mut command = Command::new(&config.command);
        command
            .args(&config.args)
            .current_dir(&cwd)
            .env_clear()
            .envs(allowed_env())
            .envs(&config.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        for variable in &config.env_from {
            let value = std::env::var(variable).with_context(|| {
                format!("MCP server {name} needs environment variable {variable}")
            })?;
            command.env(variable, value);
        }

        let mut child = command
            .spawn()
            .with_context(|| format!("cannot start MCP server {name} (`{}`)", config.command))?;
        let stdin = child.stdin.take().context("MCP server has no stdin")?;
        let stdout =
            BufReader::new(child.stdout.take().context("MCP server has no stdout")?).lines();

        let mut client = Self {
            name: name.to_string(),
            _child: child,
            stdin,
            stdout,
            next_id: 1,
            timeout: Duration::from_secs(config.timeout_secs),
            tools: Vec::new(),
        };

        client
            .request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": { "name": "ai-team", "version": env!("CARGO_PKG_VERSION") },
                }),
            )
            .await
            .with_context(|| format!("MCP server {name}: initialize failed"))?;
        client
            .notify("notifications/initialized", json!({}))
            .await?;

        let mut cursor: Option<String> = None;
        loop {
            let params = match &cursor {
                Some(cursor) => json!({ "cursor": cursor }),
                None => json!({}),
            };
            let page = client
                .request("tools/list", params)
                .await
                .with_context(|| format!("MCP server {name}: tools/list failed"))?;
            if let Some(tools) = page["tools"].as_array() {
                client.tools.extend(tools.iter().cloned());
            }
            match page["nextCursor"].as_str() {
                Some(next) if !next.is_empty() => cursor = Some(next.to_string()),
                _ => break,
            }
        }
        Ok(client)
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Raw tool definitions (`name`, `description`, `inputSchema`) as the server sent them.
    pub fn tools(&self) -> &[Value] {
        &self.tools
    }

    async fn send(&mut self, message: &Value) -> Result<()> {
        let mut line = serde_json::to_string(message)?;
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .await
            .with_context(|| format!("MCP server {} stopped accepting input", self.name))?;
        self.stdin.flush().await?;
        Ok(())
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.send(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))
            .await
    }

    /// Sends a request and waits for its response. Server notifications are ignored and
    /// server-initiated requests are declined (this client offers no capabilities).
    async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .await?;

        let timeout = self.timeout;
        let name = self.name.clone();
        tokio::time::timeout(timeout, async {
            loop {
                let line = self
                    .stdout
                    .next_line()
                    .await?
                    .ok_or_else(|| anyhow!("MCP server {name} closed its output"))?;
                let Ok(message) = serde_json::from_str::<Value>(&line) else {
                    continue; // not protocol traffic (a server logging to stdout)
                };
                if message["id"] == json!(id) && message.get("method").is_none() {
                    if let Some(error) = message.get("error") {
                        bail!(
                            "MCP server {name}: {} (code {})",
                            error["message"].as_str().unwrap_or("error"),
                            error["code"]
                        );
                    }
                    return Ok(message["result"].clone());
                }
                if message.get("method").is_some() && message.get("id").is_some() {
                    let reply = json!({
                        "jsonrpc": "2.0",
                        "id": message["id"],
                        "error": { "code": -32601, "message": "not supported by this client" },
                    });
                    self.send(&reply).await?;
                }
            }
        })
        .await
        .map_err(|_| {
            anyhow!(
                "MCP server {} did not answer {method} within {} s",
                self.name,
                timeout.as_secs()
            )
        })?
    }

    pub async fn call_tool(&mut self, tool: &str, arguments: Value) -> Result<ToolOutcome> {
        let result = self
            .request(
                "tools/call",
                json!({ "name": tool, "arguments": arguments }),
            )
            .await?;
        let mut text: String = result["content"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .map(|item| match item["type"].as_str() {
                        Some("text") => item["text"].as_str().unwrap_or("").to_string(),
                        Some(other) => format!("[{other} content omitted]"),
                        None => String::new(),
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        if text.is_empty()
            && let Some(structured) = result.get("structuredContent")
        {
            text = structured.to_string();
        }
        if text.chars().count() > TOOL_RESULT_CHARS {
            text =
                text.chars().take(TOOL_RESULT_CHARS).collect::<String>() + "\n[... truncated ...]";
        }
        Ok(ToolOutcome {
            text,
            is_error: result["isError"].as_bool().unwrap_or(false),
        })
    }
}

/// Model-facing name: `<server>__<tool>`, limited to the characters every dialect accepts.
fn qualified_name(server: &str, tool: &str) -> String {
    let raw = format!("{server}__{tool}");
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect()
}

struct Connected {
    clients: Vec<Mutex<McpClient>>,
    specs: Vec<ToolSpec>,
    /// Model-facing tool name -> (client index, server's tool name).
    routes: HashMap<String, (usize, String)>,
}

/// The MCP tools of one employee. Servers start on first use and stop when it is dropped.
pub struct Toolbox {
    servers: Vec<(String, McpServerConfig)>,
    company_dir: PathBuf,
    connected: OnceCell<Connected>,
}

impl Toolbox {
    pub fn new(servers: Vec<(String, McpServerConfig)>, company_dir: impl Into<PathBuf>) -> Self {
        Self {
            servers,
            company_dir: company_dir.into(),
            connected: OnceCell::new(),
        }
    }

    pub fn server_names(&self) -> Vec<&str> {
        self.servers.iter().map(|(name, _)| name.as_str()).collect()
    }

    async fn connected(&self) -> Result<&Connected> {
        self.connected
            .get_or_try_init(|| async {
                let mut clients = Vec::new();
                let mut specs = Vec::new();
                let mut routes = HashMap::new();
                for (index, (name, config)) in self.servers.iter().enumerate() {
                    let client = McpClient::connect(name, config, &self.company_dir).await?;
                    for tool in client.tools() {
                        let Some(tool_name) = tool["name"].as_str() else {
                            continue;
                        };
                        let qualified = qualified_name(name, tool_name);
                        routes.insert(qualified.clone(), (index, tool_name.to_string()));
                        specs.push(ToolSpec {
                            name: qualified,
                            description: format!(
                                "[{name}] {}",
                                tool["description"].as_str().unwrap_or("")
                            ),
                            parameters: tool
                                .get("inputSchema")
                                .cloned()
                                .unwrap_or_else(|| json!({ "type": "object", "properties": {} })),
                        });
                    }
                    clients.push(Mutex::new(client));
                }
                Ok::<_, anyhow::Error>(Connected {
                    clients,
                    specs,
                    routes,
                })
            })
            .await
    }

    /// The tools offered to the model (starts the servers on first use).
    pub async fn specs(&self) -> Result<&[ToolSpec]> {
        Ok(&self.connected().await?.specs)
    }

    /// Runs one tool call. An unknown tool or a failing call comes back as an error outcome
    /// for the model to read, not as a failure of the whole task.
    pub async fn call(&self, qualified: &str, arguments: Value) -> Result<ToolOutcome> {
        let connected = self.connected().await?;
        let Some((index, tool)) = connected.routes.get(qualified) else {
            return Ok(ToolOutcome {
                text: format!("unknown tool '{qualified}'"),
                is_error: true,
            });
        };
        let mut client = connected.clients[*index].lock().await;
        match client.call_tool(tool, arguments).await {
            Ok(outcome) => Ok(outcome),
            Err(err) => Ok(ToolOutcome {
                text: format!("tool call failed: {err:#}"),
                is_error: true,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qualified_names_fit_every_dialect() {
        assert_eq!(qualified_name("pydoc", "lookup"), "pydoc__lookup");
        assert_eq!(qualified_name("fs", "read.file"), "fs__read_file");
        assert!(qualified_name("a", &"x".repeat(100)).len() <= 64);
    }

    #[test]
    fn catalog_validation() {
        let catalog: McpCatalog = serde_yaml_ng::from_str(
            "servers:\n  Bad_Name:\n    command: ''\n    timeout_secs: 0\n",
        )
        .unwrap();
        let errors = catalog.validate().join(" | ");
        assert!(errors.contains("lowercase") && errors.contains("command is empty"));
        assert!(errors.contains("timeout_secs"));
    }
}
