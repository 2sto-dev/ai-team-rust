//! Control-plane side of staffing: checks a proposed team against the registry and builds
//! the agents from the employees' contracts. Nothing here asks a model for a decision.

use anyhow::{Context, Result, bail};

use crate::{
    agents::{
        Agent,
        architect::Architect,
        builder::Builder,
        lead::BuilderLead,
        reviewer::{Advisor, Reviewer},
        specialist::Specialist,
    },
    config::ModelConfig,
    domain::{Role, TeamAssignment},
    llm::ProviderFactory,
    registry::{Capability, Employee, EmployeeFunction, Registry},
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
/// Builds the agents; call only after `validate_assignment` succeeded. Each employee gets
/// its job description plus skill packs as system prompt and its MCP tools. A Builder with
/// active implementer subagents leads them; a Reviewer's reviewing subagents advise it.
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

    let specialist = |subagent: &Employee| -> Result<Specialist> {
        let contract = &subagent.contract;
        let mut profile = format!(
            "{} | skills: {}",
            contract.title,
            contract.skills.join(", ")
        );
        if !contract.skill_packs.is_empty() {
            profile.push_str(&format!(
                " | skill packs: {}",
                contract.skill_packs.join(", ")
            ));
        }
        if !contract.mcp_servers.is_empty() {
            profile.push_str(&format!(" | tools: {}", contract.mcp_servers.join(", ")));
        }
        Ok(Specialist::new(
            subagent.id(),
            registry.system_prompt(subagent),
            providers(Role::Specialist, model_of(subagent)?)?,
        )
        .with_toolbox(registry.toolbox_for(subagent))
        .with_profile(profile))
    };

    let builder_agent: Box<dyn Agent> = {
        let team: Vec<Specialist> = if builder.has(Capability::DelegateSubtasks) {
            registry
                .active_subagents_of(builder.id())
                .into_iter()
                .filter(|subagent| subagent.has(Capability::WriteImplementation))
                .map(specialist)
                .collect::<Result<_>>()?
        } else {
            Vec::new()
        };
        let llm = providers(Role::Builder, model_of(builder)?)?;
        if team.is_empty() {
            Box::new(
                Builder::new(builder.id(), registry.system_prompt(builder), llm)
                    .with_toolbox(registry.toolbox_for(builder)),
            )
        } else {
            Box::new(BuilderLead::new(
                builder.id(),
                registry.system_prompt(builder),
                llm,
                registry.toolbox_for(builder),
                team,
            ))
        }
    };

    let advisors: Vec<Advisor> = if reviewer.has(Capability::DelegateSubtasks) {
        registry
            .active_subagents_of(reviewer.id())
            .into_iter()
            .filter(|subagent| subagent.has(Capability::ReviewWork))
            .map(|subagent| {
                Ok(Advisor {
                    veto: subagent.has(Capability::VetoReview),
                    specialist: specialist(subagent)?,
                })
            })
            .collect::<Result<_>>()?
    } else {
        Vec::new()
    };

    Ok(Team {
        architect: Box::new(
            Architect::new(
                architect.id(),
                registry.system_prompt(architect),
                providers(Role::Architect, model_of(architect)?)?,
            )
            .with_toolbox(registry.toolbox_for(architect)),
        ),
        builder: builder_agent,
        reviewer: Box::new(
            Reviewer::new(
                reviewer.id(),
                registry.system_prompt(reviewer),
                providers(Role::Reviewer, model_of(reviewer)?)?,
            )
            .with_toolbox(registry.toolbox_for(reviewer))
            .with_advisors(advisors),
        ),
    })
}

pub fn model_of(employee: &Employee) -> Result<&ModelConfig> {
    employee
        .contract
        .model
        .as_ref()
        .with_context(|| format!("{} has no model", employee.id()))
}
