use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};

use ai_team::{
    audit::hr_audit_dir,
    config::{CLAUDE_DEFAULT_API_KEY_ENV, ModelConfig, ProviderKind, load_project},
    orchestrator::Orchestrator,
    project::{ProjectManager, ProjectStatus, Store},
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

    match cli.command {
        Commands::Doctor { offline } => doctor(&employees_dir, offline).await,
        Commands::Team => print_team(&employees_dir),
        Commands::Run {
            project,
            task,
            json,
        } => run(&employees_dir, project, task, json).await,
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
            && std::env::var(env_name).is_err()
        {
            problems += 1;
            println!(
                "ERROR: {} expects environment variable {env_name}",
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
    }

    if problems > 0 {
        bail!("doctor found {problems} problem(s)");
    }
    println!("AI Team doctor: OK");
    Ok(())
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
        println!(
            "{prefix}{} {}  {} [{}]  {}  {}",
            if last { "└─" } else { "├─" },
            employee.id(),
            contract.name,
            contract.function,
            model,
            contract.status,
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
    let manager = ProjectManager::new(&store, &orchestrator);

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
            println!("Project {project_id}: {}", summary.project_status);
            println!("Usage so far: {}", manager.project_usage(&project_id)?);
            print_owner_actions(&store, &project_id)?;
        }
        ProjectAction::Configure {
            project_id,
            test_command,
            test_timeout,
            remote,
        } => {
            let config = manager.configure(&project_id, test_command, test_timeout, remote)?;
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
