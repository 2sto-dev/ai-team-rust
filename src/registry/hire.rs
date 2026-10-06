//! Hiring procedure: a proposal is written to `company/proposals/<ID>/`, validated against the
//! live registry, and moved into `company/employees/` only when the Owner approves it.

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde_json::json;

use super::{
    CONTRACT_FILE, Employee, EmployeeFunction, EmployeeStatus, EmployeeType, JOB_DESCRIPTION_FILE,
    OWNER, Registry, load_dir, load_employee, load_resources, validate,
};
use crate::{agents::new_span_id, audit::AuditTrail};

const PLACEHOLDER: &str = "TODO";

pub struct HireRequest {
    pub employee_id: String,
    pub name: String,
    pub function: EmployeeFunction,
    pub manager_id: String,
    pub title: Option<String>,
    pub department: Option<String>,
}

fn employees_dir(company: &Path) -> PathBuf {
    company.join("employees")
}

fn proposals_dir(company: &Path) -> PathBuf {
    company.join("proposals")
}

/// Writes a proposal skeleton. Placeholders (`TODO`) must be filled before approval.
pub fn propose_hire(company: &Path, request: &HireRequest) -> Result<PathBuf> {
    let registry = Registry::load(employees_dir(company))?;

    anyhow::ensure!(
        request.function != EmployeeFunction::Orchestrator,
        "the company has exactly one orchestrator; it cannot be hired"
    );
    anyhow::ensure!(
        registry.get(&request.employee_id).is_none(),
        "{} already exists in the registry",
        request.employee_id
    );
    let manager = registry
        .get(&request.manager_id)
        .with_context(|| format!("unknown manager {}", request.manager_id))?;

    let employee_type = if request.function == EmployeeFunction::Specialist {
        EmployeeType::Subagent
    } else {
        EmployeeType::Agent
    };

    // A specialist starts with the capability of the department it joins.
    let permission = match request.function.required_capability() {
        Some(capability) => Some(capability),
        None => manager.contract.function.required_capability(),
    };

    // Model: the manager's; for a lead reporting to the orchestrator (no model), the model
    // of an active colleague in the same function, else of any active agent.
    let model = manager
        .contract
        .model
        .clone()
        .or_else(|| {
            registry
                .active(request.function)
                .first()
                .and_then(|colleague| colleague.contract.model.clone())
        })
        .or_else(|| {
            registry
                .employees()
                .filter(|employee| employee.is_active())
                .find_map(|employee| employee.contract.model.clone())
        })
        .context("no active employee has a model to copy; write the model block by hand")?;
    let model_yaml = serde_yaml_ng::to_string(&model).context("cannot serialize model")?;

    let dir = proposals_dir(company).join(&request.employee_id);
    anyhow::ensure!(
        !dir.exists(),
        "a proposal for {} already exists: {}",
        request.employee_id,
        dir.display()
    );
    fs::create_dir_all(&dir)
        .with_context(|| format!("cannot create proposal folder {}", dir.display()))?;

    let contract = format!(
        "# Operational contract for {id} (created with `ai-team hire propose`)\n\
         employee_id: {id}\n\
         name: {name}\n\
         title: {title}\n\
         department: {department}\n\
         manager_id: {manager}\n\
         employee_type: {employee_type}\n\
         function: {function}\n\
         status: active\n\
         model:\n{model}\
         skills:\n  - {PLACEHOLDER}\n\
         # Skill packs from company/skills/<id>/ and MCP servers from company/mcp.yaml:\n\
         skill_packs: []\n\
         mcp_servers: []\n\
         permissions:\n{permissions}\
         forbidden_actions: []\n",
        id = request.employee_id,
        name = yaml_string(&request.name),
        title = yaml_string(request.title.as_deref().unwrap_or(&request.name)),
        department = yaml_string(
            request
                .department
                .as_deref()
                .unwrap_or(&manager.contract.department)
        ),
        manager = request.manager_id,
        employee_type = match employee_type {
            EmployeeType::Agent => "agent",
            _ => "subagent",
        },
        function = request.function,
        model = indent(&model_yaml, 2),
        permissions = match permission {
            Some(capability) => format!("  - {capability}\n"),
            None => "  []\n".to_string(),
        },
    );

    let job_description = format!(
        "# {name} ({id})\n\n\
         ## Purpose\n{PLACEHOLDER}\n\n\
         ## Reports to\n{manager}\n\n\
         ## Responsibilities\n- {PLACEHOLDER}\n\n\
         ## Skills\n- {PLACEHOLDER}\n\n\
         ## Deliverables\n- {PLACEHOLDER}\n\n\
         ## Rules\n- Work only on the task you are given and with the context you receive.\n\
         - Never claim a test or command was executed when no execution tool was available.\n\n\
         ## Escalation\n- Report to {manager} when the task cannot be completed as specified.\n",
        name = request.name,
        id = request.employee_id,
        manager = request.manager_id,
    );

    fs::write(dir.join(CONTRACT_FILE), contract)?;
    fs::write(dir.join(JOB_DESCRIPTION_FILE), job_description)?;
    Ok(dir)
}

/// Validates a proposal as if it were already hired, without changing anything.
pub fn check_proposal(company: &Path, employee_id: &str) -> Result<Employee> {
    let proposal = proposals_dir(company).join(employee_id);
    anyhow::ensure!(
        proposal.is_dir(),
        "no proposal found at {}",
        proposal.display()
    );

    let mut candidate = load_employee(&proposal)?;

    for file in [CONTRACT_FILE, JOB_DESCRIPTION_FILE] {
        let text = fs::read_to_string(proposal.join(file))?;
        anyhow::ensure!(
            !text.contains(PLACEHOLDER),
            "{file} still contains {PLACEHOLDER} placeholders"
        );
    }

    let mut employees = load_dir(&employees_dir(company))?;
    anyhow::ensure!(
        !employees.contains_key(employee_id),
        "{employee_id} already exists in the registry"
    );
    candidate.dir = employees_dir(company).join(employee_id);
    employees.insert(employee_id.to_string(), candidate.clone());

    let errors = validate(&employees, &load_resources(company)?);
    if !errors.is_empty() {
        bail!(
            "proposal {employee_id} would make the registry invalid:\n- {}",
            errors.join("\n- ")
        );
    }
    Ok(candidate)
}

/// Owner approval: validates, moves the proposal into the registry and records the decision.
pub fn approve_hire(company: &Path, employee_id: &str, audit_dir: &Path) -> Result<Employee> {
    let employee = check_proposal(company, employee_id)?;

    fs::create_dir_all(employees_dir(company))?;
    fs::rename(proposals_dir(company).join(employee_id), &employee.dir)
        .with_context(|| format!("cannot move proposal into {}", employee.dir.display()))?;

    record_hr_event(
        audit_dir,
        "HIRE_APPROVED",
        &json!({ "contract": employee.contract }),
    )?;
    Ok(employee)
}

/// Changes `status:` in place (comments and layout of the contract are preserved).
pub fn set_status(
    company: &Path,
    employee_id: &str,
    status: EmployeeStatus,
    audit_dir: &Path,
) -> Result<()> {
    let registry = Registry::load(employees_dir(company))?;
    let employee = registry
        .get(employee_id)
        .with_context(|| format!("unknown employee {employee_id}"))?;
    anyhow::ensure!(
        employee.contract.employee_type != EmployeeType::System,
        "the orchestrator's status cannot be changed"
    );
    let previous = employee.contract.status;

    let path = employee.dir.join(CONTRACT_FILE);
    let original = fs::read_to_string(&path)?;

    let mut replaced = false;
    let mut updated: String = original
        .lines()
        .map(|line| {
            if line.starts_with("status:") {
                replaced = true;
                format!("status: {status}")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    if original.ends_with('\n') {
        updated.push('\n');
    }
    anyhow::ensure!(
        replaced,
        "no top-level 'status:' line in {}",
        path.display()
    );

    fs::write(&path, &updated)?;
    if let Err(err) = Registry::load(employees_dir(company)) {
        fs::write(&path, &original)?;
        return Err(err.context(format!("status change reverted for {employee_id}")));
    }

    record_hr_event(
        audit_dir,
        "STATUS_CHANGED",
        &json!({ "employee_id": employee_id, "from": previous, "to": status }),
    )
}

fn record_hr_event(audit_dir: &Path, event: &str, payload: &serde_json::Value) -> Result<()> {
    let audit = AuditTrail::create(audit_dir, "hr")?;
    audit.record("hr", &new_span_id(), None, OWNER, event, payload)
}

/// JSON strings are valid YAML double-quoted scalars, so this escapes safely.
fn yaml_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| format!("\"{value}\""))
}

fn indent(text: &str, spaces: usize) -> String {
    let pad = " ".repeat(spaces);
    text.lines().map(|line| format!("{pad}{line}\n")).collect()
}
