use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};

use ai_team::{
    audit::hr_audit_dir,
    config::{CLAUDE_DEFAULT_API_KEY_ENV, ModelConfig, ProviderKind, load_project},
    orchestrator::Orchestrator,
    owner::{self, normalize_project_name},
    project::{ProjectManager, ProjectSettings, ProjectStatus, Store},
    registry::{
        self, EmployeeFunction, EmployeeStatus, EmployeeType, HireRequest, OWNER, Registry,
    },
};
use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(name = "ai-team")]
#[command(about = "Reusable Rust AI engineering company")]
struct Cli {
    /// Company folder holding employees/ and proposals/.
    #[arg(long, default_value = "company", global = true)]
    company: PathBuf,

    /// Data folder for the project database, artifacts and milestone reports.
    #[arg(long, default_value = "data", global = true)]
    data: PathBuf,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Validate the employee registry and check that the configured model servers answer.
    Doctor {
        /// Skip the network checks (registry validation only).
        #[arg(long)]
        offline: bool,
    },

    /// Show the org chart.
    Team,

    /// Interactive console: type requests, attach files, watch the team, decide on stopped tasks.
    Console {
        /// Continue an existing project instead of starting a new one.
        #[arg(long)]
        project: Option<String>,
    },

    /// Read-only web dashboard on 127.0.0.1: projects, tasks, KPIs, team, activity.
    Dashboard {
        #[arg(long, default_value_t = 8787)]
        port: u16,
    },

    /// Web interface on 127.0.0.1: the dashboard plus every Owner action of the CLI.
    Web {
        #[arg(long, default_value_t = 8787)]
        port: u16,
    },

    /// KPIs of the AI employees, over all projects or one.
    Kpi {
        #[arg(long)]
        project: Option<String>,
    },

    /// Ask a question about a project; answered from its files, nothing changes.
    Ask {
        project_id: String,
        question: String,
    },

    /// Give the team a task in your own words, with optional files; the team starts at once.
    Request {
        /// What you want done.
        prompt: String,
        /// A file for the team (repeatable); copied to inputs/ in the project.
        #[arg(long = "file")]
        files: Vec<PathBuf>,
        /// Add the request to an existing project instead of creating one.
        #[arg(long)]
        project: Option<String>,
        /// Name (id) for the new project, e.g. "ulise".
        #[arg(long)]
        name: Option<String>,
        /// Command that must pass, run in the workspace after every Builder answer.
        #[arg(long)]
        test_command: Option<String>,
        /// Let the Planner split the request into tasks (shown for your approval).
        #[arg(long)]
        plan: bool,
        /// Approve the plan without asking (with --plan).
        #[arg(long)]
        yes: bool,
    },

    /// Projects: add, plan, approve, run, status.
    Project {
        #[command(subcommand)]
        action: ProjectAction,
    },

    /// Tasks: show, and Owner decisions on stopped tasks.
    Task {
        #[command(subcommand)]
        action: TaskAction,
    },

    /// Run one ad-hoc task (not persisted).
    Run {
        #[arg(long)]
        project: PathBuf,

        #[arg(long)]
        task: String,

        #[arg(long)]
        json: bool,
    },

    /// Hiring procedure: propose, check, approve.
    Hire {
        #[command(subcommand)]
        action: HireAction,
    },

    /// Manage existing employees.
    Employees {
        #[command(subcommand)]
        action: EmployeeAction,
    },
}

#[derive(Debug, Subcommand)]
enum ProjectAction {
    /// Register a project from its JSON file.
    Add { file: PathBuf },

    /// List projects.
    List,

    /// Ask the Planner for a task plan (stored as pending until approved).
    Plan {
        project_id: String,
        /// Guidance for the Planner, e.g. what to change in the previous proposal.
        #[arg(long)]
        note: Option<String>,
    },

    /// Owner approval of the pending plan: creates the milestones and tasks.
    Approve {
        project_id: String,
        #[arg(long)]
        note: Option<String>,
    },

    /// Execute ready tasks one at a time (interrupted tasks resume first).
    Run {
        project_id: String,
        /// Stop after this many tasks.
        #[arg(long)]
        max_tasks: Option<usize>,
    },

    /// Milestones, tasks and what waits for the Owner.
    Status { project_id: String },

    /// Permanently delete a project: records, statistics, workspace, reports, run audits.
    Delete {
        project_id: String,
        /// Skip the confirmation question.
        #[arg(long)]
        yes: bool,
    },

    /// Owner settings that can change after `project add`.
    Configure {
        project_id: String,
        /// Command run in the task workspace after every Builder answer; "" removes it.
        #[arg(long)]
        test_command: Option<String>,
        #[arg(long)]
        test_timeout: Option<u64>,
        /// Git remote the platform pushes main and task branches to; "" removes it.
        #[arg(long)]
        remote: Option<String>,
        /// Most tokens (input + output) the project may use; 0 removes the limit.
        #[arg(long)]
        budget_tokens: Option<u64>,
        /// Most dollars the project may cost (needs prices in contracts); 0 removes it.
        #[arg(long)]
        budget_usd: Option<f64>,
    },
}

#[derive(Debug, Subcommand)]
enum TaskAction {
    /// Details, last review, history, artifacts and runs.
    Show { task_id: String },

    /// Send a stopped task back to work with extra iterations and an instruction.
    Resume {
        task_id: String,
        #[arg(long)]
        note: Option<String>,
        #[arg(long, default_value_t = DEFAULT_RESUME_ITERATIONS)]
        iterations: u32,
    },

    /// Accept the last implementation of a stopped task (manual Owner approval).
    Accept {
        task_id: String,
        #[arg(long)]
        note: Option<String>,
    },

    /// Cancel a task; tasks that depend on it stay blocked.
    Cancel {
        task_id: String,
        #[arg(long)]
        note: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum HireAction {
    /// Create a proposal skeleton in <company>/proposals/<ID>.
    Propose {
        employee_id: String,
        #[arg(long)]
        name: String,
        #[arg(long, value_enum)]
        function: EmployeeFunction,
        #[arg(long)]
        manager: String,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        department: Option<String>,
    },

    /// Validate a proposal without hiring.
    Check { employee_id: String },

    /// Owner approval: validate and move the proposal into the registry.
    Approve { employee_id: String },
}

#[derive(Debug, Subcommand)]
enum EmployeeAction {
    /// Activate, suspend or disable an employee.
    SetStatus {
        employee_id: String,
        #[arg(value_enum)]
        status: EmployeeStatus,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let employees_dir = cli.company.join("employees");
    refuse_inside_workspace(&cli.data)?;

    match cli.command {
        Commands::Doctor { offline } => doctor(&employees_dir, offline).await,
        Commands::Team => print_team(&employees_dir),
        Commands::Run {
            project,
            task,
            json,
        } => run(&employees_dir, project, task, json).await,
        Commands::Console { project } => owner_console(&employees_dir, &cli.data, project).await,
        Commands::Dashboard { port } | Commands::Web { port } => {
            let writable = matches!(cli.command, Commands::Web { .. });
            ai_team::web::serve(
                ai_team::dashboard::Sources {
                    data_dir: cli.data.clone(),
                    employees_dir: employees_dir.clone(),
                    audit_dir: ai_team::audit::default_audit_dir(),
                },
                port,
                writable,
            )
            .await
        }
        Commands::Kpi { project } => {
            let store = Store::open(&cli.data)?;
            let report = ai_team::kpi::collect(&store, project.as_deref())?;
            println!(
                "KPI - {}\n",
                project
                    .as_deref()
                    .map_or("all projects".to_string(), |id| format!("project {id}"))
            );
            print!("{report}");
            Ok(())
        }
        Commands::Ask {
            project_id,
            question,
        } => {
            let store = Store::open(&cli.data)?;
            let orchestrator = Orchestrator::from_registry(Registry::load(&employees_dir)?)?;
            let answer = ProjectManager::new(&store, &orchestrator)
                .ask(&project_id, &question)
                .await?;
            println!("{answer}");
            Ok(())
        }
        Commands::Request {
            prompt,
            files,
            project,
            name,
            test_command,
            plan,
            yes,
        } => owner_request(
            &employees_dir,
            &cli.data,
            prompt,
            files,
            project,
            name,
            test_command,
            plan,
            yes,
        )
        .await
        .map(|_| ()),
        Commands::Project { action } => projects(&employees_dir, &cli.data, action).await,
        Commands::Task { action } => tasks(&employees_dir, &cli.data, action).await,
        Commands::Hire { action } => hire(&cli.company, action),
        Commands::Employees {
            action:
                EmployeeAction::SetStatus {
                    employee_id,
                    status,
                },
        } => {
            registry::set_status(&cli.company, &employee_id, status, &hr_audit_dir())?;
            println!("{employee_id}: status set to {status}");
            Ok(())
        }
    }
}

async fn doctor(employees_dir: &Path, offline: bool) -> Result<()> {
    let registry = Registry::load(employees_dir)?;

    println!("Registry:  {} (valid)", registry.root().display());
    println!("Employees: {}", registry.employees().count());

    let mut problems = 0;

    for issue in registry.readiness_issues() {
        println!("WARNING: {issue}; runs without assigned_team cannot be staffed");
    }

    for employee in registry.employees().filter(|employee| employee.is_active()) {
        let Some(model) = &employee.contract.model else {
            continue;
        };
        let env_name = match (&model.api_key_env, model.provider) {
            (Some(name), _) => Some(name.as_str()),
            (None, ProviderKind::Claude) => Some(CLAUDE_DEFAULT_API_KEY_ENV),
            (None, _) => None,
        };
        if let Some(env_name) = env_name
            && !std::env::var(env_name).is_ok_and(|key| !key.trim().is_empty())
        {
            problems += 1;
            println!(
                "ERROR: {} needs an API key: set {env_name} in .env",
                employee.id()
            );
        }
    }

    for builder in registry.active(EmployeeFunction::Builder) {
        for reviewer in registry.active(EmployeeFunction::Reviewer) {
            if let (Some(a), Some(b)) = (&builder.contract.model, &reviewer.contract.model)
                && a.same_model_as(b)
            {
                println!(
                    "NOTE: {} and {} share model {}; review independence relies on separate context",
                    builder.id(),
                    reviewer.id(),
                    a.model
                );
            }
        }
    }

    if !offline {
        problems += check_ollama_servers(&registry).await;
        problems += check_mcp_servers(&registry).await;
    }

    if problems > 0 {
        bail!("doctor found {problems} problem(s)");
    }
    println!("AI Team doctor: OK");
    Ok(())
}

/// Starts every MCP server an active employee uses and lists its tools.
async fn check_mcp_servers(registry: &Registry) -> usize {
    let mut used: Vec<String> = registry
        .employees()
        .filter(|employee| employee.is_active())
        .flat_map(|employee| employee.contract.mcp_servers.clone())
        .collect();
    used.sort();
    used.dedup();

    let mut problems = 0;
    for name in used {
        let Some(config) = registry.resources().mcp.servers.get(&name) else {
            continue; // the registry already rejects unknown servers
        };
        let toolbox = ai_team::mcp::Toolbox::new(
            vec![(name.clone(), config.clone())],
            registry.company_dir().to_path_buf(),
        );
        match toolbox.specs().await {
            Ok(tools) => println!(
                "MCP {name}: {} tool(s): {}",
                tools.len(),
                tools
                    .iter()
                    .map(|tool| tool.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Err(err) => {
                problems += 1;
                println!("ERROR: MCP server {name}: {err:#}");
            }
        }
    }
    problems
}

/// For every Ollama server used by an active employee: reachable, model installed, and
/// `num_ctx` within the model's maximum context. Returns the number of problems found.
async fn check_ollama_servers(registry: &Registry) -> usize {
    let mut servers: BTreeMap<String, Vec<(&str, &ModelConfig)>> = BTreeMap::new();
    for employee in registry.employees().filter(|employee| employee.is_active()) {
        if let Some(model) = &employee.contract.model
            && model.provider == ProviderKind::Ollama
            && let Some(base_url) = &model.base_url
        {
            servers
                .entry(base_url.trim_end_matches('/').to_string())
                .or_default()
                .push((employee.id(), model));
        }
    }

    let http = match reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
    {
        Ok(http) => http,
        Err(err) => {
            println!("ERROR: cannot build HTTP client: {err}");
            return 1;
        }
    };

    let mut problems = 0;
    for (base_url, employees) in servers {
        let installed: Vec<String> = match http.get(format!("{base_url}/api/tags")).send().await {
            Ok(response) if response.status().is_success() => response
                .json::<serde_json::Value>()
                .await
                .ok()
                .and_then(|tags| {
                    tags["models"].as_array().map(|models| {
                        models
                            .iter()
                            .filter_map(|model| model["name"].as_str().map(str::to_string))
                            .collect()
                    })
                })
                .unwrap_or_default(),
            Ok(response) => {
                problems += 1;
                println!("ERROR: Ollama {base_url} answered {}", response.status());
                continue;
            }
            Err(err) => {
                problems += 1;
                println!("ERROR: Ollama {base_url} unreachable: {err}");
                continue;
            }
        };
        println!(
            "Ollama {base_url}: reachable, {} model(s) installed",
            installed.len()
        );

        for (employee_id, model) in employees {
            if !installed.iter().any(|name| name == &model.model) {
                problems += 1;
                println!(
                    "ERROR: {employee_id}: model {} is not installed on {base_url}",
                    model.model
                );
                continue;
            }

            let max_context = http
                .post(format!("{base_url}/api/show"))
                .json(&serde_json::json!({ "model": model.model }))
                .send()
                .await
                .ok()
                .map(|response| response.json::<serde_json::Value>());
            let max_context = match max_context {
                Some(body) => body.await.ok().and_then(|show| {
                    show["model_info"].as_object().and_then(|info| {
                        info.iter()
                            .find(|(key, _)| key.ends_with(".context_length"))
                            .and_then(|(_, value)| value.as_u64())
                    })
                }),
                None => None,
            };

            match (model.num_ctx, max_context) {
                (Some(num_ctx), Some(max)) if u64::from(num_ctx) > max => {
                    problems += 1;
                    println!(
                        "ERROR: {employee_id}: num_ctx {num_ctx} exceeds the maximum context of {} ({max})",
                        model.model
                    );
                }
                (num_ctx, max) => println!(
                    "  {employee_id}: {} OK (num_ctx {}, model max {})",
                    model.model,
                    num_ctx.map_or("server default".to_string(), |n| n.to_string()),
                    max.map_or("unknown".to_string(), |n| n.to_string()),
                ),
            }
        }
    }
    problems
}

fn print_team(employees_dir: &Path) -> Result<()> {
    let registry = Registry::load(employees_dir)?;
    println!("{OWNER} (human)");
    print_reports(&registry, OWNER, "");
    Ok(())
}

fn print_reports(registry: &Registry, manager_id: &str, prefix: &str) {
    let reports = registry.reports_of(manager_id);
    for (index, employee) in reports.iter().enumerate() {
        let last = index + 1 == reports.len();
        let contract = &employee.contract;
        let model = match (&contract.employee_type, &contract.model) {
            (EmployeeType::System, _) => "control plane".to_string(),
            (_, Some(model)) => model.model.clone(),
            (_, None) => "-".to_string(),
        };
        let mut extras = Vec::new();
        if !contract.skill_packs.is_empty() {
            extras.push(format!("skills: {}", contract.skill_packs.join(", ")));
        }
        if !contract.mcp_servers.is_empty() {
            extras.push(format!("mcp: {}", contract.mcp_servers.join(", ")));
        }
        if employee.has(registry::Capability::VetoReview) {
            extras.push("VETO".to_string());
        }
        println!(
            "{prefix}{} {}  {} [{}]  {}  {}{}",
            if last { "└─" } else { "├─" },
            employee.id(),
            contract.name,
            contract.function,
            model,
            contract.status,
            if extras.is_empty() {
                String::new()
            } else {
                format!("  ({})", extras.join("; "))
            },
        );
        let child_prefix = format!("{prefix}{}", if last { "   " } else { "│  " });
        print_reports(registry, employee.id(), &child_prefix);
    }
}

fn hire(company: &Path, action: HireAction) -> Result<()> {
    match action {
        HireAction::Propose {
            employee_id,
            name,
            function,
            manager,
            title,
            department,
        } => {
            let dir = registry::propose_hire(
                company,
                &HireRequest {
                    employee_id,
                    name,
                    function,
                    manager_id: manager,
                    title,
                    department,
                },
            )?;
            println!("Proposal created: {}", dir.display());
            println!(
                "Fill in every TODO, then run `ai-team hire check <ID>` and `ai-team hire approve <ID>`."
            );
        }
        HireAction::Check { employee_id } => {
            let employee = registry::check_proposal(company, &employee_id)?;
            println!(
                "{employee_id}: proposal is valid ({} [{}], reports to {})",
                employee.contract.name, employee.contract.function, employee.contract.manager_id
            );
        }
        HireAction::Approve { employee_id } => {
            let employee = registry::approve_hire(company, &employee_id, &hr_audit_dir())?;
            println!(
                "{employee_id} hired: {} [{}], reports to {}",
                employee.contract.name, employee.contract.function, employee.contract.manager_id
            );
        }
    }
    Ok(())
}

async fn run(
    employees_dir: &Path,
    project_path: PathBuf,
    task: String,
    json_output: bool,
) -> Result<()> {
    let registry = Registry::load(employees_dir)?;
    let project = load_project(&project_path)?;
    let orchestrator = Orchestrator::from_registry(registry)?;

    let final_state = orchestrator
        .run(&project, task)
        .await
        .with_context(|| format!("team run failed for project {}", project.project_id))?;

    println!("\n=== AI TEAM RUN ===");
    println!("Project:    {}", project.name);
    println!("Run ID:     {}", final_state.run_id);
    println!("Task ID:    {}", final_state.task_id);
    if let Some(team) = &final_state.team {
        println!(
            "Team:       {} / {} / {}",
            team.architect, team.builder, team.reviewer
        );
    }
    println!("Stage:      {:?}", final_state.stage);
    println!("Iterations: {}", final_state.iteration);
    println!("Spec revs:  {}", final_state.architecture_revisions);

    println!("\n=== HISTORY ===");
    for item in &final_state.history {
        println!("- {item}");
    }

    if let Some(review) = &final_state.review {
        println!("\n=== FINAL REVIEW ===");
        println!("Decision: {:?}", review.decision);
        println!("Feedback: {}", review.feedback);
    }

    println!("\nAudit: {}", final_state.audit_file);

    if json_output {
        println!("\n=== FINAL STATE JSON ===");
        println!("{}", serde_json::to_string_pretty(&final_state)?);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Projects and tasks (phase 3)
// ---------------------------------------------------------------------------

/// Extra iterations granted by `task resume` when `--iterations` is not given.
const DEFAULT_RESUME_ITERATIONS: u32 = 2;

async fn projects(employees_dir: &Path, data_dir: &Path, action: ProjectAction) -> Result<()> {
    let store = Store::open(data_dir)?;
    let orchestrator = Orchestrator::from_registry(Registry::load(employees_dir)?)?;
    let manager = ProjectManager::new(&store, &orchestrator).with_progress(true);

    match action {
        ProjectAction::Add { file } => {
            let config = load_project(&file)?;
            store.add_project(&config)?;
            println!("Project {} added: {}", config.project_id, config.name);
            match &config.test_command {
                Some(command) => println!(
                    "Test command: `{command}` (timeout {} s)",
                    config.test_timeout_secs
                ),
                None => println!(
                    "No test command: work is reviewed without running tests. Set one with: \
                     ai-team project configure {} --test-command \"...\"",
                    config.project_id
                ),
            }
            println!("Next: ai-team project plan {}", config.project_id);
        }
        ProjectAction::List => {
            let projects = store.projects()?;
            if projects.is_empty() {
                println!("No projects. Add one with: ai-team project add <file.json>");
            }
            for project in projects {
                println!(
                    "{:<24} {:<12} {}",
                    project.config.project_id, project.status, project.config.name
                );
            }
        }
        ProjectAction::Plan { project_id, note } => {
            let plan = manager.plan(&project_id, note.as_deref()).await?;
            println!("Proposed plan for {project_id} (not approved yet):\n");
            for (index, milestone) in plan.milestones.iter().enumerate() {
                println!("Milestone {}: {}", index + 1, milestone.name);
                for task in &milestone.tasks {
                    let depends = if task.depends_on.is_empty() {
                        String::new()
                    } else {
                        format!("  (after {})", task.depends_on.join(", "))
                    };
                    println!("  {} {}{depends}", task.key, task.title);
                    for criterion in &task.acceptance_criteria {
                        println!("      - {criterion}");
                    }
                }
            }
            if !plan.rationale.is_empty() {
                println!("\nRationale: {}", plan.rationale);
            }
            println!("\nApprove with: ai-team project approve {project_id}");
            println!("Or ask again: ai-team project plan {project_id} --note \"...\"");
        }
        ProjectAction::Approve { project_id, note } => {
            let tasks = manager.approve(&project_id, note.as_deref())?;
            println!("Plan approved: {} task(s) created.", tasks.len());
            for task in tasks {
                println!("  {}  {}", task.id, task.title);
            }
            println!("Run with: ai-team project run {project_id}");
        }
        ProjectAction::Run {
            project_id,
            max_tasks,
        } => {
            let summary = manager.run(&project_id, max_tasks).await?;
            if summary.executed.is_empty() {
                println!("No task was ready to run.");
            }
            for (task_id, status) in &summary.executed {
                println!("{task_id}: {status}");
            }
            for report in &summary.reports {
                println!("Milestone report: {}", report.display());
            }
            for error in &summary.push_errors {
                println!("WARNING: {error}");
            }
            if let Some(reason) = &summary.budget_stop {
                println!(
                    "BUDGET: {reason}. Raise it with: ai-team project configure {project_id} --budget-tokens N"
                );
            }
            println!("Project {project_id}: {}", summary.project_status);
            println!("Usage so far: {}", manager.project_usage(&project_id)?);
            print_owner_actions(&store, &project_id)?;
        }
        ProjectAction::Delete { project_id, yes } => {
            store.project(&project_id)?;
            if !yes {
                use std::io::Write as _;
                print!(
                    "Delete project {project_id} permanently (records, statistics, workspace,                      reports, run audits)? Type the project id to confirm: "
                );
                std::io::stdout().flush()?;
                let mut answer = String::new();
                std::io::stdin().read_line(&mut answer)?;
                anyhow::ensure!(
                    answer.trim() == project_id,
                    "not confirmed; nothing deleted"
                );
            }
            let deleted = manager.delete_project(&project_id)?;
            println!(
                "{} deleted: workspace {}, {} audit file(s), {} report(s), {} artifact(s)",
                deleted.project_id,
                if deleted.workspace_removed {
                    "removed"
                } else {
                    "none"
                },
                deleted.audit_files,
                deleted.reports,
                deleted.artifacts
            );
        }
        ProjectAction::Configure {
            project_id,
            test_command,
            test_timeout,
            remote,
            budget_tokens,
            budget_usd,
        } => {
            let config = manager.configure(
                &project_id,
                ProjectSettings {
                    test_command,
                    test_timeout_secs: test_timeout,
                    remote_url: remote,
                    budget_tokens,
                    budget_usd,
                },
            )?;
            println!(
                "{project_id}: budget {}, used so far: {}",
                config.budget.clone().unwrap_or_default(),
                manager.project_usage(&project_id)?
            );
            println!(
                "{project_id}: remote {}",
                config
                    .remote_url
                    .as_deref()
                    .unwrap_or("none (commits stay local)")
            );
            println!(
                "{project_id}: test command {}, timeout {} s",
                config
                    .test_command
                    .as_deref()
                    .map(|command| format!("`{command}`"))
                    .unwrap_or_else(|| "none".to_string()),
                config.test_timeout_secs
            );
        }
        ProjectAction::Status { project_id } => {
            let project = store.project(&project_id)?;
            println!(
                "{} — {} [{}]",
                project.config.project_id, project.config.name, project.status
            );
            if project.status == ProjectStatus::Planning {
                let pending = store.pending_plan(&project_id)?.is_some();
                println!(
                    "{}",
                    if pending {
                        "A proposed plan is waiting for approval: ai-team project approve <id>"
                    } else {
                        "No plan yet: ai-team project plan <id>"
                    }
                );
                return Ok(());
            }
            let tasks = store.tasks(&project_id)?;
            for milestone in store.milestones(&project_id)? {
                println!("\nMilestone {}: {}", milestone.position + 1, milestone.name);
                for task in tasks
                    .iter()
                    .filter(|task| task.milestone_id == milestone.id)
                {
                    let depends = if task.depends_on.is_empty() {
                        String::new()
                    } else {
                        format!("  (after {})", task.depends_on.join(", "))
                    };
                    println!(
                        "  [{:<21}] {}  {}{depends}",
                        task.status.as_str(),
                        task.id,
                        task.title
                    );
                }
                if let Some(report) = &milestone.report_path {
                    println!("  Report: {report}");
                }
            }
            println!(
                "\nTest command: {}",
                project
                    .config
                    .test_command
                    .as_deref()
                    .map(|command| format!("`{command}`"))
                    .unwrap_or_else(|| "none".to_string())
            );
            println!(
                "Repository: {} (branch main = approved work)",
                manager.main_workspace(&project_id).display()
            );
            println!(
                "Remote: {}",
                project
                    .config
                    .remote_url
                    .as_deref()
                    .unwrap_or("none (commits stay local)")
            );
            println!("Usage: {}", manager.project_usage(&project_id)?);
            print_owner_actions(&store, &project_id)?;
        }
    }
    Ok(())
}

fn print_owner_actions(store: &Store, project_id: &str) -> Result<()> {
    let waiting: Vec<_> = store
        .tasks(project_id)?
        .into_iter()
        .filter(|task| task.status.needs_owner())
        .collect();
    if waiting.is_empty() {
        return Ok(());
    }
    println!("\nWaiting for an Owner decision:");
    for task in waiting {
        println!("  {} [{}] {}", task.id, task.status, task.title);
    }
    println!(
        "Decide with: ai-team task show <id> | task resume <id> --note \"...\" | task accept <id> | task cancel <id>"
    );
    Ok(())
}

async fn tasks(employees_dir: &Path, data_dir: &Path, action: TaskAction) -> Result<()> {
    let store = Store::open(data_dir)?;
    let orchestrator = Orchestrator::from_registry(Registry::load(employees_dir)?)?;
    let manager = ProjectManager::new(&store, &orchestrator);

    match action {
        TaskAction::Show { task_id } => {
            let task = store.task(&task_id)?;
            println!("{} — {} [{}]", task.id, task.title, task.status);
            println!("\n{}\n", task.description);
            println!("Acceptance criteria:");
            for criterion in &task.acceptance_criteria {
                println!("  - {criterion}");
            }
            if !task.depends_on.is_empty() {
                println!("Depends on: {}", task.depends_on.join(", "));
            }
            println!("Iteration budget: {}", task.iteration_budget);
            if let Some(error) = &task.error {
                println!("Last error: {error}");
            }
            if let Some(state) = store.load_task_state(&task_id)? {
                if let Some(team) = &state.team {
                    println!(
                        "Team: {} / {} / {}",
                        team.architect, team.builder, team.reviewer
                    );
                }
                println!("Iterations used: {}", state.iteration);
                println!("Usage: {}", state.usage);
                if !state.written_files.is_empty() {
                    println!("Files written: {}", state.written_files.join(", "));
                }
                if let Some(verification) = &state.verification {
                    println!("Last verification:\n{}", verification.report().trim_end());
                }
                if let Some(review) = &state.review {
                    println!("Last review: {:?} — {}", review.decision, review.feedback);
                }
                println!("Recent history:");
                for item in state.history.iter().rev().take(10).rev() {
                    println!("  - {item}");
                }
            }
            for (label, hash) in [
                ("Specification", &task.spec_hash),
                ("Implementation", &task.impl_hash),
            ] {
                if let Some(hash) = hash {
                    println!("{label}: {}", store.artifact_path(hash).display());
                }
            }
            for (run_id, outcome, audit) in store.runs_of(&task_id)? {
                println!("Run {run_id}: {outcome} (audit: {audit})");
            }
        }
        TaskAction::Resume {
            task_id,
            note,
            iterations,
        } => {
            let task = manager.resume_task(&task_id, note.as_deref(), iterations)?;
            println!(
                "{} sent back to work (iteration budget now {}).",
                task.id, task.iteration_budget
            );
            println!("Continue with: ai-team project run {}", task.project_id);
        }
        TaskAction::Accept { task_id, note } => {
            let task = manager.accept_task(&task_id, note.as_deref())?;
            println!("{} accepted by the Owner: {}", task.id, task.status);
            println!("Continue with: ai-team project run {}", task.project_id);
        }
        TaskAction::Cancel { task_id, note } => {
            let task = manager.cancel_task(&task_id, note.as_deref())?;
            println!(
                "{} cancelled; tasks that depend on it stay blocked.",
                task.id
            );
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Owner request: a prompt plus files, straight to the team
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn owner_request(
    employees_dir: &Path,
    data_dir: &Path,
    prompt: String,
    files: Vec<PathBuf>,
    project_id: Option<String>,
    new_project_name: Option<String>,
    test_command: Option<String>,
    plan: bool,
    yes: bool,
) -> Result<String> {
    let store = Store::open(data_dir)?;
    let orchestrator = Orchestrator::from_registry(Registry::load(employees_dir)?)?;
    let manager = ProjectManager::new(&store, &orchestrator).with_progress(true);
    if project_id.is_none() && test_command.is_none() {
        println!("{NO_TESTS_WARNING}");
    }
    anyhow::ensure!(
        !plan || project_id.is_none(),
        "--plan only works for a new project"
    );
    let request = owner::NewRequest {
        prompt: prompt.clone(),
        files,
        project_id,
        new_project_name,
        test_command,
    };
    let prepared = owner::prepare(&store, &manager, &request)?;
    let project_id = prepared.project_id.clone();
    if prepared.created {
        println!("Project {project_id} created.");
    }
    if !prepared.inputs.is_empty() {
        println!(
            "Files committed to the project: {}",
            prepared.inputs.join(", ")
        );
    }

    if plan {
        let proposed = manager.plan(&project_id, Some(prompt.trim())).await?;
        println!("\nProposed plan:");
        for (index, milestone) in proposed.milestones.iter().enumerate() {
            println!("Milestone {}: {}", index + 1, milestone.name);
            for task in &milestone.tasks {
                println!("  {} {}", task.key, task.title);
            }
        }
        if !yes {
            print!("\nApprove this plan and start the team? [y/N] ");
            use std::io::Write as _;
            std::io::stdout().flush()?;
            let mut answer = String::new();
            std::io::stdin().read_line(&mut answer)?;
            if !answer.trim().eq_ignore_ascii_case("y") {
                println!("Not approved. Approve later with: ai-team project approve {project_id}");
                return Ok(project_id);
            }
        }
        manager.approve(&project_id, Some("approved with the request"))?;
    } else {
        let task = owner::add_request_task(&store, &manager, &request, &prepared)?;
        println!("Task {} created: {}", task.id, task.title);
    }

    println!("The team is working...\n");
    let summary = manager.run(&project_id, None).await?;
    for (task_id, status) in &summary.executed {
        let task = store.task(task_id)?;
        println!("{task_id} [{status}] {}", task.title);
        if let Some(state) = store.load_task_state(task_id)? {
            if let Some(review) = &state.review {
                println!(
                    "  last review: {:?} - {}",
                    review.decision,
                    review
                        .feedback
                        .lines()
                        .find(|line| !line.trim().is_empty())
                        .unwrap_or("")
                );
            }
            if let Some(verification) = &state.verification
                && let Some(test) = &verification.test
            {
                println!("  tests: {}", if test.passed { "passed" } else { "FAILED" });
            }
            if let Some(error) = &state.error {
                println!("  error: {error}");
            }
        }
    }
    for error in &summary.push_errors {
        println!("WARNING: {error}");
    }
    if let Some(reason) = &summary.budget_stop {
        println!(
            "BUDGET: {reason}. Raise it with: ai-team project configure <id> --budget-tokens N"
        );
    }

    let repository = manager.main_workspace(&project_id);
    let produced: Vec<String> = ai_team::workspace::Workspace::open(&repository)?
        .files()?
        .into_iter()
        .filter(|file| file != ".gitignore" && !file.starts_with("inputs/"))
        .collect();
    println!("\nProject {project_id}: {}", summary.project_status);
    println!("Result (branch main): {}", repository.display());
    if !produced.is_empty() {
        println!("Files produced: {}", produced.join(", "));
    }
    println!("Usage: {}", manager.project_usage(&project_id)?);
    print_owner_actions(&store, &project_id)?;
    Ok(project_id)
}

// ---------------------------------------------------------------------------
// Owner console: type a request, attach files, watch the team, decide
// ---------------------------------------------------------------------------

const CONSOLE_HELP: &str = "\
Scrie cererea (poate avea mai multe randuri); un rand gol o trimite.
O cerere porneste echipa si poate schimba codul. Pentru intrebari foloseste '?'.
Comenzi:
  ?<intrebare>    raspuns despre proiectul curent, fara sa se schimbe nimic
  /nou [nume]     urmatoarea cerere porneste un proiect nou (cu numele dat, ex. /nou ulise)
  /proiect <id>   cererile urmatoare merg in proiectul <id>
  /status         starea proiectului curent
  /ajutor         acest mesaj
  /iesire         inchide consola";

fn read_line(prompt: &str) -> Result<Option<String>> {
    use std::io::Write as _;
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line)? == 0 {
        return Ok(None); // end of input
    }
    Ok(Some(line.trim_end_matches(['\r', '\n']).to_string()))
}

/// Suggested in the console for a new project; the shipped team writes Python.
const DEFAULT_TEST_COMMAND: &str = "python -m unittest discover -s tests -v";
const NO_TESTS_WARNING: &str = "ATENTIE: fara comanda de test platforma nu ruleaza nimic; Reviewer-ul \
aproba doar citind codul, iar o regresie poate trece. Seteaz-o oricand cu: \
ai-team project configure <id> --test-command \"...\"";

/// Running the CLI from inside a project's repository would create a stray `data/` there,
/// which the platform then commits as leftover work. Refuse, and say where to run from.
fn refuse_inside_workspace(data_dir: &Path) -> Result<()> {
    if data_dir.is_absolute() {
        return Ok(());
    }
    let cwd = std::env::current_dir()?;
    for dir in cwd.ancestors() {
        let Some(workspaces) = dir.parent() else {
            break;
        };
        if workspaces
            .file_name()
            .is_some_and(|name| name == "workspaces")
            && dir.join(".git").exists()
            && let Some(data) = workspaces.parent()
            && data.join("ai-team.db").is_file()
        {
            let root = data.parent().unwrap_or(data);
            anyhow::bail!(
                "you are inside the repository of project {} ({}); running ai-team here would \
                 create a stray data/ folder in it. Run it from {} instead.",
                dir.file_name().unwrap_or_default().to_string_lossy(),
                cwd.display(),
                root.display()
            );
        }
    }
    Ok(())
}

/// Answers a question about the current project in the console (read-only).
async fn console_ask(manager: &ProjectManager<'_>, project: Option<&str>, question: &str) {
    let Some(project_id) = project else {
        println!("Intrebarile au nevoie de un proiect: /proiect <id>.");
        return;
    };
    println!("(echipa citeste proiectul, nu schimba nimic...)");
    match manager.ask(project_id, question).await {
        Ok(answer) => println!("\n{}", answer.trim()),
        Err(err) => println!("Eroare: {err:#}"),
    }
}

/// A path typed or dragged into the console (Windows wraps it in quotes).
fn clean_path(raw: &str) -> PathBuf {
    PathBuf::from(raw.trim().trim_matches('"').trim_matches('\'').trim())
}

async fn owner_console(
    employees_dir: &Path,
    data_dir: &Path,
    project: Option<String>,
) -> Result<()> {
    let store = Store::open(data_dir)?;
    let orchestrator = Orchestrator::from_registry(Registry::load(employees_dir)?)?;
    let manager = ProjectManager::new(&store, &orchestrator).with_progress(true);
    let mut new_name: Option<String> = None;
    let mut current = match project {
        Some(id) => {
            store.project(&id)?;
            Some(id)
        }
        None => None,
    };

    println!("AI Team - consola Owner-ului\n{CONSOLE_HELP}");
    loop {
        println!(
            "\n=== {} ===",
            match (&current, &new_name) {
                (Some(id), _) => format!("proiect {id}"),
                (None, Some(name)) => format!("proiect nou: {name} (se creeaza la prima cerere)"),
                (None, None) => "proiect nou".to_string(),
            }
        );

        // The request (several lines until an empty one) or a command.
        let mut lines: Vec<String> = Vec::new();
        loop {
            let Some(line) = read_line(if lines.is_empty() { "> " } else { "... " })? else {
                return Ok(());
            };
            if lines.is_empty() && (line.trim().starts_with('/') || line.trim().starts_with('?')) {
                lines.push(line);
                break;
            }
            if line.trim().is_empty() {
                if lines.is_empty() {
                    continue;
                }
                break;
            }
            if lines.is_empty() {
                println!(
                    "    (continua cererea pe randul urmator sau apasa Enter pe rand gol ca s-o trimiti)"
                );
            }
            lines.push(line);
        }
        let first = lines[0].trim().to_string();
        if let Some(question) = first.strip_prefix('?') {
            console_ask(&manager, current.as_deref(), question).await;
            continue;
        }
        if let Some(command) = first.strip_prefix('/') {
            let mut parts = command.split_whitespace();
            match (parts.next().unwrap_or(""), parts.next()) {
                ("iesire" | "exit" | "quit", _) => return Ok(()),
                ("ajutor" | "help", _) => println!("{CONSOLE_HELP}"),
                ("nou" | "new", name) => {
                    current = None;
                    new_name = match name.map(normalize_project_name).transpose() {
                        Ok(name) => name,
                        Err(err) => {
                            println!("{err:#}");
                            None
                        }
                    };
                    match &new_name {
                        Some(name) if store.project(name).is_ok() => {
                            println!("Proiectul {name} exista deja; continui-l cu /proiect {name}");
                            new_name = None;
                        }
                        Some(name) => println!(
                            "Proiectul {name} se creeaza la prima cerere. Scrie ce trebuie sa faca echipa."
                        ),
                        None => println!("Urmatoarea cerere porneste un proiect nou."),
                    }
                }
                ("proiect" | "project", Some(id)) => match store.project(id) {
                    Ok(_) => current = Some(id.to_string()),
                    Err(err) => println!("{err:#}"),
                },
                ("status", _) => match &current {
                    Some(id) => {
                        let project_id = id.clone();
                        if let Err(err) = projects(
                            employees_dir,
                            data_dir,
                            ProjectAction::Status { project_id },
                        )
                        .await
                        {
                            println!("{err:#}");
                        }
                    }
                    None => println!("Niciun proiect curent."),
                },
                _ => println!("Comanda necunoscuta. /ajutor pentru lista."),
            }
            continue;
        }
        let prompt = lines.join("\n");

        // A question typed as a request would become a task that rewrites code.
        if current.is_some() && prompt.trim_end().ends_with('?') {
            let choice = read_line(
                "Pare o intrebare. [Enter] raspund fara sa schimb codul  [t] porneste echipa (task nou): ",
            )?
            .unwrap_or_default();
            if !choice.trim().eq_ignore_ascii_case("t") {
                console_ask(&manager, current.as_deref(), &prompt).await;
                continue;
            }
        }

        // Files: paths typed or dragged into the console, one per line.
        let mut files = Vec::new();
        println!("Fisiere pentru echipa (cale sau trage fisierul aici; Enter gol = gata):");
        loop {
            let Some(line) = read_line("  fisier> ")? else {
                return Ok(());
            };
            if line.trim().is_empty() {
                break;
            }
            let path = clean_path(&line);
            if path.is_file() {
                println!("  + {}", path.display());
                files.push(path);
            } else {
                println!("  nu exista fisierul: {}", path.display());
            }
        }

        let test_command = if current.is_none() {
            let answer = read_line(&format!(
                "Comanda de test [{DEFAULT_TEST_COMMAND}] (Enter = aceasta, '-' = fara teste): "
            ))?
            .unwrap_or_default();
            match answer.trim() {
                "" => Some(DEFAULT_TEST_COMMAND.to_string()),
                "-" => None,
                command => Some(command.to_string()),
            }
        } else {
            None
        };

        println!("Cererea ta:\n  {}", prompt.replace('\n', "\n  "));
        let answer = read_line("Pornesc echipa? [D/n] ")?.unwrap_or_default();
        if answer.trim().eq_ignore_ascii_case("n") {
            println!("Anulat.");
            continue;
        }

        match owner_request(
            employees_dir,
            data_dir,
            prompt,
            files,
            current.clone(),
            new_name.clone(),
            test_command,
            false,
            true,
        )
        .await
        {
            Ok(project_id) => {
                current = Some(project_id);
                new_name = None;
            }
            Err(err) => {
                println!("Eroare: {err:#}");
                continue;
            }
        }

        // Stopped tasks wait for the Owner: decide right here.
        let project_id = current.clone().expect("set above");
        loop {
            let waiting: Vec<_> = store
                .tasks(&project_id)?
                .into_iter()
                .filter(|task| task.status.needs_owner())
                .collect();
            let Some(task) = waiting.first() else {
                break;
            };
            println!("\n{} s-a oprit [{}]: {}", task.id, task.status, task.title);
            if let Some(state) = store.load_task_state(&task.id)?
                && let Some(review) = &state.review
            {
                let summary: String = review
                    .feedback
                    .lines()
                    .take(6)
                    .collect::<Vec<_>>()
                    .join("\n  ");
                println!("  ultimul feedback:\n  {summary}");
            }
            let choice =
                read_line("  [r] reia cu o nota  [a] accepta  [c] anuleaza  [Enter] lasa asa: ")?
                    .unwrap_or_default();
            match choice.trim().to_lowercase().as_str() {
                "r" => {
                    let note = read_line("  nota pentru echipa: ")?.unwrap_or_default();
                    let note = (!note.trim().is_empty()).then_some(note);
                    match manager.resume_task(&task.id, note.as_deref(), DEFAULT_RESUME_ITERATIONS)
                    {
                        Ok(_) => {
                            println!("Echipa reia lucrul...");
                            let summary = manager.run(&project_id, None).await?;
                            for (task_id, status) in &summary.executed {
                                println!("{task_id}: {status}");
                            }
                        }
                        Err(err) => println!("  {err:#}"),
                    }
                }
                "a" => match manager.accept_task(&task.id, Some("accepted from the console")) {
                    Ok(task) => println!("  {} acceptat: {}", task.id, task.status),
                    Err(err) => println!("  {err:#}"),
                },
                "c" => match manager.cancel_task(&task.id, Some("cancelled from the console")) {
                    Ok(task) => println!("  {} anulat", task.id),
                    Err(err) => println!("  {err:#}"),
                },
                _ => break,
            }
        }
    }
}
