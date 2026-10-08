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

/// Build tools that look for their manifest in parent folders, and the manifest they need.
const MANIFEST_TOOLS: [(&str, &str); 6] = [
    ("cargo", "Cargo.toml"),
    ("npm", "package.json"),
    ("npx", "package.json"),
    ("yarn", "package.json"),
    ("pnpm", "package.json"),
    ("go", "go.mod"),
];

/// Cargo, npm and go walk up to the nearest manifest. Workspaces live inside the platform's
/// folder, so a project without its own `Cargo.toml` silently ran the platform's tests in a
/// real run, and the gate passed. A missing manifest that a parent folder has is a problem.
fn foreign_manifest(root: &std::path::Path, command: &str) -> Option<String> {
    let words: Vec<&str> = command
        .split(|c: char| c.is_whitespace() || matches!(c, '&' | '|' | ';' | '(' | ')'))
        .filter(|word| !word.is_empty())
        .collect();
    for (tool, manifest) in MANIFEST_TOOLS {
        let used = words.iter().any(|word| {
            let name = word.rsplit(['/', '\\']).next().unwrap_or(word);
            name.eq_ignore_ascii_case(tool)
                || name.eq_ignore_ascii_case(&format!("{tool}.exe"))
                || name.eq_ignore_ascii_case(&format!("{tool}.cmd"))
        });
        if !used || root.join(manifest).is_file() {
            continue;
        }
        if let Some(parent) = root
            .ancestors()
            .skip(1)
            .find(|dir| dir.join(manifest).is_file())
        {
            return Some(format!(
                "{manifest} is missing in the project root: `{command}` would use {} from a parent                  folder instead of this project. Write {manifest} at the project root.",
                parent.join(manifest).display()
            ));
        }
    }
    None
}

/// How much workspace content the Builder and Architect see (about 15k tokens of a 48k
/// context, leaving room for the spec, the last attempt, tool schemas and the answer).
const SNAPSHOT_BUDGET_CHARS: usize = 60_000;

pub trait Workbench: Send + Sync {
    /// The workspace and the rules for answers, shown to the Builder.
    fn briefing(&self) -> Result<String>;

    /// The project's current files, shown to the Architect so the specification builds on
    /// the existing work instead of starting over.
    fn snapshot(&self) -> Result<String>;

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
    /// Files with merge conflict markers left by bringing the branch up to date with main.
    conflicts: Vec<String>,
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
            conflicts: Vec::new(),
        }
    }

    pub fn with_conflicts(mut self, conflicts: Vec<String>) -> Self {
        self.conflicts = conflicts;
        self
    }

    pub fn workspace(&self) -> &Workspace {
        &self.workspace
    }
}

impl Workbench for TaskWorkbench {
    fn briefing(&self) -> Result<String> {
        let conflicts = if self.conflicts.is_empty() {
            String::new()
        } else {
            format!(
                "MERGE CONFLICTS: bringing this task up to date with the main branch left \
                 conflict markers (<<<<<<< ======= >>>>>>>) in: {}. Rewrite each of these files \
                 completely, combining both sides correctly, before anything else.\n\n",
                self.conflicts.join(", ")
            )
        };
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
            "{conflicts}WORKSPACE (the project's files as this task sees them):\n{}\n\n\
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

    fn snapshot(&self) -> Result<String> {
        self.workspace.snapshot(SNAPSHOT_BUDGET_CHARS)
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
            for path in &answer.expected {
                if !files_written.contains(path) && !self.workspace.root().join(path).exists() {
                    problems.push(format!(
                        "{path} was assigned in the work split but nobody delivered it and the \
                         project does not have it; write {path}"
                    ));
                }
            }

            if let Some(command) = &self.test_command
                && let Some(problem) = foreign_manifest(self.workspace.root(), command)
            {
                problems.push(problem);
            }

            let test = match (&self.test_command, problems.is_empty()) {
                (Some(command), true) => {
                    Some(run_tests(self.workspace.root(), command, self.test_timeout).await?)
                }
                _ => None,
            };

            let mut warnings = if problems.is_empty() {
                self.workspace.test_regressions()?
            } else {
                Vec::new()
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
            if problems.is_empty() && !self.workspace.differs_from_main()? {
                warnings.push(
                    "the task changed no files: the project already matched this answer; confirm                      the task was in fact already done"
                        .to_string(),
                );
            }

            Ok(Verification {
                files_written,
                files_deleted,
                problems,
                warnings,
                test,
                commit,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn assigned_files_nobody_delivered_are_problems() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).unwrap();
        std::fs::write(dir.path().join("existing.txt"), "kept from earlier work").unwrap();
        let bench = TaskWorkbench::new(workspace, "app-T01", None, Duration::from_secs(30));
        // What a lead writes when its specialist left out two of its owned files.
        let answer = "### FILE: src/lib.rs
```
pub fn f() {}
```

                      Platform note: EMP-RUST-001 did not deliver Cargo.toml, existing.txt
                      ### EXPECTED: Cargo.toml
### EXPECTED: existing.txt
";
        let verification = bench
            .verify(&Artifact {
                kind: crate::domain::ArtifactKind::Implementation,
                author: "EMP-BUILD-001".to_string(),
                revision: 1,
                content: answer.to_string(),
            })
            .await
            .unwrap();

        assert_eq!(verification.files_written, ["src/lib.rs"]);
        assert_eq!(
            verification.problems.len(),
            1,
            "{:?}",
            verification.problems
        );
        assert!(verification.problems[0].starts_with("Cargo.toml was assigned"));
        assert!(!verification.passed());
    }

    #[test]
    fn a_manifest_found_only_in_a_parent_folder_is_a_problem() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]").unwrap();
        let project = dir.path().join("data/workspaces/app");
        std::fs::create_dir_all(&project).unwrap();

        let problem = foreign_manifest(&project, "cargo test").unwrap();
        assert!(problem.contains("Cargo.toml is missing"), "{problem}");
        assert!(foreign_manifest(&project, "cd . && cargo.exe test --all").is_some());
        // Other tools, or a project with its own manifest, are fine.
        assert!(foreign_manifest(&project, "python -m unittest discover -s tests").is_none());
        std::fs::write(project.join("Cargo.toml"), "[package]").unwrap();
        assert!(foreign_manifest(&project, "cargo test").is_none());
        // No manifest anywhere: the tool's own error is clear enough.
        assert!(foreign_manifest(&project, "npm test").is_none());
    }
}
