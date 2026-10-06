//! Real execution (phase 4): the Builder's answer becomes files in a per-task workspace, the
//! Owner's test command runs there, and approved work is merged into the project's main
//! workspace only when nothing changed underneath it.
//!
//! Layout under `<data>/workspaces/<project>/`: `main/` holds the integrated project, and
//! `tasks/<task-id>/` is a copy of `main/` taken when the task started, plus a manifest of the
//! hashes it was copied from (used to detect merge conflicts).

mod runner;

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

pub use runner::{TestRun, run_tests};

/// Most files one Builder answer may write.
pub const MAX_FILES_PER_ANSWER: usize = 60;
/// Largest file one Builder answer may write.
pub const MAX_FILE_BYTES: usize = 256 * 1024;
const MAX_PATH_LEN: usize = 200;
/// Directories never copied, merged or written by agents (build output, caches, VCS).
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
const BASE_MANIFEST: &str = ".ai-team-base.json";
const WINDOWS_RESERVED: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// A file the Builder asked to write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileBlock {
    pub path: String,
    pub content: String,
}

/// Finds every `### FILE: <path>` heading followed by a fenced code block. Returns the
/// blocks and the problems found (unterminated blocks, duplicates, too many files).
pub fn extract_files(markdown: &str) -> (Vec<FileBlock>, Vec<String>) {
    let lines: Vec<&str> = markdown.lines().collect();
    let mut files: Vec<FileBlock> = Vec::new();
    let mut problems = Vec::new();
    let mut i = 0;

    while i < lines.len() {
        let Some(path) = file_heading(lines[i]) else {
            i += 1;
            continue;
        };
        i += 1;
        while i < lines.len() && lines[i].trim().is_empty() {
            i += 1;
        }
        let Some(fence) = lines.get(i).and_then(|line| opening_fence(line)) else {
            problems.push(format!(
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
            problems.push(format!(
                "FILE {path}: code block is not closed with {fence}"
            ));
            break;
        }
        let mut content = lines[start..i].join("\n");
        content.push('\n');
        i += 1;

        if files.iter().any(|file| file.path == path) {
            problems.push(format!("FILE {path}: written twice in one answer"));
            continue;
        }
        files.push(FileBlock { path, content });
    }

    if files.len() > MAX_FILES_PER_ANSWER {
        problems.push(format!(
            "{} files in one answer; the maximum is {MAX_FILES_PER_ANSWER}",
            files.len()
        ));
    }
    (files, problems)
}

/// `### FILE: src/lib.rs` (any heading level; backticks around the path allowed).
fn file_heading(line: &str) -> Option<String> {
    let rest = line
        .trim()
        .strip_prefix('#')?
        .trim_start_matches('#')
        .trim();
    let path = rest.strip_prefix("FILE:")?.trim().trim_matches('`').trim();
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

    let first = path.split('/').next().unwrap_or("");
    anyhow::ensure!(
        !SKIP_DIRS.contains(&first),
        "writing into '{first}/' is not allowed"
    );
    anyhow::ensure!(path != BASE_MANIFEST, "'{BASE_MANIFEST}' is reserved");
    Ok(path)
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn file_hash(path: &Path) -> Result<Option<String>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(sha256_hex(&bytes))),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("cannot read {}", path.display())),
    }
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
            } else if kind.is_file() && name != BASE_MANIFEST {
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

/// The workspace of one task.
#[derive(Debug, Clone)]
pub struct Workspace {
    root: PathBuf,
}

impl Workspace {
    /// Reuses the task workspace if it exists (resumed task); otherwise copies `main` into it
    /// and records the hashes it was copied from.
    pub fn prepare(main: &Path, task_dir: &Path) -> Result<Self> {
        fs::create_dir_all(main).with_context(|| format!("cannot create {}", main.display()))?;
        if !task_dir.join(BASE_MANIFEST).is_file() {
            fs::create_dir_all(task_dir)
                .with_context(|| format!("cannot create {}", task_dir.display()))?;
            let mut base = BTreeMap::new();
            for relative in list_files(main)? {
                let source = main.join(&relative);
                let target = task_dir.join(&relative);
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::copy(&source, &target)
                    .with_context(|| format!("cannot copy {}", source.display()))?;
                base.insert(relative, sha256_hex(&fs::read(&source)?));
            }
            fs::write(
                task_dir.join(BASE_MANIFEST),
                serde_json::to_string_pretty(&base)?,
            )?;
        }
        Ok(Self {
            root: task_dir.to_path_buf(),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn files(&self) -> Result<Vec<String>> {
        list_files(&self.root)
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

    /// What the Builder sees of the workspace: every file, with the contents of text files
    /// until `budget_chars` is spent.
    pub fn snapshot(&self, budget_chars: usize) -> Result<String> {
        let files = self.files()?;
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
                    contents.push_str(&format!("\n#### {relative}\n```\n{text}\n```\n"));
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

    /// Copies `files` into `main`, unless `main` changed any of them since this task's copy
    /// was taken (another task merged first). All or nothing.
    pub fn merge_into(&self, main: &Path, files: &[String]) -> Result<Vec<String>> {
        let base: BTreeMap<String, String> = serde_json::from_str(
            &fs::read_to_string(self.root.join(BASE_MANIFEST))
                .context("task workspace has no base manifest")?,
        )?;

        let mut conflicts = Vec::new();
        for relative in files {
            let current = file_hash(&main.join(relative))?;
            if current.as_ref() != base.get(relative) {
                conflicts.push(relative.clone());
            }
        }
        if !conflicts.is_empty() {
            bail!(
                "merge conflict: the main workspace changed since this task started: {}",
                conflicts.join(", ")
            );
        }

        for relative in files {
            let source = self.root.join(relative);
            let target = main.join(relative);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(&source, &target)
                .with_context(|| format!("cannot merge {}", source.display()))?;
        }
        Ok(files.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_file_blocks_with_any_fence_length() {
        let answer = "Intro text\n\n### FILE: src/lib.rs\n```rust\npub fn a() {}\n```\n\n## FILE: `docs/readme.md`\n````markdown\n# Title\n```\ncode\n```\n````\n";
        let (files, problems) = extract_files(answer);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, "src/lib.rs");
        assert_eq!(files[0].content, "pub fn a() {}\n");
        assert_eq!(files[1].path, "docs/readme.md");
        assert!(
            files[1].content.contains("```\ncode\n```"),
            "inner fences kept"
        );
    }

    #[test]
    fn reports_malformed_blocks() {
        let (files, problems) =
            extract_files("### FILE: a.txt\nno fence here\n### FILE: b.txt\n```\nunclosed");
        assert!(files.is_empty());
        assert!(problems[0].contains("a.txt"));
        assert!(problems[1].contains("not closed"));
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
            BASE_MANIFEST,
        ] {
            assert!(validate_path(bad).is_err(), "{bad:?} must be rejected");
        }
        assert_eq!(validate_path("src\\lib.rs").unwrap(), "src/lib.rs");
        assert_eq!(validate_path("tests/test_a.py").unwrap(), "tests/test_a.py");
    }

    #[test]
    fn merge_detects_changes_under_the_task() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("main");
        fs::create_dir_all(&main).unwrap();
        fs::write(main.join("shared.txt"), "v1").unwrap();

        let workspace = Workspace::prepare(&main, &dir.path().join("t1")).unwrap();
        let (written, problems) = workspace
            .write_files(&[
                FileBlock {
                    path: "shared.txt".into(),
                    content: "task".into(),
                },
                FileBlock {
                    path: "new.txt".into(),
                    content: "new".into(),
                },
            ])
            .unwrap();
        assert!(problems.is_empty());

        // Another task changed shared.txt in main meanwhile.
        fs::write(main.join("shared.txt"), "v2").unwrap();
        let err = workspace
            .merge_into(&main, &written)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("shared.txt") && !err.contains("new.txt"),
            "{err}"
        );
        assert!(!main.join("new.txt").exists(), "all or nothing");

        fs::write(main.join("shared.txt"), "v1").unwrap();
        workspace.merge_into(&main, &written).unwrap();
        assert_eq!(fs::read_to_string(main.join("shared.txt")).unwrap(), "task");
        assert_eq!(fs::read_to_string(main.join("new.txt")).unwrap(), "new");
    }

    #[test]
    fn prepare_copies_main_once_and_skips_build_output() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("main");
        fs::create_dir_all(main.join("target")).unwrap();
        fs::write(main.join("target/big.bin"), "x").unwrap();
        fs::write(main.join("a.txt"), "a").unwrap();

        let task = dir.path().join("t1");
        let workspace = Workspace::prepare(&main, &task).unwrap();
        assert_eq!(workspace.files().unwrap(), ["a.txt"]);

        // A resumed task keeps its own edits.
        fs::write(task.join("a.txt"), "edited").unwrap();
        Workspace::prepare(&main, &task).unwrap();
        assert_eq!(fs::read_to_string(task.join("a.txt")).unwrap(), "edited");
    }
}
