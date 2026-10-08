//! Real execution: the Builder's answer becomes commits in the project's git repository, the
//! Owner's test command runs on them, and approved work is merged into `main` by git.
//!
//! Every project has one repository at `<data>/workspaces/<project>/`, created empty for a new
//! project. Each task works on its own branch `task/<task-id>` (tasks run one at a time, so a
//! single working tree is enough); every Builder iteration is a commit authored by the
//! employee; an approved task is merged into `main` with `--no-ff`. Pushing goes only to the
//! remote the Owner configured, never with force.

mod runner;

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use anyhow::{Context, Result, bail};

pub(crate) use runner::allowed_env;
pub use runner::{TestRun, run_tests};

/// Most files one Builder answer may write or delete.
pub const MAX_FILES_PER_ANSWER: usize = 60;
/// Largest file one Builder answer may write.
pub const MAX_FILE_BYTES: usize = 256 * 1024;
pub const MAIN_BRANCH: &str = "main";
const MAX_PATH_LEN: usize = 200;
/// Directories agents never write into and git never tracks (build output, caches, VCS).
const SKIP_DIRS: [&str; 11] = [
    ".git",
    "target",
    "node_modules",
    "__pycache__",
    ".venv",
    "venv",
    ".pytest_cache",
    ".mypy_cache",
    "dist",
    "build",
    ".ai-team",
];
const WINDOWS_RESERVED: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];
/// Identity for commits made by the platform itself (initial commit, merges, recovery).
pub const PLATFORM_AUTHOR: &str = "EMP-ORCH-001";
/// Identity for files the Owner hands to the team.
pub const OWNER_AUTHOR: &str = "OWNER";
/// Largest file the Owner can attach to a request.
pub const MAX_INPUT_BYTES: u64 = 10 * 1024 * 1024;

/// A file the Builder asked to write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileBlock {
    pub path: String,
    pub content: String,
}

/// What a Builder answer asks the platform to do to the workspace.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AnswerFiles {
    pub files: Vec<FileBlock>,
    pub deletions: Vec<String>,
    pub problems: Vec<String>,
    /// `### EXPECTED: <path>`: files a delegating lead assigned that its specialist did not
    /// deliver. The workbench reports those that are also absent from the workspace.
    pub expected: Vec<String>,
}

/// Parses `### FILE: <path>` headings (each followed by one fenced code block),
/// `### DELETE: <path>` headings and `### EXPECTED: <path>` notes from a delegating lead.
pub fn extract_files(markdown: &str) -> AnswerFiles {
    let lines: Vec<&str> = markdown.lines().collect();
    let mut answer = AnswerFiles::default();
    let mut i = 0;

    while i < lines.len() {
        if let Some(path) = heading(lines[i], "EXPECTED:") {
            if let Ok(path) = validate_path(&path)
                && !answer.expected.contains(&path)
            {
                answer.expected.push(path);
            }
            i += 1;
            continue;
        }
        if let Some(path) = heading(lines[i], "DELETE:") {
            if !answer.deletions.contains(&path) {
                answer.deletions.push(path);
            }
            i += 1;
            continue;
        }
        let Some(path) = heading(lines[i], "FILE:") else {
            i += 1;
            continue;
        };
        i += 1;
        while i < lines.len() && lines[i].trim().is_empty() {
            i += 1;
        }
        let Some(fence) = lines.get(i).and_then(|line| opening_fence(line)) else {
            answer.problems.push(format!(
                "FILE {path}: the heading must be followed by a fenced code block"
            ));
            continue;
        };
        i += 1;
        let start = i;
        while i < lines.len() && lines[i].trim() != fence {
            i += 1;
        }
        if i == lines.len() {
            answer.problems.push(format!(
                "FILE {path}: code block is not closed with {fence}"
            ));
            break;
        }
        let mut content = lines[start..i].join("\n");
        content.push('\n');
        i += 1;

        if answer.files.iter().any(|file| file.path == path) {
            answer
                .problems
                .push(format!("FILE {path}: written twice in one answer"));
            continue;
        }
        answer.files.push(FileBlock { path, content });
    }

    for path in &answer.deletions {
        if answer.files.iter().any(|file| &file.path == path) {
            answer
                .problems
                .push(format!("{path}: both written and deleted in one answer"));
        }
    }
    let total = answer.files.len() + answer.deletions.len();
    if total > MAX_FILES_PER_ANSWER {
        answer.problems.push(format!(
            "{total} files in one answer; the maximum is {MAX_FILES_PER_ANSWER}"
        ));
    }
    answer
}

/// `### FILE: src/lib.rs` (any heading level; backticks around the path allowed).
fn heading(line: &str, keyword: &str) -> Option<String> {
    let rest = line
        .trim()
        .strip_prefix('#')?
        .trim_start_matches('#')
        .trim();
    let path = rest.strip_prefix(keyword)?.trim().trim_matches('`').trim();
    (!path.is_empty()).then(|| path.to_string())
}

/// Returns the fence (three or more backticks) that opens a code block.
fn opening_fence(line: &str) -> Option<String> {
    let trimmed = line.trim();
    let ticks = trimmed.chars().take_while(|c| *c == '`').count();
    (ticks >= 3).then(|| "`".repeat(ticks))
}

/// Checks a Builder-chosen path and returns it relative, with `/` separators.
pub fn validate_path(raw: &str) -> Result<String> {
    let path = raw.trim().replace('\\', "/");
    anyhow::ensure!(!path.is_empty(), "empty path");
    anyhow::ensure!(
        path.len() <= MAX_PATH_LEN,
        "path longer than {MAX_PATH_LEN} characters"
    );
    anyhow::ensure!(
        !path.starts_with('/') && path.chars().nth(1) != Some(':'),
        "absolute paths are not allowed"
    );

    for component in path.split('/') {
        anyhow::ensure!(
            !component.is_empty() && component != "." && component != "..",
            "'.', '..' and empty path segments are not allowed"
        );
        anyhow::ensure!(
            !component
                .chars()
                .any(|c| c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*')),
            "invalid character in '{component}'"
        );
        anyhow::ensure!(
            !component.ends_with('.') && !component.ends_with(' '),
            "'{component}' ends with a dot or a space"
        );
        let stem = component
            .split('.')
            .next()
            .unwrap_or("")
            .to_ascii_uppercase();
        anyhow::ensure!(
            !WINDOWS_RESERVED.contains(&stem.as_str()),
            "'{component}' is a reserved device name"
        );
    }

    // Directories only (the last component is the file name), at any depth.
    if let Some(dir) = path
        .split('/')
        .rev()
        .skip(1)
        .find(|dir| SKIP_DIRS.contains(dir))
    {
        anyhow::bail!("writing into '{dir}/' is not allowed");
    }
    anyhow::ensure!(
        path != ".gitignore",
        "'.gitignore' is managed by the platform"
    );
    Ok(path)
}

/// Relative paths (with `/`) of every regular file under `root`, skipping [`SKIP_DIRS`].
fn list_files(root: &Path) -> Result<Vec<String>> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) -> Result<()> {
        for entry in fs::read_dir(dir).with_context(|| format!("cannot read {}", dir.display()))? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            let path = entry.path();
            let kind = entry.file_type()?;
            if kind.is_dir() {
                if !SKIP_DIRS.contains(&name.as_str()) {
                    walk(root, &path, out)?;
                }
            } else if kind.is_file() {
                let relative = path
                    .strip_prefix(root)
                    .expect("walked under root")
                    .to_string_lossy()
                    .replace('\\', "/");
                out.push(relative);
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    if root.is_dir() {
        walk(root, root, &mut out)?;
    }
    out.sort();
    Ok(out)
}

/// Test files by common conventions (tests/ folders, test_*.py, *_test.go, *.test.ts, ...).
fn is_test_path(path: &str) -> bool {
    let mut parts: Vec<&str> = path.split('/').collect();
    let name = parts.pop().unwrap_or("").to_ascii_lowercase();
    parts
        .iter()
        .any(|dir| matches!(*dir, "tests" | "test" | "__tests__" | "spec"))
        || name.starts_with("test_")
        || ["_test.", ".test.", ".spec."]
            .iter()
            .any(|marker| name.contains(marker) && !name.starts_with(marker))
}

/// Test cases in a file, by the definitions common frameworks use.
fn count_test_cases(source: &str) -> usize {
    const MARKERS: [&str; 9] = [
        "def test",
        "async def test",
        "#[test]",
        "#[tokio::test]",
        "func Test",
        "it(",
        "test(",
        "@Test",
        "it.each",
    ];
    source
        .lines()
        .map(str::trim_start)
        .filter(|line| MARKERS.iter().any(|marker| line.starts_with(marker)))
        .count()
}

pub fn task_branch(task_id: &str) -> String {
    format!("task/{task_id}")
}

/// A project's git repository and working tree.
#[derive(Debug, Clone)]
pub struct Workspace {
    root: PathBuf,
}

impl Workspace {
    /// Opens the project repository, creating an empty one (with an initial commit holding
    /// only the platform's `.gitignore`) for a new project.
    pub fn open(root: &Path) -> Result<Self> {
        let workspace = Self {
            root: root.to_path_buf(),
        };
        if !root.join(".git").exists() {
            fs::create_dir_all(root)
                .with_context(|| format!("cannot create {}", root.display()))?;
            workspace.git(&["init", "--quiet", "-b", MAIN_BRANCH])?;
            // Files stay byte-for-byte as the Builder wrote them.
            workspace.git(&["config", "core.autocrlf", "false"])?;
            let ignore: String = SKIP_DIRS
                .iter()
                .filter(|dir| **dir != ".git")
                // Unanchored: caches such as tests/__pycache__/ sit below the root too.
                .map(|dir| format!("{dir}/\n"))
                .collect();
            fs::write(root.join(".gitignore"), ignore)?;
            workspace.commit_paths(
                &[".gitignore".to_string()],
                PLATFORM_AUTHOR,
                "Initialize project workspace",
            )?;
        }
        Ok(workspace)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new("git");
        command
            .current_dir(&self.root)
            .args(args)
            // Fail instead of waiting for a password on a terminal nobody watches.
            .env("GIT_TERMINAL_PROMPT", "0");
        command
    }

    fn run(&self, args: &[&str]) -> Result<Output> {
        self.command(args)
            .output()
            .with_context(|| format!("cannot run git {}", args.join(" ")))
    }

    /// Runs git and fails with its stderr when it exits non-zero.
    fn git(&self, args: &[&str]) -> Result<String> {
        let output = self.run(args)?;
        if !output.status.success() {
            bail!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    fn author_args(author: &str) -> [String; 4] {
        [
            "-c".to_string(),
            format!("user.name={author}"),
            "-c".to_string(),
            format!("user.email={}@ai-team.local", author.to_lowercase()),
        ]
    }

    pub fn current_branch(&self) -> Result<String> {
        self.git(&["rev-parse", "--abbrev-ref", "HEAD"])
    }

    pub fn branch_exists(&self, branch: &str) -> Result<bool> {
        Ok(self
            .run(&[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ])?
            .status
            .success())
    }

    /// Commits whatever is left uncommitted (a run interrupted between writing and
    /// committing), so switching branches never loses work.
    fn save_uncommitted(&self) -> Result<()> {
        if !self.git(&["status", "--porcelain"])?.is_empty() {
            self.git(&["add", "--all"])?;
            let mut args: Vec<String> = Self::author_args(PLATFORM_AUTHOR).to_vec();
            args.extend(["commit", "--quiet", "-m", "Save uncommitted work"].map(String::from));
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            self.git(&args)?;
        }
        Ok(())
    }

    /// Checks out the task's branch, creating it from `main` the first time. An existing
    /// branch is brought up to date with `main` first, so a resumed task works on the current
    /// project; returns the files left with conflict markers for the Builder to resolve.
    pub fn start_task(&self, task_id: &str) -> Result<Vec<String>> {
        self.save_uncommitted()?;
        let branch = task_branch(task_id);
        if self.branch_exists(&branch)? {
            self.git(&["checkout", "--quiet", &branch])?;
            self.sync_with_main(&branch)
        } else {
            self.git(&["checkout", "--quiet", "-b", &branch, MAIN_BRANCH])?;
            Ok(Vec::new())
        }
    }

    /// Merges `main` into the checked-out task branch. Conflicts are committed with their
    /// markers (on the task branch only): resolving them becomes part of the task, and the
    /// tests and the Reviewer check the resolution before anything reaches `main`.
    fn sync_with_main(&self, branch: &str) -> Result<Vec<String>> {
        let mut args: Vec<String> = Self::author_args(PLATFORM_AUTHOR).to_vec();
        args.extend(
            [
                "merge",
                "--no-ff",
                "--quiet",
                "-m",
                &format!("Merge main into {branch}"),
                MAIN_BRANCH,
            ]
            .map(String::from),
        );
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let output = self.run(&args)?;
        if output.status.success() {
            return Ok(Vec::new());
        }

        let conflicts: Vec<String> = self
            .git(&["diff", "--name-only", "--diff-filter=U"])?
            .lines()
            .map(str::to_string)
            .collect();
        if conflicts.is_empty() {
            let _ = self.run(&["merge", "--abort"]);
            bail!(
                "cannot bring {branch} up to date with main: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        self.git(&["add", "--all"])?;
        let mut args: Vec<String> = Self::author_args(PLATFORM_AUTHOR).to_vec();
        args.extend(
            [
                "commit",
                "--quiet",
                "-m",
                &format!(
                    "Merge main into {branch} (conflicts left for the Builder: {})",
                    conflicts.join(", ")
                ),
            ]
            .map(String::from),
        );
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        self.git(&args)?;
        Ok(conflicts)
    }

    /// Copies the Owner's files into `inputs/` on `main` and commits them as the Owner.
    /// Returns the workspace paths.
    pub fn add_owner_inputs(&self, files: &[PathBuf]) -> Result<Vec<String>> {
        self.checkout_main()?;
        let mut paths = Vec::new();
        for file in files {
            let name = file
                .file_name()
                .and_then(|name| name.to_str())
                .with_context(|| format!("not a file name: {}", file.display()))?;
            let relative = validate_path(&format!("inputs/{name}"))
                .with_context(|| format!("unusable file name {name}"))?;
            let size = fs::metadata(file)
                .with_context(|| format!("cannot read {}", file.display()))?
                .len();
            anyhow::ensure!(
                size <= MAX_INPUT_BYTES,
                "{} is {size} bytes; the maximum for an input file is {MAX_INPUT_BYTES}",
                file.display()
            );
            let target = self.root.join(&relative);
            fs::create_dir_all(target.parent().expect("inputs/ has a parent"))?;
            fs::copy(file, &target).with_context(|| format!("cannot copy {}", file.display()))?;
            paths.push(relative);
        }
        self.commit_paths(&paths, OWNER_AUTHOR, "Owner inputs")?;
        Ok(paths)
    }

    pub fn checkout_main(&self) -> Result<()> {
        self.save_uncommitted()?;
        self.git(&["checkout", "--quiet", MAIN_BRANCH])?;
        Ok(())
    }

    pub fn files(&self) -> Result<Vec<String>> {
        list_files(&self.root)
    }

    /// Whether the working tree differs from `main`, i.e. the task changed anything at all.
    pub fn differs_from_main(&self) -> Result<bool> {
        let output = self.run(&["diff", "--quiet", MAIN_BRANCH, "--"])?;
        match output.status.code() {
            Some(0) => Ok(false),
            Some(1) => Ok(true),
            _ => bail!(
                "git diff failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        }
    }

    /// Tests approved on `main` that the working tree lost: removed test files, and test
    /// files with fewer test cases than before. A rewrite that quietly drops tests is how a
    /// follow-up task regressed approved work in a real run.
    pub fn test_regressions(&self) -> Result<Vec<String>> {
        let listing = self.git(&["ls-tree", "-r", "--name-only", MAIN_BRANCH])?;
        let mut findings = Vec::new();
        for path in listing.lines().filter(|path| is_test_path(path)) {
            let current = self.root.join(path);
            if !current.is_file() {
                findings.push(format!("test file {path} was removed"));
                continue;
            }
            let before = count_test_cases(&self.git(&["show", &format!("{MAIN_BRANCH}:{path}")])?);
            let after = count_test_cases(&fs::read_to_string(&current).unwrap_or_default());
            if after < before {
                findings.push(format!(
                    "{path} has {after} test case(s), main had {before}"
                ));
            }
        }
        Ok(findings)
    }

    /// Writes validated files; returns the paths written and the problems found. Nothing
    /// outside the workspace can be touched: every path is validated first.
    pub fn write_files(&self, files: &[FileBlock]) -> Result<(Vec<String>, Vec<String>)> {
        let mut written = Vec::new();
        let mut problems = Vec::new();
        for file in files {
            let path = match validate_path(&file.path) {
                Ok(path) => path,
                Err(err) => {
                    problems.push(format!("FILE {}: {err}", file.path));
                    continue;
                }
            };
            if file.content.len() > MAX_FILE_BYTES {
                problems.push(format!(
                    "FILE {path}: {} bytes; the maximum is {MAX_FILE_BYTES}",
                    file.content.len()
                ));
                continue;
            }
            let target = self.root.join(&path);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&target, &file.content)
                .with_context(|| format!("cannot write {}", target.display()))?;
            written.push(path);
        }
        Ok((written, problems))
    }

    /// Deletes validated files; returns the paths deleted and the problems found.
    pub fn delete_files(&self, paths: &[String]) -> Result<(Vec<String>, Vec<String>)> {
        let mut deleted = Vec::new();
        let mut problems = Vec::new();
        for raw in paths {
            let path = match validate_path(raw) {
                Ok(path) => path,
                Err(err) => {
                    problems.push(format!("DELETE {raw}: {err}"));
                    continue;
                }
            };
            let target = self.root.join(&path);
            if !target.is_file() {
                problems.push(format!("DELETE {path}: no such file"));
                continue;
            }
            fs::remove_file(&target)
                .with_context(|| format!("cannot delete {}", target.display()))?;
            deleted.push(path);
        }
        Ok((deleted, problems))
    }

    /// Commits exactly `paths` (written or deleted) as `author`. Returns the short commit id,
    /// or `None` when nothing changed.
    pub fn commit_paths(
        &self,
        paths: &[String],
        author: &str,
        message: &str,
    ) -> Result<Option<String>> {
        if paths.is_empty() {
            return Ok(None);
        }
        let mut add = vec!["add", "--all", "--"];
        add.extend(paths.iter().map(String::as_str));
        self.git(&add)?;
        if self.run(&["diff", "--cached", "--quiet"])?.status.success() {
            return Ok(None);
        }
        let mut args: Vec<String> = Self::author_args(author).to_vec();
        args.extend(["commit", "--quiet", "-m", message].map(String::from));
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        self.git(&args)?;
        Ok(Some(self.git(&["rev-parse", "--short", "HEAD"])?))
    }

    /// Merges the task branch into `main` (`--no-ff`, so the task stays visible in history).
    /// On a conflict the merge is aborted and `main` is left exactly as it was.
    pub fn merge_task(&self, task_id: &str, message: &str) -> Result<String> {
        let branch = task_branch(task_id);
        anyhow::ensure!(self.branch_exists(&branch)?, "no branch {branch}");
        self.checkout_main()?;

        let mut args: Vec<String> = Self::author_args(PLATFORM_AUTHOR).to_vec();
        args.extend(["merge", "--no-ff", "--quiet", "-m", message, &branch].map(String::from));
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let output = self.run(&args)?;
        if !output.status.success() {
            let conflicts = self
                .git(&["diff", "--name-only", "--diff-filter=U"])
                .unwrap_or_default()
                .replace('\n', ", ");
            let _ = self.run(&["merge", "--abort"]);
            bail!(
                "merge conflict between main and {branch}{}",
                if conflicts.is_empty() {
                    format!(": {}", String::from_utf8_lossy(&output.stderr).trim())
                } else {
                    format!(" in: {conflicts}")
                }
            );
        }
        self.git(&["rev-parse", "--short", "HEAD"])
    }

    /// Points `origin` at the Owner's remote (or removes it).
    pub fn set_remote(&self, url: Option<&str>) -> Result<()> {
        let current = self.run(&["remote", "get-url", "origin"])?;
        let current = current
            .status
            .success()
            .then(|| String::from_utf8_lossy(&current.stdout).trim().to_string());
        match (current.as_deref(), url) {
            (Some(existing), Some(url)) if existing == url => {}
            (Some(_), Some(url)) => {
                self.git(&["remote", "set-url", "origin", url])?;
            }
            (None, Some(url)) => {
                self.git(&["remote", "add", "origin", url])?;
            }
            (Some(_), None) => {
                self.git(&["remote", "remove", "origin"])?;
            }
            (None, None) => {}
        }
        Ok(())
    }

    /// Pushes one branch to `origin`. Never forces: a rejected push is reported, not overridden.
    pub fn push(&self, branch: &str) -> Result<()> {
        self.git(&["push", "--quiet", "origin", &format!("{branch}:{branch}")])?;
        Ok(())
    }

    /// One line per commit the task added on top of `main`.
    pub fn task_log(&self, task_id: &str) -> Result<Vec<String>> {
        let branch = task_branch(task_id);
        if !self.branch_exists(&branch)? {
            return Ok(Vec::new());
        }
        let log = self.git(&[
            "log",
            "--format=%h %an %s",
            &format!("{MAIN_BRANCH}..{branch}"),
        ])?;
        Ok(log.lines().map(str::to_string).collect())
    }

    /// What the Builder sees of the workspace: every file, with the contents of text files
    /// until `budget_chars` is spent.
    pub fn snapshot(&self, budget_chars: usize) -> Result<String> {
        let files: Vec<String> = self
            .files()?
            .into_iter()
            .filter(|file| file != ".gitignore")
            .collect();
        if files.is_empty() {
            return Ok("(empty workspace)".to_string());
        }
        let mut listing = String::new();
        let mut contents = String::new();
        let mut budget = budget_chars;
        let mut omitted = 0;
        for relative in &files {
            let bytes = fs::read(self.root.join(relative))?;
            listing.push_str(&format!("- {relative} ({} bytes)\n", bytes.len()));
            match String::from_utf8(bytes) {
                Ok(text) if text.len() <= budget => {
                    budget -= text.len();
                    // The byte length lets a reader skip the contents exactly, whatever they hold.
                    contents.push_str(&format!(
                        "\n{FILE_MARK}{relative} ({} bytes)\n```\n{text}\n```\n",
                        text.len()
                    ));
                }
                _ => omitted += 1,
            }
        }
        if omitted > 0 {
            contents.push_str(&format!(
                "\n({omitted} file(s) not shown: binary or over the context budget)\n"
            ));
        }
        Ok(format!("{listing}{contents}"))
    }
}

/// Starts one file's contents in a snapshot.
const FILE_MARK: &str = "#### FILE: ";

/// A snapshot (or a briefing containing one) with the contents of only the `keep` files:
/// the file listing and every text outside the contents stay. Each agent then pays only for
/// the files it works on; a specialist sees the whole project only by name.
pub fn filter_snapshot(text: &str, keep: &[String]) -> String {
    let opener = format!("\n{FILE_MARK}");
    let Some(first) = text.find(&opener) else {
        return text.to_string();
    };
    let mut out = String::with_capacity(text.len().min(64 * 1024));
    out.push_str(&text[..first]);
    let mut position = first;
    let mut dropped = 0;
    // Each block: "\n#### FILE: <path> (<n> bytes)\n```\n" + n bytes + "\n```\n".
    while text[position..].starts_with(&opener) {
        let header_start = position + opener.len();
        let Some(header_len) = text[header_start..].find('\n') else {
            break;
        };
        let header = &text[header_start..header_start + header_len];
        let Some((path, size)) = header
            .strip_suffix(" bytes)")
            .and_then(|rest| rest.rsplit_once(" ("))
            .and_then(|(path, size)| Some((path, size.parse::<usize>().ok()?)))
        else {
            break;
        };
        let body_start = header_start + header_len + 1 + "```\n".len();
        let block_end = body_start + size + "\n```\n".len();
        if block_end > text.len() || !text.is_char_boundary(block_end) {
            break;
        }
        if keep.iter().any(|kept| kept == path) {
            out.push_str(&text[position..block_end]);
        } else {
            dropped += 1;
        }
        position = block_end;
    }
    if dropped > 0 {
        out.push_str(&format!(
            "\n({dropped} file(s) listed above are not shown here: they are not yours to change)\n"
        ));
    }
    out.push_str(&text[position..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshots_can_keep_only_some_files() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).unwrap();
        fs::write(
            dir.path().join("a.py"),
            "A = 1
#### FILE: fake heading in content
",
        )
        .unwrap();
        fs::write(
            dir.path().join("b.md"),
            "# B
#### Title
",
        )
        .unwrap();
        let briefing = format!(
            "WORKSPACE:
{}
HOW YOUR ANSWER IS APPLIED: rules",
            workspace.snapshot(10_000).unwrap()
        );

        let only_b = filter_snapshot(&briefing, &["b.md".to_string()]);
        assert!(
            only_b.contains("- a.py (") && only_b.contains("- b.md ("),
            "{only_b}"
        );
        assert!(
            only_b.contains("#### Title") && !only_b.contains("A = 1"),
            "{only_b}"
        );
        assert!(
            only_b.contains("1 file(s) listed above are not shown"),
            "{only_b}"
        );
        assert!(
            only_b.ends_with("HOW YOUR ANSWER IS APPLIED: rules"),
            "{only_b}"
        );

        let names = filter_snapshot(&briefing, &[]);
        assert!(
            !names.contains("# B") && names.contains("- b.md ("),
            "{names}"
        );
        assert_eq!(
            filter_snapshot("(empty workspace)", &[]),
            "(empty workspace)"
        );
    }

    fn block(path: &str, content: &str) -> FileBlock {
        FileBlock {
            path: path.into(),
            content: content.into(),
        }
    }

    #[test]
    fn extracts_files_and_deletions() {
        let answer = "Intro\n\n### FILE: src/lib.rs\n```rust\npub fn a() {}\n```\n\n## FILE: `docs/readme.md`\n````markdown\n# Title\n```\ncode\n```\n````\n### DELETE: old.txt\n";
        let parsed = extract_files(answer);
        assert!(parsed.problems.is_empty(), "{:?}", parsed.problems);
        assert_eq!(parsed.files.len(), 2);
        assert_eq!(parsed.files[0].content, "pub fn a() {}\n");
        assert!(
            parsed.files[1].content.contains("```\ncode\n```"),
            "inner fences kept"
        );
        assert_eq!(parsed.deletions, ["old.txt"]);
    }

    #[test]
    fn reports_malformed_answers() {
        let parsed =
            extract_files("### FILE: a.txt\nno fence here\n### FILE: b.txt\n```\nunclosed");
        assert!(parsed.files.is_empty());
        assert!(parsed.problems[0].contains("a.txt"));
        assert!(parsed.problems[1].contains("not closed"));

        let both = extract_files("### FILE: x.txt\n```\n1\n```\n### DELETE: x.txt\n");
        assert!(both.problems[0].contains("both written and deleted"));
    }

    #[test]
    fn rejects_paths_that_escape_or_misbehave() {
        for bad in [
            "../etc/passwd",
            "/abs/path",
            "C:/Windows/x",
            "src/../../x",
            ".git/config",
            "target/debug/x",
            "con.txt",
            "a/b:c",
            "dir./x",
            "",
            ".gitignore",
            "tests/__pycache__/t.pyc",
        ] {
            assert!(validate_path(bad).is_err(), "{bad:?} must be rejected");
        }
        assert_eq!(validate_path("src\\lib.rs").unwrap(), "src/lib.rs");
        // A file merely named like a skipped directory is fine.
        assert_eq!(validate_path("docs/build").unwrap(), "docs/build");
    }

    #[test]
    fn recognises_test_files_and_counts_their_cases() {
        for path in [
            "tests/test_a.py",
            "test_a.py",
            "pkg/a_test.go",
            "src/a.test.ts",
            "tests/data.json",
        ] {
            assert!(is_test_path(path), "{path}");
        }
        for path in ["ulise.py", "src/lib.rs", "contest.py", "docs/testing.md"] {
            assert!(!is_test_path(path), "{path}");
        }
        let python = "class T:\n    def test_a(self):\n        pass\n    def test_b(self):\n        pass\n    def helper(self):\n        pass\n";
        assert_eq!(count_test_cases(python), 2);
        assert_eq!(
            count_test_cases("#[test]\nfn a() {}\n#[tokio::test]\nasync fn b() {}"),
            2
        );
    }

    #[test]
    fn dropped_tests_are_reported_against_main() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).unwrap();
        fs::create_dir_all(dir.path().join("tests")).unwrap();
        fs::write(
            dir.path().join("tests/test_a.py"),
            "def test_one():\n    pass\ndef test_two():\n    pass\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("tests/test_b.py"),
            "def test_b():\n    pass\n",
        )
        .unwrap();
        workspace
            .commit_paths(
                &["tests/test_a.py".to_string(), "tests/test_b.py".to_string()],
                "EMP-BUILD-001",
                "tests",
            )
            .unwrap();
        assert!(workspace.test_regressions().unwrap().is_empty());
        assert!(!workspace.differs_from_main().unwrap());

        fs::write(
            dir.path().join("tests/test_a.py"),
            "def test_one():\n    pass\n",
        )
        .unwrap();
        fs::remove_file(dir.path().join("tests/test_b.py")).unwrap();
        assert!(workspace.differs_from_main().unwrap());
        let findings = workspace.test_regressions().unwrap();
        assert_eq!(
            findings,
            [
                "tests/test_a.py has 1 test case(s), main had 2",
                "test file tests/test_b.py was removed"
            ]
        );
    }

    #[test]
    fn tasks_work_on_branches_and_merge_into_main() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).unwrap();
        assert_eq!(workspace.current_branch().unwrap(), "main");
        assert!(dir.path().join(".gitignore").is_file());

        workspace.start_task("p-T01").unwrap();
        assert_eq!(workspace.current_branch().unwrap(), "task/p-T01");
        let (written, problems) = workspace.write_files(&[block("a.txt", "one\n")]).unwrap();
        assert!(problems.is_empty());
        let commit = workspace
            .commit_paths(&written, "EMP-BUILD-001", "p-T01 iteration 1")
            .unwrap();
        assert!(commit.is_some());
        assert_eq!(workspace.task_log("p-T01").unwrap().len(), 1);
        assert!(workspace.task_log("p-T01").unwrap()[0].contains("EMP-BUILD-001"));

        // Nothing new to commit.
        assert!(
            workspace
                .commit_paths(&written, "EMP-BUILD-001", "again")
                .unwrap()
                .is_none()
        );

        workspace.merge_task("p-T01", "Merge p-T01").unwrap();
        assert_eq!(workspace.current_branch().unwrap(), "main");
        assert_eq!(
            fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "one\n"
        );
        assert!(
            workspace.task_log("p-T01").unwrap().is_empty(),
            "fully merged"
        );
    }

    #[test]
    fn deletions_are_committed() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).unwrap();
        workspace.start_task("p-T01").unwrap();
        let (written, _) = workspace.write_files(&[block("old.txt", "x\n")]).unwrap();
        workspace
            .commit_paths(&written, "EMP-BUILD-001", "add")
            .unwrap();

        let (deleted, problems) = workspace.delete_files(&["old.txt".to_string()]).unwrap();
        assert!(problems.is_empty());
        assert!(
            workspace
                .commit_paths(&deleted, "EMP-BUILD-001", "remove")
                .unwrap()
                .is_some()
        );
        assert!(!dir.path().join("old.txt").exists());

        let (_, problems) = workspace
            .delete_files(&["missing.txt".to_string()])
            .unwrap();
        assert!(problems[0].contains("no such file"));
    }

    #[test]
    fn conflicting_merge_is_aborted_and_main_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).unwrap();

        workspace.start_task("p-T01").unwrap();
        let (w, _) = workspace
            .write_files(&[block("shared.txt", "from T01\n")])
            .unwrap();
        workspace.commit_paths(&w, "EMP-BUILD-001", "T01").unwrap();

        workspace.checkout_main().unwrap();
        workspace.start_task("p-T02").unwrap();
        let (w, _) = workspace
            .write_files(&[block("shared.txt", "from T02\n")])
            .unwrap();
        workspace.commit_paths(&w, "EMP-BUILD-001", "T02").unwrap();

        workspace.merge_task("p-T02", "Merge p-T02").unwrap();
        let err = workspace
            .merge_task("p-T01", "Merge p-T01")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("merge conflict") && err.contains("shared.txt"),
            "{err}"
        );
        assert_eq!(workspace.current_branch().unwrap(), "main");
        assert_eq!(
            fs::read_to_string(dir.path().join("shared.txt")).unwrap(),
            "from T02\n"
        );
        assert!(
            workspace
                .git(&["status", "--porcelain"])
                .unwrap()
                .is_empty(),
            "clean"
        );
    }

    #[test]
    fn resumed_task_is_synced_with_main_and_gets_conflicts_to_resolve() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).unwrap();

        workspace.start_task("p-T01").unwrap();
        let (w, _) = workspace
            .write_files(&[block(
                "shared.txt",
                "T01
",
            )])
            .unwrap();
        workspace.commit_paths(&w, "EMP-BUILD-001", "T01").unwrap();

        // Meanwhile another task changed the same file and was merged.
        workspace.checkout_main().unwrap();
        workspace.start_task("p-T02").unwrap();
        let (w, _) = workspace
            .write_files(&[
                block(
                    "shared.txt",
                    "T02
",
                ),
                block(
                    "other.txt",
                    "x
",
                ),
            ])
            .unwrap();
        workspace.commit_paths(&w, "EMP-BUILD-001", "T02").unwrap();
        workspace.merge_task("p-T02", "Merge p-T02").unwrap();

        let conflicts = workspace.start_task("p-T01").unwrap();
        assert_eq!(conflicts, ["shared.txt"]);
        assert_eq!(workspace.current_branch().unwrap(), "task/p-T01");
        let text = fs::read_to_string(dir.path().join("shared.txt")).unwrap();
        assert!(text.contains("<<<<<<<") && text.contains("T01") && text.contains("T02"));
        assert!(
            dir.path().join("other.txt").is_file(),
            "main's other work arrived"
        );
        assert!(
            workspace
                .git(&["status", "--porcelain"])
                .unwrap()
                .is_empty(),
            "committed"
        );

        // The Builder resolves the file; the final merge into main is now clean.
        let (w, _) = workspace
            .write_files(&[block(
                "shared.txt",
                "T01+T02
",
            )])
            .unwrap();
        workspace
            .commit_paths(&w, "EMP-BUILD-001", "resolve")
            .unwrap();
        workspace.merge_task("p-T01", "Merge p-T01").unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("shared.txt")).unwrap(),
            "T01+T02
"
        );

        // An up-to-date branch syncs without conflicts.
        workspace.start_task("p-T02").unwrap();
        assert!(workspace.start_task("p-T02").unwrap().is_empty());
    }

    #[test]
    fn different_files_merge_even_after_main_moved() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).unwrap();
        for (task, file) in [("p-T01", "a.txt"), ("p-T02", "b.txt")] {
            workspace.checkout_main().unwrap();
            workspace.start_task(task).unwrap();
            let (w, _) = workspace.write_files(&[block(file, "x\n")]).unwrap();
            workspace.commit_paths(&w, "EMP-BUILD-001", task).unwrap();
        }
        workspace.merge_task("p-T02", "Merge p-T02").unwrap();
        workspace.merge_task("p-T01", "Merge p-T01").unwrap();
        assert!(dir.path().join("a.txt").is_file() && dir.path().join("b.txt").is_file());
    }

    #[test]
    fn pushes_to_the_configured_remote_only() {
        let dir = tempfile::tempdir().unwrap();
        let remote = dir.path().join("remote.git");
        Command::new("git")
            .args(["init", "--bare", "--quiet"])
            .arg(&remote)
            .status()
            .unwrap();

        let workspace = Workspace::open(&dir.path().join("work")).unwrap();
        assert!(workspace.push("main").is_err(), "no remote configured");

        workspace
            .set_remote(Some(remote.to_str().unwrap()))
            .unwrap();
        workspace.push("main").unwrap();
        let pushed = Command::new("git")
            .arg("-C")
            .arg(&remote)
            .args(["rev-parse", "--verify", "main"])
            .output()
            .unwrap();
        assert!(pushed.status.success(), "main arrived on the remote");

        workspace.set_remote(None).unwrap();
        assert!(workspace.push("main").is_err());
    }
}
