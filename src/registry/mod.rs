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

use crate::config::ModelConfig;

pub use hire::{HireRequest, approve_hire, check_proposal, propose_hire, set_status};

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

pub struct Registry {
    root: PathBuf,
    employees: BTreeMap<String, Employee>,
}

impl Registry {
    /// Loads and validates every employee folder; all problems are reported at once.
    pub fn load(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        let employees = load_dir(root)?;

        let errors = validate(&employees);
        if !errors.is_empty() {
            bail!(
                "invalid employee registry {}:\n- {}",
                root.display(),
                errors.join("\n- ")
            );
        }

        Ok(Self {
            root: root.to_path_buf(),
            employees,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
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
pub(crate) fn validate(employees: &BTreeMap<String, Employee>) -> Vec<String> {
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
                        if !manager.has(*permission) {
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
