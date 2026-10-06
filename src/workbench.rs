//! The bridge between the workflow and real execution: shows the Builder its workspace,
//! applies each implementation as files, and runs the Owner's test command.

use std::time::Duration;

use anyhow::Result;

use crate::{
    BoxFuture,
    domain::{Artifact, Verification},
    registry::Capability,
    workspace::{Workspace, extract_files, run_tests},
};

/// How much workspace content the Builder sees in its prompt.
const SNAPSHOT_BUDGET_CHARS: usize = 24_000;

pub trait Workbench: Send + Sync {
    /// The workspace and the rules for answers, shown to the Builder.
    fn briefing(&self) -> Result<String>;

    /// What the Builder must hold for the platform to act on its behalf.
    fn required_capabilities(&self) -> Vec<Capability>;

    /// Applies an implementation and checks it. `Err` means the check itself could not run.
    fn verify<'a>(&'a self, implementation: &'a Artifact) -> BoxFuture<'a, Result<Verification>>;
}

/// Applies one task's answers on its branch, runs the project's test command, and commits
/// every iteration (as the employee who wrote it).
pub struct TaskWorkbench {
    workspace: Workspace,
    task_id: String,
    test_command: Option<String>,
    test_timeout: Duration,
}

impl TaskWorkbench {
    pub fn new(
        workspace: Workspace,
        task_id: impl Into<String>,
        test_command: Option<String>,
        test_timeout: Duration,
    ) -> Self {
        Self {
            workspace,
            task_id: task_id.into(),
            test_command: test_command.filter(|command| !command.trim().is_empty()),
            test_timeout,
        }
    }

    pub fn workspace(&self) -> &Workspace {
        &self.workspace
    }
}

impl Workbench for TaskWorkbench {
    fn briefing(&self) -> Result<String> {
        let tests = match &self.test_command {
            Some(command) => format!(
                "- After writing, the platform runs the Owner's test command `{command}` in the \
                 workspace root. Your work goes to review only if it exits with code 0, so ship \
                 the tests together with the code."
            ),
            None => {
                "- No test command is configured; your files are reviewed as written.".to_string()
            }
        };
        Ok(format!(
            "WORKSPACE (the project's files as this task sees them):\n{}\n\n\
             HOW YOUR ANSWER IS APPLIED:\n\
             - Write every file you create or change as a heading `### FILE: relative/path` \
             followed by ONE fenced code block with the COMPLETE file content.\n\
             - To delete a file, write a heading `### DELETE: relative/path` (no code block).\n\
             - Paths are relative to the workspace root; no `..` and no absolute paths.\n\
             - Files you do not mention stay as they are. Each answer is committed on the \
             task's git branch.\n\
             {tests}",
            self.workspace.snapshot(SNAPSHOT_BUDGET_CHARS)?
        ))
    }

    fn required_capabilities(&self) -> Vec<Capability> {
        let mut capabilities = vec![Capability::WriteWorkspace];
        if self.test_command.is_some() {
            capabilities.push(Capability::RunTests);
        }
        capabilities
    }

    fn verify<'a>(&'a self, implementation: &'a Artifact) -> BoxFuture<'a, Result<Verification>> {
        Box::pin(async move {
            let answer = extract_files(&implementation.content);
            let mut problems = answer.problems;
            if answer.files.is_empty() && answer.deletions.is_empty() && problems.is_empty() {
                problems.push(
                    "no files found: write each file as `### FILE: relative/path` followed by a \
                     fenced code block"
                        .to_string(),
                );
            }
            let (files_written, write_problems) = self.workspace.write_files(&answer.files)?;
            let (files_deleted, delete_problems) =
                self.workspace.delete_files(&answer.deletions)?;
            problems.extend(write_problems);
            problems.extend(delete_problems);

            let test = match (&self.test_command, problems.is_empty()) {
                (Some(command), true) => {
                    Some(run_tests(self.workspace.root(), command, self.test_timeout).await?)
                }
                _ => None,
            };

            // Every iteration is recorded, passing or not: the branch is the task's history.
            let changed: Vec<String> = files_written
                .iter()
                .chain(&files_deleted)
                .cloned()
                .collect();
            let outcome = match (&test, problems.is_empty()) {
                (_, false) => "rejected by the platform (invalid answer)".to_string(),
                (Some(test), true) if test.passed => format!("tests passed (`{}`)", test.command),
                (Some(test), true) => format!("tests FAILED (`{}`)", test.command),
                (None, true) => "no test command".to_string(),
            };
            let commit = self.workspace.commit_paths(
                &changed,
                &implementation.author,
                &format!(
                    "{}: iteration {}\n\nVerification: {outcome}",
                    self.task_id, implementation.revision
                ),
            )?;

            Ok(Verification {
                files_written,
                files_deleted,
                problems,
                test,
                commit,
            })
        })
    }
}
