//! Task plans: the Planner proposes milestones and tasks; the control plane validates them
//! here and fixes the execution order. An invalid plan is never stored as approvable.

use std::collections::{BTreeSet, HashMap, HashSet};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

/// Upper bound on tasks in one plan, so a runaway proposal cannot flood the project.
pub const MAX_PLAN_TASKS: usize = 25;
const MAX_KEY_LEN: usize = 20;
const MAX_TITLE_LEN: usize = 120;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskPlan {
    pub milestones: Vec<PlannedMilestone>,
    #[serde(default)]
    pub rationale: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannedMilestone {
    pub name: String,
    pub tasks: Vec<PlannedTask>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannedTask {
    pub key: String,
    pub title: String,
    pub description: String,
    #[serde(default)]
    pub acceptance_criteria: Vec<String>,
    #[serde(default)]
    pub depends_on: Vec<String>,
}

/// A validated task with its milestone index, in execution order.
#[derive(Debug, Clone)]
pub struct OrderedTask {
    pub milestone: usize,
    pub task: PlannedTask,
}

/// Checks every rule and returns the tasks in dependency order (ties broken by milestone,
/// then by the order the Planner listed them). All violations are reported at once.
pub fn validate_plan(plan: &TaskPlan) -> Result<Vec<OrderedTask>> {
    let mut errors = Vec::new();

    if plan.milestones.is_empty() {
        errors.push("the plan has no milestones".to_string());
    }

    let mut location: HashMap<&str, (usize, usize)> = HashMap::new();
    let mut milestone_names = HashSet::new();
    let mut total = 0;

    for (m, milestone) in plan.milestones.iter().enumerate() {
        let name = milestone.name.trim();
        if name.is_empty() {
            errors.push(format!("milestone {} has no name", m + 1));
        } else if !milestone_names.insert(name.to_lowercase()) {
            errors.push(format!("duplicate milestone name '{name}'"));
        }
        if milestone.tasks.is_empty() {
            errors.push(format!("milestone '{name}' has no tasks"));
        }

        for (i, task) in milestone.tasks.iter().enumerate() {
            total += 1;
            let key = task.key.trim();
            if key.is_empty()
                || key.len() > MAX_KEY_LEN
                || !key
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                errors.push(format!(
                    "task key '{key}' must be 1-{MAX_KEY_LEN} letters, digits, '-' or '_'"
                ));
            } else if location.insert(key, (m, i)).is_some() {
                errors.push(format!("duplicate task key '{key}'"));
            }
            if task.title.trim().is_empty() || task.title.len() > MAX_TITLE_LEN {
                errors.push(format!(
                    "task {key}: title must be 1-{MAX_TITLE_LEN} characters"
                ));
            }
            if task.description.trim().is_empty() {
                errors.push(format!("task {key}: description is empty"));
            }
            if task.acceptance_criteria.is_empty()
                || task
                    .acceptance_criteria
                    .iter()
                    .any(|criterion| criterion.trim().is_empty())
            {
                errors.push(format!(
                    "task {key}: needs at least one non-empty acceptance criterion"
                ));
            }
        }
    }

    if total > MAX_PLAN_TASKS {
        errors.push(format!(
            "the plan has {total} tasks; the maximum is {MAX_PLAN_TASKS}"
        ));
    }

    for (m, milestone) in plan.milestones.iter().enumerate() {
        for task in &milestone.tasks {
            let key = task.key.trim();
            let mut seen = HashSet::new();
            for dependency in &task.depends_on {
                let dependency = dependency.trim();
                if !seen.insert(dependency) {
                    errors.push(format!("task {key}: duplicate dependency '{dependency}'"));
                } else if dependency == key {
                    errors.push(format!("task {key} depends on itself"));
                } else {
                    match location.get(dependency) {
                        None => errors.push(format!(
                            "task {key}: depends on unknown task '{dependency}'"
                        )),
                        Some((dependency_milestone, _)) if *dependency_milestone > m => errors
                            .push(format!(
                                "task {key}: depends on '{dependency}' from a later milestone"
                            )),
                        Some(_) => {}
                    }
                }
            }
        }
    }

    if !errors.is_empty() {
        bail!("{}", errors.join("; "));
    }

    order_tasks(plan, &location)
}

/// Kahn's algorithm, always taking the earliest (milestone, position) task that is ready.
fn order_tasks(
    plan: &TaskPlan,
    location: &HashMap<&str, (usize, usize)>,
) -> Result<Vec<OrderedTask>> {
    let tasks: HashMap<&str, (usize, &PlannedTask)> = plan
        .milestones
        .iter()
        .enumerate()
        .flat_map(|(m, milestone)| {
            milestone
                .tasks
                .iter()
                .map(move |task| (task.key.trim(), (m, task)))
        })
        .collect();

    let mut pending: HashMap<&str, usize> = HashMap::new();
    let mut dependents: HashMap<&str, Vec<&str>> = HashMap::new();
    for (key, (_, task)) in &tasks {
        pending.insert(key, task.depends_on.len());
        for dependency in &task.depends_on {
            dependents.entry(dependency.trim()).or_default().push(key);
        }
    }

    let mut ready: BTreeSet<((usize, usize), &str)> = pending
        .iter()
        .filter(|(_, count)| **count == 0)
        .map(|(key, _)| (location[key], *key))
        .collect();

    let mut ordered = Vec::with_capacity(tasks.len());
    while let Some(next) = ready.pop_first() {
        let key = next.1;
        let (milestone, task) = tasks[key];
        ordered.push(OrderedTask {
            milestone,
            task: task.clone(),
        });
        for dependent in dependents.get(key).into_iter().flatten() {
            let count = pending.get_mut(dependent).expect("known task");
            *count -= 1;
            if *count == 0 {
                ready.insert((location[dependent], dependent));
            }
        }
    }

    if ordered.len() < tasks.len() {
        let mut stuck: Vec<&str> = pending
            .iter()
            .filter(|(_, count)| **count > 0)
            .map(|(key, _)| *key)
            .collect();
        stuck.sort();
        bail!("dependency cycle among tasks: {}", stuck.join(", "));
    }
    Ok(ordered)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(key: &str, depends_on: &[&str]) -> PlannedTask {
        PlannedTask {
            key: key.to_string(),
            title: format!("Task {key}"),
            description: "do it".to_string(),
            acceptance_criteria: vec!["works".to_string()],
            depends_on: depends_on.iter().map(|d| d.to_string()).collect(),
        }
    }

    fn plan(milestones: Vec<(&str, Vec<PlannedTask>)>) -> TaskPlan {
        TaskPlan {
            milestones: milestones
                .into_iter()
                .map(|(name, tasks)| PlannedMilestone {
                    name: name.to_string(),
                    tasks,
                })
                .collect(),
            rationale: String::new(),
        }
    }

    fn keys(ordered: &[OrderedTask]) -> Vec<&str> {
        ordered.iter().map(|o| o.task.key.as_str()).collect()
    }

    #[test]
    fn orders_by_dependencies_then_listing() {
        // T2 is listed first but depends on T1.
        let plan = plan(vec![
            ("Core", vec![task("T2", &["T1"]), task("T1", &[])]),
            ("Extras", vec![task("T3", &["T2"])]),
        ]);
        assert_eq!(keys(&validate_plan(&plan).unwrap()), ["T1", "T2", "T3"]);
    }

    #[test]
    fn rejects_cycles() {
        let plan = plan(vec![(
            "Core",
            vec![task("A", &["C"]), task("B", &["A"]), task("C", &["B"])],
        )]);
        let err = validate_plan(&plan).unwrap_err().to_string();
        assert!(
            err.contains("dependency cycle among tasks: A, B, C"),
            "{err}"
        );
    }

    #[test]
    fn reports_every_violation() {
        let mut no_criteria = task("T2", &["T9"]);
        no_criteria.acceptance_criteria.clear();
        let plan = plan(vec![
            (
                "Core",
                vec![task("T1", &["T3"]), no_criteria, task("T1", &[])],
            ),
            ("Later", vec![task("T3", &[])]),
        ]);
        let err = validate_plan(&plan).unwrap_err().to_string();
        for expected in [
            "duplicate task key 'T1'",
            "task T2: needs at least one non-empty acceptance criterion",
            "task T2: depends on unknown task 'T9'",
            "task T1: depends on 'T3' from a later milestone",
        ] {
            assert!(err.contains(expected), "missing '{expected}' in: {err}");
        }
    }

    #[test]
    fn rejects_empty_and_oversized_plans() {
        assert!(validate_plan(&plan(vec![])).is_err());
        let tasks = (0..=MAX_PLAN_TASKS)
            .map(|i| task(&format!("T{i}"), &[]))
            .collect();
        let err = validate_plan(&plan(vec![("Big", tasks)]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("the maximum is 25"));
    }
}
