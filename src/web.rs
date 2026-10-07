//! The Owner's web interface on 127.0.0.1: the dashboard, and (with `writable`) the same
//! actions as the CLI - requests with files, questions, task decisions, project settings,
//! plans and hiring.
//!
//! Safety: the server binds 127.0.0.1 only and rejects other `Host` headers (DNS rebinding).
//! Every action needs the random token embedded in the page at startup, sent as a custom
//! header; a page from another site can neither read the token nor send that header without a
//! CORS preflight, which is never answered. Team runs go one at a time, like the CLI.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

use crate::{
    audit::hr_audit_dir,
    config::parse_project,
    dashboard::{Sources, snapshot},
    orchestrator::Orchestrator,
    owner::{self, NewRequest},
    project::{ProjectManager, ProjectSettings, RunSummary, Store},
    registry::{self, EmployeeStatus, Registry},
};

const PAGE: &str = include_str!("dashboard.html");
/// Tailwind output (`npm run css`), inlined into the page: no CDN, works offline.
const CSS: &str = include_str!("dashboard.css");
const TOKEN_HEADER: &str = "x-ai-team-token";
/// Largest request body (files arrive base64-encoded inside JSON).
const MAX_BODY_BYTES: usize = 40 * 1024 * 1024;
const RECENT_JOBS: usize = 10;
/// Largest project file the viewer shows.
const MAX_VIEW_BYTES: u64 = 512 * 1024;
const DEFAULT_RESUME_ITERATIONS: u32 = 2;

/// A team run started from the web page.
#[derive(Debug, Clone, Serialize)]
pub struct Job {
    pub id: u64,
    pub kind: String,
    pub project_id: Option<String>,
    pub started_unix_ms: u64,
    pub finished_unix_ms: Option<u64>,
    /// `running`, `done` or `failed`.
    pub status: String,
    pub message: String,
}

#[derive(Debug, Default)]
struct Jobs {
    next_id: u64,
    current: Option<Job>,
    recent: Vec<Job>,
}

struct Server {
    sources: Sources,
    port: u16,
    writable: bool,
    token: String,
    jobs: Mutex<Jobs>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or_default()
}

/// Serves the page until the process is stopped. `writable` enables the actions.
pub async fn serve(sources: Sources, port: u16, writable: bool) -> Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .with_context(|| format!("cannot listen on 127.0.0.1:{port}"))?;
    let server = Arc::new(Server {
        sources,
        port,
        writable,
        token: uuid::Uuid::new_v4().simple().to_string(),
        jobs: Mutex::default(),
    });
    println!(
        "{}: http://127.0.0.1:{port}  (Ctrl+C to stop)",
        if writable {
            "Web interface"
        } else {
            "Dashboard (read-only)"
        }
    );
    loop {
        let (stream, _) = listener.accept().await?;
        let server = server.clone();
        tokio::spawn(async move {
            if let Err(err) = handle(stream, &server).await {
                tracing::debug!("web request failed: {err:#}");
            }
        });
    }
}

struct Request {
    method: String,
    path: String,
    query: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

async fn read_request(stream: &mut TcpStream) -> Result<Request> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
    let header_end = loop {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            bail!("connection closed before the headers ended");
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break end;
        }
        anyhow::ensure!(buffer.len() < 64 * 1024, "headers too large");
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let mut lines = head.lines();
    let mut first = lines.next().unwrap_or("").split_whitespace();
    let method = first.next().unwrap_or("").to_string();
    let target = first.next().unwrap_or("/");
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let (path, query) = (path.to_string(), query.to_string());
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.trim().to_string(), value.trim().to_string()))
        .collect();
    let length: usize = headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.parse().ok())
        .unwrap_or(0);
    anyhow::ensure!(length <= MAX_BODY_BYTES, "request body too large");
    let mut body = buffer[header_end + 4..].to_vec();
    while body.len() < length {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            bail!("connection closed before the body ended");
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(length);
    Ok(Request {
        method,
        path,
        query,
        headers,
        body,
    })
}

async fn respond(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &[u8],
) -> Result<()> {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n\
         X-Frame-Options: DENY\r\nReferrer-Policy: no-referrer\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.shutdown().await?;
    Ok(())
}

async fn respond_json(stream: &mut TcpStream, status: &str, value: &Value) -> Result<()> {
    respond(
        stream,
        status,
        "application/json",
        &serde_json::to_vec(value)?,
    )
    .await
}

async fn handle(mut stream: TcpStream, server: &Arc<Server>) -> Result<()> {
    let request = match read_request(&mut stream).await {
        Ok(request) => request,
        Err(err) => {
            return respond_json(
                &mut stream,
                "400 Bad Request",
                &json!({ "error": format!("{err:#}") }),
            )
            .await;
        }
    };

    // DNS rebinding: only our own host names.
    let allowed_hosts = [
        format!("127.0.0.1:{}", server.port),
        format!("localhost:{}", server.port),
    ];
    if !request
        .header("host")
        .is_some_and(|host| allowed_hosts.iter().any(|allowed| allowed == host))
    {
        return respond_json(
            &mut stream,
            "403 Forbidden",
            &json!({ "error": "unexpected Host" }),
        )
        .await;
    }

    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/") => {
            let page = PAGE
                .replace("/*__AI_TEAM_CSS__*/", CSS)
                .replace(
                    "__AI_TEAM_TOKEN__",
                    if server.writable { &server.token } else { "" },
                )
                .replace(
                    "__AI_TEAM_WRITABLE__",
                    if server.writable { "true" } else { "false" },
                );
            respond(
                &mut stream,
                "200 OK",
                "text/html; charset=utf-8",
                page.as_bytes(),
            )
            .await
        }
        ("GET", "/api/snapshot") => {
            let sources = server.sources.clone();
            let result = tokio::task::spawn_blocking(move || snapshot(&sources)).await?;
            match result {
                Ok(mut value) => {
                    value["writable"] = json!(server.writable);
                    {
                        let jobs = server.jobs.lock().expect("jobs lock");
                        value["job"] = json!(jobs.current);
                        value["recent_jobs"] = json!(jobs.recent);
                    }
                    respond_json(&mut stream, "200 OK", &value).await
                }
                Err(err) => {
                    respond_json(
                        &mut stream,
                        "500 Internal Server Error",
                        &json!({ "error": format!("{err:#}") }),
                    )
                    .await
                }
            }
        }
        ("POST", path) if path.starts_with("/api/") => {
            if !server.writable {
                return respond_json(
                    &mut stream,
                    "403 Forbidden",
                    &json!({ "error": "read-only dashboard; start `ai-team web` for actions" }),
                )
                .await;
            }
            if request.header(TOKEN_HEADER) != Some(server.token.as_str()) {
                return respond_json(
                    &mut stream,
                    "403 Forbidden",
                    &json!({ "error": "missing or wrong token" }),
                )
                .await;
            }
            if let Some(origin) = request.header("origin")
                && !allowed_hosts
                    .iter()
                    .any(|host| origin == format!("http://{host}"))
            {
                return respond_json(
                    &mut stream,
                    "403 Forbidden",
                    &json!({ "error": "unexpected Origin" }),
                )
                .await;
            }
            let body: Value = match serde_json::from_slice(&request.body) {
                Ok(body) => body,
                Err(err) => {
                    return respond_json(
                        &mut stream,
                        "400 Bad Request",
                        &json!({ "error": format!("invalid JSON: {err}") }),
                    )
                    .await;
                }
            };
            let action = path.trim_start_matches("/api/").to_string();
            match act(server.clone(), &action, body).await {
                Ok(value) => respond_json(&mut stream, "200 OK", &value).await,
                Err(err) => {
                    let status = if format!("{err:#}").starts_with("busy:") {
                        "409 Conflict"
                    } else {
                        "400 Bad Request"
                    };
                    respond_json(
                        &mut stream,
                        status,
                        &json!({ "error": format!("{err:#}").trim_start_matches("busy: ") }),
                    )
                    .await
                }
            }
        }
        ("GET", "/api/files" | "/api/file") => {
            let sources = server.sources.clone();
            let params = parse_query(&request.query);
            let one = request.path == "/api/file";
            let result =
                tokio::task::spawn_blocking(move || project_files(&sources, &params, one)).await?;
            match result {
                Ok(value) => respond_json(&mut stream, "200 OK", &value).await,
                Err(err) => {
                    respond_json(
                        &mut stream,
                        "400 Bad Request",
                        &json!({ "error": format!("{err:#}") }),
                    )
                    .await
                }
            }
        }
        ("GET", _) => respond(&mut stream, "404 Not Found", "text/plain", b"not found").await,
        _ => {
            respond(
                &mut stream,
                "405 Method Not Allowed",
                "text/plain",
                b"method not allowed",
            )
            .await
        }
    }
}

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

#[derive(Deserialize, Default)]
struct UploadedFile {
    name: String,
    /// File content, base64.
    data: String,
}

#[derive(Deserialize)]
struct RequestAction {
    prompt: String,
    #[serde(default)]
    project_id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    test_command: Option<String>,
    #[serde(default)]
    files: Vec<UploadedFile>,
}

#[derive(Deserialize)]
struct TaskDecision {
    task_id: String,
    #[serde(default)]
    note: Option<String>,
    #[serde(default)]
    iterations: Option<u32>,
}

#[derive(Deserialize)]
struct ConfigureAction {
    project_id: String,
    #[serde(default)]
    test_command: Option<String>,
    #[serde(default)]
    test_timeout: Option<u64>,
    #[serde(default)]
    remote: Option<String>,
    #[serde(default)]
    budget_tokens: Option<u64>,
    #[serde(default)]
    budget_usd: Option<f64>,
}

fn field(body: &Value, name: &str) -> Result<String> {
    body[name]
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .with_context(|| format!("`{name}` is required"))
}

fn optional(body: &Value, name: &str) -> Option<String> {
    body[name]
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// Runs `work` on its own thread with its own runtime: the team's futures borrow the store
/// and need not be `Send`.
async fn blocking<T, F>(work: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(work).await?
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?)
}

fn open(sources: &Sources) -> Result<(Store, Orchestrator)> {
    Ok((
        Store::open(&sources.data_dir)?,
        Orchestrator::from_registry(Registry::load(&sources.employees_dir)?)?
            .with_audit_dir(sources.audit_dir.clone()),
    ))
}

fn company_dir(sources: &Sources) -> PathBuf {
    sources
        .employees_dir
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("company"))
}

fn summary_message(summary: &RunSummary) -> String {
    let mut parts: Vec<String> = summary
        .executed
        .iter()
        .map(|(task, status)| format!("{task}: {status}"))
        .collect();
    if parts.is_empty() {
        parts.push("no task was ready to run".to_string());
    }
    if let Some(reason) = &summary.budget_stop {
        parts.push(format!("BUDGET: {reason}"));
    }
    parts.extend(
        summary
            .push_errors
            .iter()
            .map(|error| format!("push: {error}")),
    );
    parts.push(format!("project {}", summary.project_status));
    parts.join("; ")
}

/// Starts a team run in the background; only one at a time.
fn start_job<F>(
    server: &Arc<Server>,
    kind: &str,
    project_id: Option<String>,
    work: F,
) -> Result<Value>
where
    F: FnOnce(&Sources) -> Result<(Option<String>, String)> + Send + 'static,
{
    let job = {
        let mut jobs = server.jobs.lock().expect("jobs lock");
        if let Some(current) = &jobs.current {
            bail!(
                "busy: the team is already working ({}{}); wait for it to finish",
                current.kind,
                current
                    .project_id
                    .as_deref()
                    .map(|id| format!(" on {id}"))
                    .unwrap_or_default()
            );
        }
        jobs.next_id += 1;
        let job = Job {
            id: jobs.next_id,
            kind: kind.to_string(),
            project_id,
            started_unix_ms: now_ms(),
            finished_unix_ms: None,
            status: "running".to_string(),
            message: String::new(),
        };
        jobs.current = Some(job.clone());
        job
    };

    let server = server.clone();
    let id = job.id;
    tokio::task::spawn_blocking(move || {
        let outcome = work(&server.sources);
        let mut jobs = server.jobs.lock().expect("jobs lock");
        if let Some(mut job) = jobs.current.take() {
            job.finished_unix_ms = Some(now_ms());
            match outcome {
                Ok((project_id, message)) => {
                    job.status = "done".to_string();
                    job.project_id = project_id.or(job.project_id);
                    job.message = message;
                }
                Err(err) => {
                    job.status = "failed".to_string();
                    job.message = format!("{err:#}");
                }
            }
            jobs.recent.insert(0, job);
            jobs.recent.truncate(RECENT_JOBS);
        }
    });
    Ok(json!({ "job_id": id, "started": true }))
}

async fn act(server: Arc<Server>, action: &str, body: Value) -> Result<Value> {
    let sources = server.sources.clone();
    match action {
        "request" => {
            let request: RequestAction = serde_json::from_value(body).context("invalid request")?;
            anyhow::ensure!(!request.prompt.trim().is_empty(), "the request is empty");
            // Files land in a private upload folder, then the platform copies them to inputs/.
            let upload = sources
                .data_dir
                .join("uploads")
                .join(uuid::Uuid::new_v4().simple().to_string());
            let mut files = Vec::new();
            for file in &request.files {
                let name = safe_file_name(&file.name)?;
                let bytes = decode_base64(&file.data)
                    .with_context(|| format!("file {name} is not valid base64"))?;
                std::fs::create_dir_all(&upload)?;
                let path = upload.join(&name);
                std::fs::write(&path, bytes)?;
                files.push(path);
            }
            let new_request = NewRequest {
                prompt: request.prompt,
                files,
                project_id: request.project_id.filter(|id| !id.trim().is_empty()),
                new_project_name: request.name.filter(|name| !name.trim().is_empty()),
                test_command: request
                    .test_command
                    .filter(|command| !command.trim().is_empty()),
            };
            let project = new_request.project_id.clone();
            start_job(&server, "request", project, move |sources| {
                let result = (|| {
                    let (store, orchestrator) = open(sources)?;
                    let manager = ProjectManager::new(&store, &orchestrator).with_progress(true);
                    let prepared = owner::prepare(&store, &manager, &new_request)?;
                    let task = owner::add_request_task(&store, &manager, &new_request, &prepared)?;
                    let summary = runtime()?.block_on(manager.run(&prepared.project_id, None))?;
                    Ok((
                        Some(prepared.project_id.clone()),
                        format!("{} created; {}", task.id, summary_message(&summary)),
                    ))
                })();
                let _ = std::fs::remove_dir_all(&upload);
                result
            })
        }
        "run" => {
            let project_id = field(&body, "project_id")?;
            start_job(&server, "run", Some(project_id.clone()), move |sources| {
                let (store, orchestrator) = open(sources)?;
                let manager = ProjectManager::new(&store, &orchestrator).with_progress(true);
                let summary = runtime()?.block_on(manager.run(&project_id, None))?;
                Ok((None, summary_message(&summary)))
            })
        }
        "resume" => {
            let decision: TaskDecision =
                serde_json::from_value(body).context("invalid decision")?;
            let iterations = decision.iterations.unwrap_or(DEFAULT_RESUME_ITERATIONS);
            anyhow::ensure!(
                (1..=10).contains(&iterations),
                "iterations must be between 1 and 10"
            );
            let project_id = {
                let store = Store::open(&sources.data_dir)?;
                store.task(&decision.task_id)?.project_id
            };
            start_job(
                &server,
                "resume",
                Some(project_id.clone()),
                move |sources| {
                    let (store, orchestrator) = open(sources)?;
                    let manager = ProjectManager::new(&store, &orchestrator).with_progress(true);
                    manager.resume_task(&decision.task_id, decision.note.as_deref(), iterations)?;
                    let summary = runtime()?.block_on(manager.run(&project_id, None))?;
                    Ok((None, summary_message(&summary)))
                },
            )
        }
        "plan" => {
            let project_id = field(&body, "project_id")?;
            let note = optional(&body, "note");
            start_job(&server, "plan", Some(project_id.clone()), move |sources| {
                let (store, orchestrator) = open(sources)?;
                let manager = ProjectManager::new(&store, &orchestrator);
                let plan = runtime()?.block_on(manager.plan(&project_id, note.as_deref()))?;
                let tasks: usize = plan
                    .milestones
                    .iter()
                    .map(|milestone| milestone.tasks.len())
                    .sum();
                Ok((
                    None,
                    format!(
                        "plan proposed: {} milestone(s), {tasks} task(s); approve it to start",
                        plan.milestones.len()
                    ),
                ))
            })
        }
        "ask" => {
            let project_id = optional(&body, "project_id");
            let question = field(&body, "question")?;
            // Documents for this question only: they go into the prompt and are never saved.
            let files: Vec<UploadedFile> =
                serde_json::from_value(body["files"].clone()).unwrap_or_default();
            let mut attachments = Vec::new();
            for file in &files {
                let name = safe_file_name(&file.name)?;
                let bytes = decode_base64(&file.data)
                    .with_context(|| format!("file {name} is not valid base64"))?;
                let text = String::from_utf8(bytes)
                    .map_err(|_| anyhow::anyhow!("{name} is not a text file; questions take text documents (.md, .txt, code, CSV)"))?;
                attachments.push((name, text));
            }
            let answer = blocking(move || {
                let (store, orchestrator) = open(&sources)?;
                let manager = ProjectManager::new(&store, &orchestrator);
                runtime()?.block_on(manager.ask_with(
                    project_id.as_deref(),
                    &question,
                    &attachments,
                ))
            })
            .await?;
            Ok(json!({ "answer": answer }))
        }
        "accept" | "cancel" => {
            let decision: TaskDecision =
                serde_json::from_value(body).context("invalid decision")?;
            let action = action.to_string();
            let task = blocking(move || {
                let (store, orchestrator) = open(&sources)?;
                let manager = ProjectManager::new(&store, &orchestrator);
                let note = decision.note.as_deref();
                let task = if action == "accept" {
                    manager.accept_task(
                        &decision.task_id,
                        note.or(Some("accepted from the web interface")),
                    )?
                } else {
                    manager.cancel_task(
                        &decision.task_id,
                        note.or(Some("cancelled from the web interface")),
                    )?
                };
                Ok(format!("{} {}", task.id, task.status))
            })
            .await?;
            Ok(json!({ "message": task }))
        }
        "approve" => {
            let project_id = field(&body, "project_id")?;
            let note = optional(&body, "note");
            let count = blocking(move || {
                let (store, orchestrator) = open(&sources)?;
                Ok(ProjectManager::new(&store, &orchestrator)
                    .approve(&project_id, note.as_deref())?
                    .len())
            })
            .await?;
            Ok(json!({ "message": format!("plan approved: {count} task(s) created") }))
        }
        "configure" => {
            let change: ConfigureAction =
                serde_json::from_value(body).context("invalid settings")?;
            let message = blocking(move || {
                let (store, orchestrator) = open(&sources)?;
                let config = ProjectManager::new(&store, &orchestrator).configure(
                    &change.project_id,
                    ProjectSettings {
                        test_command: change.test_command,
                        test_timeout_secs: change.test_timeout,
                        remote_url: change.remote,
                        budget_tokens: change.budget_tokens,
                        budget_usd: change.budget_usd,
                    },
                )?;
                Ok(format!("{} saved", config.project_id))
            })
            .await?;
            Ok(json!({ "message": message }))
        }
        "add_project" => {
            let raw = field(&body, "json")?;
            let message = blocking(move || {
                let config = parse_project(&raw)?;
                Store::open(&sources.data_dir)?.add_project(&config)?;
                Ok(format!(
                    "project {} added; propose a plan next",
                    config.project_id
                ))
            })
            .await?;
            Ok(json!({ "message": message }))
        }
        "hire_check" | "hire_approve" => {
            let employee_id = field(&body, "employee_id")?;
            let approve = action == "hire_approve";
            let message = blocking(move || {
                let company = company_dir(&sources);
                let employee = if approve {
                    registry::approve_hire(&company, &employee_id, &hr_audit_dir())?
                } else {
                    registry::check_proposal(&company, &employee_id)?
                };
                Ok(format!(
                    "{employee_id} {}: {} [{}], reports to {}",
                    if approve { "hired" } else { "is valid" },
                    employee.contract.name,
                    employee.contract.function,
                    employee.contract.manager_id
                ))
            })
            .await?;
            Ok(json!({ "message": message }))
        }
        "set_status" => {
            let employee_id = field(&body, "employee_id")?;
            let status: EmployeeStatus = serde_json::from_value(body["status"].clone())
                .context("status must be active, suspended or disabled")?;
            let message = blocking(move || {
                registry::set_status(
                    &company_dir(&sources),
                    &employee_id,
                    status,
                    &hr_audit_dir(),
                )?;
                Ok(format!(
                    "{employee_id} is now {}",
                    body["status"].as_str().unwrap_or("")
                ))
            })
            .await?;
            Ok(json!({ "message": message }))
        }
        other => bail!("unknown action `{other}`"),
    }
}

/// `a=1&b=x%20y` as decoded pairs.
fn parse_query(query: &str) -> Vec<(String, String)> {
    fn decode(text: &str) -> String {
        let bytes = text.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut index = 0;
        while index < bytes.len() {
            match bytes[index] {
                b'+' => out.push(b' '),
                b'%' if index + 2 < bytes.len() => {
                    let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
                    match u8::from_str_radix(hex, 16) {
                        Ok(byte) => {
                            out.push(byte);
                            index += 2;
                        }
                        Err(_) => out.push(b'%'),
                    }
                }
                byte => out.push(byte),
            }
            index += 1;
        }
        String::from_utf8_lossy(&out).to_string()
    }
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (decode(key), decode(value))
        })
        .collect()
}

/// The project's files (`one = false`) or one file's content (`one = true`), read-only.
/// Only paths the workspace lists can be read: no traversal, no `.git`, no build folders.
fn project_files(sources: &Sources, params: &[(String, String)], one: bool) -> Result<Value> {
    let param = |name: &str| {
        params
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
            .with_context(|| format!("`{name}` is required"))
    };
    let project_id = param("project")?;
    let store = Store::open(&sources.data_dir)?;
    store.project(&project_id)?; // a known project, so the folder name is safe
    let root = sources.data_dir.join("workspaces").join(&project_id);
    if !root.is_dir() {
        anyhow::ensure!(!one, "the project has no files yet");
        return Ok(json!({ "files": [] }));
    }
    let workspace = crate::workspace::Workspace::open(&root)?;
    let files = workspace.files()?;
    if !one {
        let listed: Vec<Value> = files
            .iter()
            .filter(|path| path.as_str() != ".gitignore")
            .map(|path| {
                let size = std::fs::metadata(root.join(path))
                    .map(|meta| meta.len())
                    .unwrap_or(0);
                json!({ "path": path, "size": size })
            })
            .collect();
        return Ok(json!({ "files": listed }));
    }
    let path = crate::workspace::validate_path(&param("path")?)?;
    anyhow::ensure!(files.contains(&path), "{path} is not a file of the project");
    let full = root.join(&path);
    let size = std::fs::metadata(&full)?.len();
    if size > MAX_VIEW_BYTES {
        return Ok(
            json!({ "path": path, "size": size, "text": null, "note": "file too large to show" }),
        );
    }
    let bytes = std::fs::read(&full)?;
    Ok(match String::from_utf8(bytes) {
        Ok(text) => json!({ "path": path, "size": size, "text": text }),
        Err(_) => json!({ "path": path, "size": size, "text": null, "note": "binary file" }),
    })
}

/// The last path component of an uploaded file name, checked like a workspace path.
fn safe_file_name(raw: &str) -> Result<String> {
    let name = raw
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    anyhow::ensure!(
        !name.is_empty() && name != "." && name != ".." && !name.starts_with('.'),
        "invalid file name `{raw}`"
    );
    crate::workspace::validate_path(&format!("inputs/{name}"))
        .with_context(|| format!("invalid file name `{raw}`"))?;
    Ok(name)
}

/// Standard base64 (with or without padding; whitespace ignored).
fn decode_base64(text: &str) -> Result<Vec<u8>> {
    fn value(byte: u8) -> Option<u32> {
        match byte {
            b'A'..=b'Z' => Some(u32::from(byte - b'A')),
            b'a'..=b'z' => Some(u32::from(byte - b'a') + 26),
            b'0'..=b'9' => Some(u32::from(byte - b'0') + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let clean: Vec<u8> = text
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace() && *byte != b'=')
        .collect();
    anyhow::ensure!(clean.len() % 4 != 1, "truncated base64");
    let mut out = Vec::with_capacity(clean.len() * 3 / 4);
    for chunk in clean.chunks(4) {
        let mut acc = 0u32;
        for (index, byte) in chunk.iter().enumerate() {
            acc |= value(*byte).context("invalid base64 character")? << (18 - 6 * index);
        }
        out.push((acc >> 16) as u8);
        if chunk.len() > 2 {
            out.push((acc >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(acc as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_base64() {
        assert_eq!(decode_base64("QW5hCk1paGFp").unwrap(), b"Ana\nMihai");
        assert_eq!(decode_base64("YQ==").unwrap(), b"a");
        assert_eq!(decode_base64("YWI=").unwrap(), b"ab");
        assert!(decode_base64("Y").is_err());
        assert!(decode_base64("@@@@").is_err());
    }

    #[test]
    fn parses_queries() {
        assert_eq!(
            parse_query("project=demo&path=src%2Fa%20b.py&x=1+2&bad=%zz"),
            [
                ("project".to_string(), "demo".to_string()),
                ("path".to_string(), "src/a b.py".to_string()),
                ("x".to_string(), "1 2".to_string()),
                ("bad".to_string(), "%zz".to_string()),
            ]
        );
    }

    #[test]
    fn uploaded_names_cannot_escape() {
        assert_eq!(
            safe_file_name("C:\\Users\\x\\nume.txt").unwrap(),
            "nume.txt"
        );
        assert_eq!(safe_file_name("../../etc/passwd").unwrap(), "passwd");
        for bad in ["", "..", ".env", "con.txt", "a:b"] {
            assert!(safe_file_name(bad).is_err(), "{bad:?}");
        }
    }
}
