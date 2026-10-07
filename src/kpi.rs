//! KPIs for the AI employees (firma.md §18), computed from the stored tasks and runs.
//!
//! A task counts for its Builder (whose work is measured) and its Reviewer (whose approvals
//! are). Quality comes first: an Owner accept is not a Reviewer approval, and approvals given
//! despite platform warnings are counted separately.

use std::{collections::BTreeMap, fmt};

use anyhow::Result;

use crate::{
    domain::TeamAssignment,
    llm::UsageTotals,
    project::{Store, TaskStatus},
};

/// What one task contributes to the KPIs.
#[derive(Debug, Clone)]
pub struct TaskFacts {
    pub status: TaskStatus,
    pub team: Option<TeamAssignment>,
    /// Builder iterations used.
    pub iterations: u32,
    pub spec_revisions: u32,
    pub usage: UsageTotals,
    /// The Owner approved it, not the Reviewer.
    pub owner_accepted: bool,
    /// The Owner had to step in: resume, accept or cancel, or the task waits for a decision.
    pub escalated: bool,
    /// The last verification carried warnings (e.g. tests lost) and the task is done.
    pub approved_with_warnings: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Kpi {
    /// Tasks a team worked on.
    pub tasks: u32,
    pub done: u32,
    /// Done with the Reviewer's approval on the first iteration, no specification revision.
    pub first_pass: u32,
    pub owner_accepted: u32,
    pub escalated: u32,
    pub failed: u32,
    pub cancelled: u32,
    pub approved_with_warnings: u32,
    /// Correction cycles (iterations after the first) over done tasks.
    pub correction_cycles: u32,
    pub spec_revisions: u32,
    /// Usage of done tasks only (cost per completed task).
    pub done_usage: UsageTotals,
    pub usage: UsageTotals,
}

impl Kpi {
    pub fn add(&mut self, facts: &TaskFacts) {
        self.tasks += 1;
        self.usage.add(&facts.usage);
        self.spec_revisions += facts.spec_revisions;
        self.escalated += u32::from(facts.escalated);
        match facts.status {
            TaskStatus::Done => {
                self.done += 1;
                self.done_usage.add(&facts.usage);
                self.correction_cycles += facts.iterations.saturating_sub(1);
                if facts.owner_accepted {
                    self.owner_accepted += 1;
                } else if facts.iterations <= 1 && facts.spec_revisions == 0 {
                    self.first_pass += 1;
                }
                self.approved_with_warnings += u32::from(facts.approved_with_warnings);
            }
            TaskStatus::Failed => self.failed += 1,
            TaskStatus::Cancelled => self.cancelled += 1,
            _ => {}
        }
    }

    fn rate(part: u32, whole: u32) -> String {
        if whole == 0 {
            "-".to_string()
        } else {
            format!(
                "{:.0}% ({part}/{whole})",
                f64::from(part) * 100.0 / f64::from(whole)
            )
        }
    }

    fn average(total: f64, count: u32) -> Option<f64> {
        (count > 0).then(|| total / f64::from(count))
    }
}

#[derive(Debug, Default)]
pub struct KpiReport {
    pub overall: Kpi,
    pub by_builder: BTreeMap<String, Kpi>,
    pub by_reviewer: BTreeMap<String, Kpi>,
}

impl KpiReport {
    pub fn add(&mut self, facts: &TaskFacts) {
        self.overall.add(facts);
        if let Some(team) = &facts.team {
            self.by_builder
                .entry(team.builder.clone())
                .or_default()
                .add(facts);
            self.by_reviewer
                .entry(team.reviewer.clone())
                .or_default()
                .add(facts);
        }
    }
}

/// KPIs over every task a team worked on, in one project or in all of them.
pub fn collect(store: &Store, project_id: Option<&str>) -> Result<KpiReport> {
    let projects: Vec<String> = match project_id {
        Some(id) => vec![store.project(id)?.config.project_id],
        None => store
            .projects()?
            .into_iter()
            .map(|project| project.config.project_id)
            .collect(),
    };
    let mut report = KpiReport::default();
    for project in projects {
        let decisions = store.owner_decisions(&project)?;
        for task in store.tasks(&project)? {
            let Some(state) = store.load_task_state(&task.id)? else {
                continue; // never started
            };
            let decided = |kind: &str| {
                decisions.iter().any(|decision| {
                    decision.task_id.as_deref() == Some(task.id.as_str())
                        && decision.decision == kind
                })
            };
            let owner_accepted = decided("accept");
            report.add(&TaskFacts {
                status: task.status,
                team: state.team.clone(),
                iterations: state.iteration,
                spec_revisions: state.architecture_revisions,
                usage: state.usage.clone(),
                owner_accepted,
                escalated: owner_accepted
                    || decided("resume")
                    || decided("cancel")
                    || task.status.needs_owner(),
                approved_with_warnings: task.status == TaskStatus::Done
                    && state
                        .verification
                        .as_ref()
                        .is_some_and(|verification| !verification.warnings.is_empty()),
            });
        }
    }
    Ok(report)
}

impl fmt::Display for KpiReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let all = &self.overall;
        writeln!(f, "Tasks worked on:            {}", all.tasks)?;
        writeln!(
            f,
            "Task completion rate:       {}",
            Kpi::rate(all.done, all.tasks)
        )?;
        writeln!(
            f,
            "Review pass rate (1st try): {}",
            Kpi::rate(all.first_pass, all.done)
        )?;
        writeln!(
            f,
            "Average correction cycles:  {}",
            Kpi::average(f64::from(all.correction_cycles), all.done)
                .map_or("-".to_string(), |value| format!("{value:.1}"))
        )?;
        writeln!(
            f,
            "Escalation rate:            {}",
            Kpi::rate(all.escalated, all.tasks)
        )?;
        writeln!(f, "Accepted by the Owner:      {}", all.owner_accepted)?;
        writeln!(
            f,
            "Approved despite warnings:  {}",
            all.approved_with_warnings
        )?;
        writeln!(
            f,
            "Failed / cancelled:         {} / {}",
            all.failed, all.cancelled
        )?;
        match Kpi::average(
            (all.done_usage.input_tokens + all.done_usage.output_tokens) as f64,
            all.done,
        ) {
            Some(tokens) => {
                write!(f, "Tokens per completed task:  {tokens:.0}")?;
                if let Some(cost) = Kpi::average(all.done_usage.cost_usd, all.done)
                    && cost > 0.0
                {
                    write!(f, " (${cost:.4})")?;
                }
                writeln!(f)?;
            }
            None => writeln!(f, "Tokens per completed task:  -")?,
        }
        writeln!(f, "Usage (all tasks):          {}", all.usage)?;

        for (title, table) in [
            ("Builder", &self.by_builder),
            ("Reviewer", &self.by_reviewer),
        ] {
            if table.is_empty() {
                continue;
            }
            writeln!(
                f,
                "\n{title:<16} {:>5} {:>5} {:>10} {:>7} {:>9} {:>8}",
                "tasks", "done", "1st-pass", "cycles", "escalated", "warnings"
            )?;
            for (employee, kpi) in table {
                writeln!(
                    f,
                    "{employee:<16} {:>5} {:>5} {:>10} {:>7} {:>9} {:>8}",
                    kpi.tasks,
                    kpi.done,
                    kpi.first_pass,
                    Kpi::average(f64::from(kpi.correction_cycles), kpi.done)
                        .map_or("-".to_string(), |value| format!("{value:.1}")),
                    kpi.escalated,
                    kpi.approved_with_warnings
                )?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(status: TaskStatus, iterations: u32) -> TaskFacts {
        TaskFacts {
            status,
            team: Some(TeamAssignment {
                architect: "EMP-ARCH-001".to_string(),
                builder: "EMP-BUILD-001".to_string(),
                reviewer: "EMP-REV-001".to_string(),
            }),
            iterations,
            spec_revisions: 0,
            usage: UsageTotals {
                calls: 2,
                input_tokens: 100,
                output_tokens: 50,
                ..UsageTotals::default()
            },
            owner_accepted: false,
            escalated: false,
            approved_with_warnings: false,
        }
    }

    #[test]
    fn owner_accepts_are_not_first_pass_approvals() {
        let mut report = KpiReport::default();
        report.add(&facts(TaskStatus::Done, 1));
        report.add(&facts(TaskStatus::Done, 3));
        report.add(&TaskFacts {
            owner_accepted: true,
            escalated: true,
            ..facts(TaskStatus::Done, 1)
        });
        report.add(&TaskFacts {
            escalated: true,
            ..facts(TaskStatus::HumanReviewRequired, 3)
        });

        let all = &report.overall;
        assert_eq!((all.tasks, all.done, all.first_pass), (4, 3, 1));
        assert_eq!(all.owner_accepted, 1);
        assert_eq!(all.escalated, 2);
        assert_eq!(all.correction_cycles, 2);
        assert_eq!(all.done_usage.input_tokens, 300);
        assert_eq!(report.by_builder["EMP-BUILD-001"].tasks, 4);
        let text = report.to_string();
        assert!(
            text.contains("Task completion rate:       75% (3/4)"),
            "{text}"
        );
        assert!(
            text.contains("Review pass rate (1st try): 33% (1/3)"),
            "{text}"
        );
    }
}
