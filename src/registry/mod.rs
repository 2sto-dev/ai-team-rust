//! Employee registry: one folder per AI employee under `company/employees/`, holding the
//! operational contract (`contract.yaml`) and the job description (`job_description.md`).
//! Adding an employee is a file change, never a recompile.

mod hire;

use std::{
    collections::BTreeMap,
    fmt, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::{
    config::ModelConfig,
    mcp::{McpCatalog, Toolbox},
};

pub use hire::{HireRequest, approve_hire, check_proposal, propose_hire, set_model, set_status};

/// The human at the top of the org chart; the orchestrator reports to it.
pub const OWNER: &str = "OWNER";
pub const CONTRACT_FILE: &str = "contract.yaml";
pub const JOB_DESCRIPTION_FILE: &str = "job_description.md";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmployeeType {
    /// Platform code (the orchestrator's control plane); no model.
    System,
    /// Department lead, reports to the orchestrator.
    Agent,
    /// Specialist, reports to an agent.
    Subagent,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, clap::ValueEnum,
)]
#[serde(rename_all = "snake_case")]
pub enum EmployeeFunction {
    Orchestrator,
    Planner,
    Architect,
    Builder,
    Reviewer,
    Specialist,
}

impl EmployeeFunction {
    /// The capability an employee needs to be given work in this function.
    pub fn required_capability(self) -> Option<Capability> {
        match self {
            EmployeeFunction::Planner => Some(Capability::ProposePlan),
            EmployeeFunction::Architect => Some(Capability::WriteSpecification),
            EmployeeFunction::Builder => Some(Capability::WriteImplementation),
            EmployeeFunction::Reviewer => Some(Capability::ReviewWork),
            EmployeeFunction::Orchestrator | EmployeeFunction::Specialist => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            EmployeeFunction::Orchestrator => "orchestrator",
            EmployeeFunction::Planner => "planner",
            EmployeeFunction::Architect => "architect",
            EmployeeFunction::Builder => "builder",
            EmployeeFunction::Reviewer => "reviewer",
            EmployeeFunction::Specialist => "specialist",
        }
    }
}

impl fmt::Display for EmployeeFunction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum EmployeeStatus {
    Active,
    Suspended,
    Disabled,
}

impl EmployeeStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            EmployeeStatus::Active => "active",
            EmployeeStatus::Suspended => "suspended",
            EmployeeStatus::Disabled => "disabled",
        }
    }
}

impl fmt::Display for EmployeeStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An action the platform lets an employee perform. Only capabilities the control plane
/// actually enforces exist here; workspace and tool capabilities arrive with real execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    WriteSpecification,
    WriteImplementation,
    ReviewWork,
    ProposePlan,
    DelegateSubtasks,
    /// The platform may write this employee's answers into a task workspace.
    WriteWorkspace,
    /// The platform may run the project's test command on this employee's work.
    RunTests,
    /// A reviewer's subagent whose rejection the Reviewer cannot overrule (security).
    VetoReview,
}

impl Capability {
    pub fn as_str(self) -> &'static str {
        match self {
            Capability::WriteSpecification => "write_specification",
            Capability::WriteImplementation => "write_implementation",
            Capability::ReviewWork => "review_work",
            Capability::ProposePlan => "propose_plan",
            Capability::DelegateSubtasks => "delegate_subtasks",
            Capability::WriteWorkspace => "write_workspace",
            Capability::RunTests => "run_tests",
            Capability::VetoReview => "veto_review",
        }
    }
}

impl fmt::Display for Capability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmployeeContract {
    pub employee_id: String,
    pub name: String,
    pub title: String,
    pub department: String,
    pub manager_id: String,
    pub employee_type: EmployeeType,
    pub function: EmployeeFunction,
    pub status: EmployeeStatus,
    #[serde(default)]
    pub model: Option<ModelConfig>,
    #[serde(default)]
    pub skills: Vec<String>,
    /// Skill packs (`company/skills/<id>/SKILL.md`) appended to the system prompt.
    #[serde(default)]
    pub skill_packs: Vec<String>,
    /// MCP servers (`company/mcp.yaml`) whose tools this employee may call.
    #[serde(default)]
    pub mcp_servers: Vec<String>,
    #[serde(default)]
    pub permissions: Vec<Capability>,
    /// Informative only: anything not listed in `permissions` is already forbidden.
    #[serde(default)]
    pub forbidden_actions: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Employee {
    pub contract: EmployeeContract,
    /// Used verbatim as the employee's system prompt.
    pub job_description: String,
    pub dir: PathBuf,
}

impl Employee {
    pub fn id(&self) -> &str {
        &self.contract.employee_id
    }

    pub fn is_active(&self) -> bool {
        self.contract.status == EmployeeStatus::Active
    }

    pub fn has(&self, capability: Capability) -> bool {
        self.contract.permissions.contains(&capability)
    }
}

/// A package of instructions in `company/skills/<id>/SKILL.md` (optional YAML front matter
/// with `name` and `description`, then Markdown).
#[derive(Debug, Clone)]
pub struct SkillPack {
    pub id: String,
    pub name: String,
    pub description: String,
    pub body: String,
}

/// Company-wide resources contracts refer to.
#[derive(Debug, Clone, Default)]
pub struct Resources {
    pub skill_packs: BTreeMap<String, SkillPack>,
    pub mcp: McpCatalog,
}

/// Loads `skills/` and `mcp.yaml` from the company folder (both optional).
pub(crate) fn load_resources(company_dir: &Path) -> Result<Resources> {
    let mut resources = Resources::default();
    let mut errors = Vec::new();

    let skills_dir = company_dir.join("skills");
    if skills_dir.is_dir() {
        let mut dirs: Vec<PathBuf> = fs::read_dir(&skills_dir)?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.is_dir())
            .collect();
        dirs.sort();
        for dir in dirs {
            let id = dir
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            if id.is_empty()
                || !id
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            {
                errors.push(format!(
                    "skill folder '{id}' must use lowercase letters, digits and '-'"
                ));
                continue;
            }
            match fs::read_to_string(dir.join("SKILL.md")) {
                Ok(text) => {
                    let pack = parse_skill(&id, text.trim_start_matches('\u{feff}'));
                    if pack.body.trim().is_empty() {
                        errors.push(format!("skill {id}: SKILL.md has no instructions"));
                    }
                    resources.skill_packs.insert(id, pack);
                }
                Err(err) => errors.push(format!("skill {id}: cannot read SKILL.md: {err}")),
            }
        }
    }

    let mcp_file = company_dir.join("mcp.yaml");
    if mcp_file.is_file() {
        let raw = fs::read_to_string(&mcp_file)?;
        match serde_yaml_ng::from_str::<McpCatalog>(raw.trim_start_matches('\u{feff}')) {
            Ok(catalog) => {
                errors.extend(catalog.validate());
                resources.mcp = catalog;
            }
            Err(err) => errors.push(format!("invalid mcp.yaml: {err}")),
        }
    }

    if !errors.is_empty() {
        bail!(
            "invalid company resources in {}:\n- {}",
            company_dir.display(),
            errors.join("\n- ")
        );
    }
    Ok(resources)
}

fn parse_skill(id: &str, text: &str) -> SkillPack {
    let (front, body) = match text.strip_prefix("---") {
        Some(rest) => match rest.split_once("\n---") {
            Some((front, body)) => (front, body.trim_start_matches(['\r', '\n', '-'])),
            None => ("", text),
        },
        None => ("", text),
    };
    let meta: serde_yaml_ng::Value = serde_yaml_ng::from_str(front).unwrap_or_default();
    let field = |key: &str| meta.get(key).and_then(|v| v.as_str()).map(str::to_string);
    SkillPack {
        id: id.to_string(),
        name: field("name").unwrap_or_else(|| id.to_string()),
        description: field("description").unwrap_or_default(),
        body: body.trim().to_string(),
    }
}

pub struct Registry {
    root: PathBuf,
    company_dir: PathBuf,
    employees: BTreeMap<String, Employee>,
    resources: Resources,
}

impl Registry {
    /// Loads and validates every employee folder; all problems are reported at once.
    pub fn load(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        let company_dir = root.parent().unwrap_or(root).to_path_buf();
        let resources = load_resources(&company_dir)?;
        let employees = load_dir(root)?;

        let errors = validate(&employees, &resources);
        if !errors.is_empty() {
            bail!(
                "invalid employee registry {}:\n- {}",
                root.display(),
                errors.join("\n- ")
            );
        }

        Ok(Self {
            root: root.to_path_buf(),
            company_dir,
            employees,
            resources,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn company_dir(&self) -> &Path {
        &self.company_dir
    }

    pub fn resources(&self) -> &Resources {
        &self.resources
    }

    /// The job description plus every skill pack of the contract.
    pub fn system_prompt(&self, employee: &Employee) -> String {
        let mut prompt = employee.job_description.clone();
        let packs: Vec<&SkillPack> = employee
            .contract
            .skill_packs
            .iter()
            .filter_map(|id| self.resources.skill_packs.get(id))
            .collect();
        if !packs.is_empty() {
            prompt.push_str("\n\n# SKILLS\n");
            for pack in packs {
                prompt.push_str(&format!("\n## {}\n\n{}\n", pack.name, pack.body));
            }
        }
        prompt
    }

    /// The MCP tools this employee may use, if any (servers start on first use).
    pub fn toolbox_for(&self, employee: &Employee) -> Option<std::sync::Arc<Toolbox>> {
        if employee.contract.mcp_servers.is_empty() {
            return None;
        }
        let servers = employee
            .contract
            .mcp_servers
            .iter()
            .filter_map(|name| {
                self.resources
                    .mcp
                    .servers
                    .get(name)
                    .map(|config| (name.clone(), config.clone()))
            })
            .collect();
        Some(std::sync::Arc::new(Toolbox::new(
            servers,
            self.company_dir.clone(),
        )))
    }

    /// Active subagents reporting to `manager_id`.
    pub fn active_subagents_of(&self, manager_id: &str) -> Vec<&Employee> {
        self.reports_of(manager_id)
            .into_iter()
            .filter(|employee| {
                employee.is_active() && employee.contract.employee_type == EmployeeType::Subagent
            })
            .collect()
    }

    pub fn get(&self, employee_id: &str) -> Option<&Employee> {
        self.employees.get(employee_id)
    }

    /// All employees, ordered by `employee_id`.
    pub fn employees(&self) -> impl Iterator<Item = &Employee> {
        self.employees.values()
    }

    pub fn active(&self, function: EmployeeFunction) -> Vec<&Employee> {
        self.employees()
            .filter(|employee| employee.is_active() && employee.contract.function == function)
            .collect()
    }

    pub fn reports_of(&self, manager_id: &str) -> Vec<&Employee> {
        self.employees()
            .filter(|employee| employee.contract.manager_id == manager_id)
            .collect()
    }

    /// Valid registry, but some run would be impossible (e.g. no active builder).
    pub fn readiness_issues(&self) -> Vec<String> {
        [
            EmployeeFunction::Planner,
            EmployeeFunction::Architect,
            EmployeeFunction::Builder,
            EmployeeFunction::Reviewer,
        ]
        .into_iter()
        .filter(|function| self.active(*function).is_empty())
        .map(|function| format!("no active {function}"))
        .collect()
    }
}

pub(crate) fn load_dir(root: &Path) -> Result<BTreeMap<String, Employee>> {
    let entries = fs::read_dir(root)
        .with_context(|| format!("cannot read employee registry: {}", root.display()))?;

    let mut dirs: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();

    let mut employees = BTreeMap::new();
    let mut errors = Vec::new();
    for dir in dirs {
        match load_employee(&dir) {
            Ok(employee) => {
                employees.insert(employee.id().to_string(), employee);
            }
            Err(err) => errors.push(format!("{}: {err:#}", dir.display())),
        }
    }

    if !errors.is_empty() {
        bail!(
            "cannot load employee registry {}:\n- {}",
            root.display(),
            errors.join("\n- ")
        );
    }
    Ok(employees)
}

pub(crate) fn load_employee(dir: &Path) -> Result<Employee> {
    let raw = fs::read_to_string(dir.join(CONTRACT_FILE))
        .with_context(|| format!("cannot read {CONTRACT_FILE}"))?;
    // Windows editors (Notepad, PowerShell 5.1) may prepend a UTF-8 BOM.
    let contract: EmployeeContract = serde_yaml_ng::from_str(raw.trim_start_matches('\u{feff}'))
        .with_context(|| format!("invalid {CONTRACT_FILE}"))?;
    let job_description = fs::read_to_string(dir.join(JOB_DESCRIPTION_FILE))
        .with_context(|| format!("cannot read {JOB_DESCRIPTION_FILE}"))?
        .trim_start_matches('\u{feff}')
        .to_string();

    let folder = dir.file_name().and_then(|name| name.to_str()).unwrap_or("");
    anyhow::ensure!(
        folder == contract.employee_id,
        "folder name '{folder}' must match employee_id '{}'",
        contract.employee_id
    );

    Ok(Employee {
        contract,
        job_description,
        dir: dir.to_path_buf(),
    })
}

/// Structural rules of the company. Returns every violation found.
pub(crate) fn validate(
    employees: &BTreeMap<String, Employee>,
    resources: &Resources,
) -> Vec<String> {
    use EmployeeFunction as F;
    use EmployeeType as T;

    let mut errors = Vec::new();

    for employee in employees.values() {
        let contract = &employee.contract;
        let mut fail = |message: String| {
            errors.push(format!("{}: {message}", contract.employee_id));
        };

        if !contract.employee_id.starts_with("EMP-")
            || contract.employee_id.contains(char::is_whitespace)
        {
            fail("employee_id must start with 'EMP-' and contain no spaces".to_string());
        }
        if contract.name.trim().is_empty() {
            fail("name cannot be empty".to_string());
        }
        if employee.job_description.trim().is_empty() {
            fail(format!("{JOB_DESCRIPTION_FILE} is empty"));
        }

        let type_matches_function = match contract.employee_type {
            T::System => contract.function == F::Orchestrator,
            T::Agent => matches!(
                contract.function,
                F::Planner | F::Architect | F::Builder | F::Reviewer
            ),
            T::Subagent => contract.function == F::Specialist,
        };
        if !type_matches_function {
            fail(format!(
                "employee_type {:?} cannot have function '{}'",
                contract.employee_type, contract.function
            ));
        }

        match (contract.employee_type, &contract.model) {
            (T::System, Some(_)) => fail("system employees have no model".to_string()),
            (T::System, None) => {}
            (_, None) => fail("model is required".to_string()),
            (_, Some(model)) => {
                if let Err(err) = model.validate() {
                    fail(format!("model: {err:#}"));
                }
            }
        }

        match contract.employee_type {
            T::System => {
                if contract.manager_id != OWNER {
                    fail(format!("the orchestrator reports to {OWNER}"));
                }
                if !employee.is_active() {
                    fail("the orchestrator must be active".to_string());
                }
                if !contract.permissions.is_empty() {
                    fail("system employees hold no permissions".to_string());
                }
            }
            T::Agent => match employees.get(&contract.manager_id) {
                Some(manager) if manager.contract.employee_type == T::System => {}
                Some(_) => fail("agents report to the orchestrator".to_string()),
                None => fail(format!("unknown manager_id '{}'", contract.manager_id)),
            },
            T::Subagent => match employees.get(&contract.manager_id) {
                Some(manager) if manager.contract.employee_type == T::Agent => {
                    if !manager.has(Capability::DelegateSubtasks) {
                        fail(format!(
                            "manager {} lacks delegate_subtasks",
                            contract.manager_id
                        ));
                    }
                    for permission in &contract.permissions {
                        // A veto belongs to a reviewer's specialist, not to the reviewer.
                        if *permission != Capability::VetoReview && !manager.has(*permission) {
                            fail(format!(
                                "permission {permission} exceeds manager {}'s permissions",
                                contract.manager_id
                            ));
                        }
                    }
                }
                Some(_) => fail("subagents report to an agent".to_string()),
                None => fail(format!("unknown manager_id '{}'", contract.manager_id)),
            },
        }

        if let Some(required) = contract.function.required_capability()
            && !employee.has(required)
        {
            fail(format!(
                "function '{}' requires permission {required}",
                contract.function
            ));
        }
        if employee.has(Capability::WriteImplementation) && employee.has(Capability::ReviewWork) {
            fail(
                "separation of duties: write_implementation and review_work cannot be held \
                 together (no approving own work)"
                    .to_string(),
            );
        }
        for capability in [Capability::WriteWorkspace, Capability::RunTests] {
            if employee.has(capability) && !employee.has(Capability::WriteImplementation) {
                fail(format!(
                    "{capability} requires write_implementation (only implementers touch the workspace)"
                ));
            }
        }
        if employee.has(Capability::ProposePlan) && contract.function != F::Planner {
            fail("only the planner may hold propose_plan".to_string());
        }
        if contract.employee_type == T::Subagent && employee.has(Capability::DelegateSubtasks) {
            fail("subagents cannot delegate (maximum delegation depth is 1)".to_string());
        }
        let mut seen_permissions = Vec::new();
        for permission in &contract.permissions {
            if seen_permissions.contains(permission) {
                fail(format!("permission {permission} is listed twice"));
            }
            seen_permissions.push(*permission);
        }
        for (field, items) in [
            ("skill_packs", &contract.skill_packs),
            ("mcp_servers", &contract.mcp_servers),
        ] {
            for (index, item) in items.iter().enumerate() {
                if items[..index].contains(item) {
                    fail(format!("{field}: '{item}' is listed twice"));
                }
            }
        }
        for pack in &contract.skill_packs {
            if !resources.skill_packs.contains_key(pack) {
                fail(format!(
                    "unknown skill pack '{pack}' (company/skills/{pack}/SKILL.md)"
                ));
            }
        }
        for server in &contract.mcp_servers {
            if !resources.mcp.servers.contains_key(server) {
                fail(format!("unknown MCP server '{server}' (company/mcp.yaml)"));
            }
        }
        if contract.employee_type == T::Subagent
            && let Some(manager) = employees.get(&contract.manager_id)
        {
            for server in &contract.mcp_servers {
                if !manager.contract.mcp_servers.contains(server) {
                    fail(format!(
                        "MCP server '{server}' exceeds manager {}'s servers",
                        contract.manager_id
                    ));
                }
            }
        }
        if employee.has(Capability::VetoReview) {
            let under_reviewer = contract.employee_type == T::Subagent
                && employees
                    .get(&contract.manager_id)
                    .is_some_and(|manager| manager.contract.function == F::Reviewer);
            if !under_reviewer || !employee.has(Capability::ReviewWork) {
                fail(
                    "veto_review is only for a reviewer's subagent that also holds review_work"
                        .to_string(),
                );
            }
        }
        for forbidden in &contract.forbidden_actions {
            if contract
                .permissions
                .iter()
                .any(|permission| permission.as_str() == forbidden)
            {
                fail(format!("'{forbidden}' is both permitted and forbidden"));
            }
        }
    }

    let orchestrators = employees
        .values()
        .filter(|employee| employee.contract.employee_type == T::System)
        .count();
    if orchestrators != 1 {
        errors.push(format!(
            "the registry must contain exactly one orchestrator (employee_type: system), found {orchestrators}"
        ));
    }

    let active_planners = employees
        .values()
        .filter(|employee| employee.is_active() && employee.contract.function == F::Planner)
        .count();
    if active_planners > 1 {
        errors.push(format!(
            "at most one active planner is allowed, found {active_planners}"
        ));
    }

    errors
}
