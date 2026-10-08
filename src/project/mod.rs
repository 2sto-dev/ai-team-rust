//! Phase 3: projects broken into tasks, persisted between runs, executed one task at a time,
//! with Owner decisions for tasks that stop.

mod manager;
pub mod plan;
mod store;

use std::{fmt, str::FromStr};

use anyhow::bail;

pub use manager::{ProjectDeletion, ProjectManager, ProjectSettings, RunSummary};
pub use store::{DeletedProject, MilestoneRecord, OwnerDecision, ProjectRecord, Store, TaskRecord};

/// Task states (firma.md §13). Transitions are made by the control plane only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    Planned,
    Assigned,
    InProgress,
    Blocked,
    Review,
    ChangesRequired,
    Done,
    HumanReviewRequired,
    Failed,
    Cancelled,
}

impl TaskStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            TaskStatus::Planned => "PLANNED",
            TaskStatus::Assigned => "ASSIGNED",
            TaskStatus::InProgress => "IN_PROGRESS",
            TaskStatus::Blocked => "BLOCKED",
            TaskStatus::Review => "REVIEW",
            TaskStatus::ChangesRequired => "CHANGES_REQUIRED",
            TaskStatus::Done => "DONE",
            TaskStatus::HumanReviewRequired => "HUMAN_REVIEW_REQUIRED",
            TaskStatus::Failed => "FAILED",
            TaskStatus::Cancelled => "CANCELLED",
        }
    }

    /// A run was in the middle of this task when it stopped (crash, kill, power loss).
    pub fn is_interrupted(self) -> bool {
        matches!(
            self,
            TaskStatus::Assigned
                | TaskStatus::InProgress
                | TaskStatus::Review
                | TaskStatus::ChangesRequired
        )
    }

    /// Stopped and waiting for an Owner decision.
    pub fn needs_owner(self) -> bool {
        matches!(self, TaskStatus::HumanReviewRequired | TaskStatus::Failed)
    }

    /// Dependents of a task in this state cannot start.
    pub fn blocks_dependents(self) -> bool {
        matches!(
            self,
            TaskStatus::HumanReviewRequired
                | TaskStatus::Failed
                | TaskStatus::Cancelled
                | TaskStatus::Blocked
        )
    }
}

impl fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for TaskStatus {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> anyhow::Result<Self> {
        Ok(match value {
            "PLANNED" => TaskStatus::Planned,
            "ASSIGNED" => TaskStatus::Assigned,
            "IN_PROGRESS" => TaskStatus::InProgress,
            "BLOCKED" => TaskStatus::Blocked,
            "REVIEW" => TaskStatus::Review,
            "CHANGES_REQUIRED" => TaskStatus::ChangesRequired,
            "DONE" => TaskStatus::Done,
            "HUMAN_REVIEW_REQUIRED" => TaskStatus::HumanReviewRequired,
            "FAILED" => TaskStatus::Failed,
            "CANCELLED" => TaskStatus::Cancelled,
            other => bail!("unknown task status '{other}'"),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectStatus {
    /// Added; no approved task plan yet.
    Planning,
    /// Plan approved; no task started.
    Planned,
    InProgress,
    /// Every task is done or cancelled.
    Done,
}

impl ProjectStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ProjectStatus::Planning => "PLANNING",
            ProjectStatus::Planned => "PLANNED",
            ProjectStatus::InProgress => "IN_PROGRESS",
            ProjectStatus::Done => "DONE",
        }
    }
}

impl fmt::Display for ProjectStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ProjectStatus {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> anyhow::Result<Self> {
        Ok(match value {
            "PLANNING" => ProjectStatus::Planning,
            "PLANNED" => ProjectStatus::Planned,
            "IN_PROGRESS" => ProjectStatus::InProgress,
            "DONE" => ProjectStatus::Done,
            other => bail!("unknown project status '{other}'"),
        })
    }
}
