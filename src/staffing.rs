//! Control-plane side of staffing: checks a proposed team against the registry and builds
//! the agents from the employees' contracts. Nothing here asks a model for a decision.

use anyhow::{Context, Result, bail};

use crate::{
    agents::{Agent, architect::Architect, builder::Builder, reviewer::Reviewer},
    config::ModelConfig,
    domain::{Role, TeamAssignment},
    llm::ProviderFactory,
    registry::{Employee, EmployeeFunction, Registry},
};

pub struct Team {
    pub architect: Box<dyn Agent>,
    pub builder: Box<dyn Agent>,
    pub reviewer: Box<dyn Agent>,
}

fn slots(assignment: &TeamAssignment) -> [(EmployeeFunction, &str); 3] {
    [
        (EmployeeFunction::Architect, &assignment.architect),
        (EmployeeFunction::Builder, &assignment.builder),
        (EmployeeFunction::Reviewer, &assignment.reviewer),
    ]
}

/// Rejects any assignment that names an unknown, inactive or wrongly-qualified employee.
pub fn validate_assignment(registry: &Registry, assignment: &TeamAssignment) -> Result<()> {
    let mut errors = Vec::new();

    for (function, employee_id) in slots(assignment) {
        match registry.get(employee_id) {
            None => errors.push(format!("{function}: unknown employee '{employee_id}'")),
            Some(employee) if !employee.is_active() => errors.push(format!(
                "{function}: {employee_id} is {}",
                employee.contract.status
            )),
            Some(employee) if employee.contract.function != function => errors.push(format!(
                "{function}: {employee_id} is a {}",
                employee.contract.function
            )),
            Some(employee) => {
                if let Some(capability) = function.required_capability()
                    && !employee.has(capability)
                {
                    errors.push(format!("{function}: {employee_id} lacks {capability}"));
                }
            }
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        bail!(
            "team assignment rejected by control plane: {}",
            errors.join("; ")
        )
    }
}

/// Model-level independence check between Builder and Reviewer (allowed, but reported).
pub fn same_model_warning(registry: &Registry, assignment: &TeamAssignment) -> Option<String> {
    let builder = model_of(registry.get(&assignment.builder)?).ok()?;
    let reviewer = model_of(registry.get(&assignment.reviewer)?).ok()?;
    builder.same_model_as(reviewer).then(|| {
        format!(
            "builder {} and reviewer {} use the same model ({}); independence relies on separate context only",
            assignment.builder, assignment.reviewer, builder.model
        )
    })
}

/// Builds the agents; call only after `validate_assignment` succeeded.
pub fn build_team(
    registry: &Registry,
    assignment: &TeamAssignment,
    providers: &ProviderFactory,
) -> Result<Team> {
    let employee = |id: &str| {
        registry
            .get(id)
            .with_context(|| format!("unknown employee {id}"))
    };

    let architect = employee(&assignment.architect)?;
    let builder = employee(&assignment.builder)?;
    let reviewer = employee(&assignment.reviewer)?;

    Ok(Team {
        architect: Box::new(Architect::new(
            architect.id(),
            architect.job_description.clone(),
            providers(Role::Architect, model_of(architect)?)?,
        )),
        builder: Box::new(Builder::new(
            builder.id(),
            builder.job_description.clone(),
            providers(Role::Builder, model_of(builder)?)?,
        )),
        reviewer: Box::new(Reviewer::new(
            reviewer.id(),
            reviewer.job_description.clone(),
            providers(Role::Reviewer, model_of(reviewer)?)?,
        )),
    })
}

pub fn model_of(employee: &Employee) -> Result<&ModelConfig> {
    employee
        .contract
        .model
        .as_ref()
        .with_context(|| format!("{} has no model", employee.id()))
}
