use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{llm::UsageTotals, workspace::TestRun};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectConfig {
    pub project_id: String,
    pub name: String,
    pub objective: String,
    pub max_iterations: u32,
    /// How many times the Reviewer may send the specification back to the Architect.
    #[serde(default = "default_max_architecture_revisions")]
    pub max_architecture_revisions: u32,
    #[serde(default)]
    pub rules: Vec<String>,
    #[serde(default)]
    pub acceptance_criteria: Vec<String>,
    /// Team chosen by the Owner; when absent the Planner proposes one.
    #[serde(default)]
    pub assigned_team: Option<TeamAssignment>,
    /// Owner-chosen command run in a task workspace after every Builder answer
    /// (e.g. `cargo test`, `python -m unittest`). Agents can never change it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_command: Option<String>,
    #[serde(default = "default_test_timeout_secs")]
    pub test_timeout_secs: u64,
    /// Git remote the platform pushes `main` and task branches to (Owner-chosen).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_url: Option<String>,
    /// Owner limits on model usage; checked before each task starts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<Budget>,
}

/// What a project may spend on models. It is checked before each task: a task that has
/// started finishes (its iteration budget bounds it), and no new task starts once a limit
/// is reached. Prices exist only when contracts set `cost_per_mtok_*`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Budget {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cost_usd: Option<f64>,
}

impl Budget {
    pub fn is_empty(&self) -> bool {
        self.max_tokens.is_none() && self.max_cost_usd.is_none()
    }

    /// Why no new task may start, if a limit is reached.
    pub fn exceeded(&self, usage: &crate::llm::UsageTotals) -> Option<String> {
        let tokens = usage.input_tokens + usage.output_tokens;
        if let Some(max) = self.max_tokens
            && tokens >= max
        {
            return Some(format!(
                "token budget reached: {tokens} of {max} tokens used"
            ));
        }
        if let Some(max) = self.max_cost_usd
            && usage.cost_usd >= max
        {
            return Some(format!(
                "cost budget reached: ${:.4} of ${max:.4} used",
                usage.cost_usd
            ));
        }
        None
    }
}

impl std::fmt::Display for Budget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (self.max_tokens, self.max_cost_usd) {
            (None, None) => write!(f, "none"),
            (Some(tokens), None) => write!(f, "{tokens} tokens"),
            (None, Some(cost)) => write!(f, "${cost:.2}"),
            (Some(tokens), Some(cost)) => write!(f, "{tokens} tokens, ${cost:.2}"),
        }
    }
}

fn default_test_timeout_secs() -> u64 {
    300
}

/// What the platform observed after applying an implementation: files written, problems
/// with the answer, and the test run. Produced by the platform, never by an agent.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Verification {
    pub files_written: Vec<String>,
    #[serde(default)]
    pub files_deleted: Vec<String>,
    pub problems: Vec<String>,
    /// Findings that do not block the work but that the Reviewer must weigh, e.g. tests that
    /// existed on the main branch and are gone.
    #[serde(default)]
    pub warnings: Vec<String>,
    pub test: Option<TestRun>,
    /// The commit that recorded this iteration on the task branch.
    #[serde(default)]
    pub commit: Option<String>,
}

impl Verification {
    pub fn passed(&self) -> bool {
        self.problems.is_empty() && self.test.as_ref().is_none_or(|test| test.passed)
    }

    /// Plain-text report for prompts and the CLI.
    pub fn report(&self) -> String {
        let mut report = format!(
            "files written: {}\n",
            if self.files_written.is_empty() {
                "none".to_string()
            } else {
                self.files_written.join(", ")
            }
        );
        if !self.files_deleted.is_empty() {
            report.push_str(&format!(
                "files deleted: {}\n",
                self.files_deleted.join(", ")
            ));
        }
        if let Some(commit) = &self.commit {
            report.push_str(&format!("commit: {commit}\n"));
        }
        for problem in &self.problems {
            report.push_str(&format!("problem: {problem}\n"));
        }
        for warning in &self.warnings {
            report.push_str(&format!(
                "WARNING: {warning} (approve only if the task asked for this)\n"
            ));
        }
        match &self.test {
            None if self.problems.is_empty() => {
                report.push_str("tests: no test command configured\n")
            }
            None => report.push_str("tests: not run (fix the problems above first)\n"),
            Some(test) => {
                let result = if test.timed_out {
                    "TIMED OUT".to_string()
                } else if test.passed {
                    "PASSED".to_string()
                } else {
                    format!(
                        "FAILED (exit code {})",
                        test.exit_code
                            .map_or("none".to_string(), |code| code.to_string())
                    )
                };
                report.push_str(&format!(
                    "tests: `{}` -> {result} in {} s\noutput:\n{}\n",
                    test.command, test.duration_secs, test.output
                ));
                if !test.passed
                    && let Some(hint) = diagnose(&test.output)
                {
                    report.push_str(&format!("diagnosis: {hint}\n"));
                }
            }
        }
        report
    }
}

/// Plain-language causes for test-runner failures a model tends to misread.
fn diagnose(output: &str) -> Option<&'static str> {
    if output.contains("NO TESTS RAN") || output.contains("Ran 0 tests") {
        Some(
            "unittest found no tests. Test files must be named test*.py (for example \
             tests/test_report.py, not tests/report_test.py), test classes must subclass \
             unittest.TestCase and test methods must start with test_.",
        )
    } else if output.contains("collected 0 items") || output.contains("no tests ran") {
        Some(
            "pytest found no tests. Test files must be named test_*.py or *_test.py and test \
             functions must start with test_.",
        )
    } else if output.contains("ModuleNotFoundError") || output.contains("ImportError") {
        Some(
            "the tests cannot import the code. Check module and package names, that package \
             folders have an __init__.py, and that imports match the file layout.",
        )
    } else {
        None
    }
}

/// Which employee leads each department for one run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TeamAssignment {
    pub architect: String,
    pub builder: String,
    pub reviewer: String,
}

fn default_max_architecture_revisions() -> u32 {
    1
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Planner,
    Architect,
    Builder,
    Reviewer,
    /// A subagent working for a department lead.
    Specialist,
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Role::Planner => "Planner",
            Role::Architect => "Architect",
            Role::Builder => "Builder",
            Role::Reviewer => "Reviewer",
            Role::Specialist => "Specialist",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    New,
    ArchitectureReady,
    Implemented,
    Reviewing,
    ChangesRequired,
    ArchitectureRevisionRequired,
    Approved,
    HumanReviewRequired,
    Done,
    Failed,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReviewDecision {
    Approved,
    ChangesRequired,
}

/// Who has to act on a `CHANGES_REQUIRED` review.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReviewTarget {
    #[default]
    Builder,
    Architect,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewResult {
    pub decision: ReviewDecision,
    #[serde(default)]
    pub target: ReviewTarget,
    pub feedback: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Specification,
    Implementation,
}

/// A versioned work product handed between department leads.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Artifact {
    pub kind: ArtifactKind,
    pub author: String,
    pub revision: u32,
    pub content: String,
}

/// Everything the orchestrator gives an agent for one step. The same shape is used for
/// every role, so a department lead can forward it unchanged to its subagents.
#[derive(Debug, Clone, Serialize)]
pub struct WorkOrder {
    pub project: ProjectConfig,
    pub task: String,
    pub iteration: u32,
    /// Current specification; for the Architect, `Some` means "revise this".
    pub specification: Option<Artifact>,
    /// Builder: the previous iteration's implementation. Reviewer: the candidate under review.
    pub implementation: Option<Artifact>,
    /// The most recent review, if any.
    pub review: Option<ReviewResult>,
    /// Builder only, when work is applied to a workspace: its files and the answer rules.
    pub workspace: Option<String>,
    /// The latest platform verification of `implementation`, if any.
    pub verification: Option<Verification>,
}

#[derive(Debug, Clone, Serialize)]
pub enum AgentOutput {
    Artifact(Artifact),
    Review(ReviewResult),
}

impl AgentOutput {
    pub fn describe(&self) -> String {
        match self {
            AgentOutput::Artifact(artifact) => format!("{:?} artifact", artifact.kind),
            AgentOutput::Review(_) => "a review".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamState {
    pub run_id: String,
    pub project_id: String,
    pub task_id: String,
    pub task: String,
    pub stage: Stage,
    pub iteration: u32,
    pub architecture_revisions: u32,
    pub team: Option<TeamAssignment>,
    pub specification: Option<Artifact>,
    pub implementation: Option<Artifact>,
    pub review: Option<ReviewResult>,
    pub history: Vec<String>,
    pub audit_file: String,
    pub error: Option<String>,
    #[serde(default)]
    pub verification: Option<Verification>,
    /// Every workspace file the Builder wrote during this task (what gets merged).
    #[serde(default)]
    pub written_files: Vec<String>,
    /// Model calls made for this task.
    #[serde(default)]
    pub usage: UsageTotals,
}

impl TeamState {
    pub fn new(
        run_id: String,
        project_id: String,
        task_id: String,
        task: String,
        audit_file: String,
    ) -> Self {
        Self {
            run_id,
            project_id,
            task_id,
            task,
            stage: Stage::New,
            iteration: 0,
            architecture_revisions: 0,
            team: None,
            specification: None,
            implementation: None,
            review: None,
            history: vec!["orchestrator: run created".to_string()],
            audit_file,
            error: None,
            verification: None,
            written_files: Vec::new(),
            usage: UsageTotals::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failed_run(output: &str) -> Verification {
        Verification {
            files_written: vec!["tests/report_test.py".to_string()],
            test: Some(TestRun {
                command: "python -m unittest discover -s tests".to_string(),
                exit_code: Some(5),
                timed_out: false,
                passed: false,
                duration_secs: 0,
                output: output.to_string(),
            }),
            ..Verification::default()
        }
    }

    #[test]
    fn failed_tests_get_a_diagnosis_when_the_cause_is_known() {
        let report = failed_run("Ran 0 tests in 0.000s\n\nNO TESTS RAN").report();
        assert!(
            report.contains("diagnosis: unittest found no tests"),
            "{report}"
        );

        let report = failed_run("ModuleNotFoundError: No module named 'raport'").report();
        assert!(report.contains("cannot import the code"), "{report}");

        let report = failed_run("AssertionError: 2 != 3").report();
        assert!(
            !report.contains("diagnosis"),
            "no guessing for ordinary failures"
        );
    }
}
