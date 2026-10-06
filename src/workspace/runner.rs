//! Runs the Owner's test command in a task workspace. Agents never choose the command.
//! The child gets a minimal environment (no API keys or other secrets from this process) and
//! a timeout; on timeout the whole process tree is killed.

use std::{path::Path, process::Stdio, time::Duration};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::process::Command;

/// How much of the combined output is kept (the end, where failures are reported).
const OUTPUT_TAIL_CHARS: usize = 8000;

/// Variables passed through to tests; everything else is dropped.
const ALLOWED_ENV: [&str; 25] = [
    "PATH",
    "PATHEXT",
    "SYSTEMROOT",
    "SYSTEMDRIVE",
    "WINDIR",
    "COMSPEC",
    "TEMP",
    "TMP",
    "TMPDIR",
    "HOME",
    "USERPROFILE",
    "HOMEDRIVE",
    "HOMEPATH",
    "APPDATA",
    "LOCALAPPDATA",
    "PROGRAMDATA",
    "PROGRAMFILES",
    "PROGRAMFILES(X86)",
    "NUMBER_OF_PROCESSORS",
    "PROCESSOR_ARCHITECTURE",
    "OS",
    "LANG",
    "CARGO_HOME",
    "RUSTUP_HOME",
    "RUSTUP_TOOLCHAIN",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestRun {
    pub command: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub passed: bool,
    pub duration_secs: u64,
    /// The last part of stdout + stderr.
    pub output: String,
}

fn allowed_env() -> Vec<(String, String)> {
    std::env::vars()
        .filter(|(name, _)| ALLOWED_ENV.contains(&name.to_ascii_uppercase().as_str()))
        .collect()
}

fn shell(command: &str) -> Command {
    if cfg!(windows) {
        let mut shell = Command::new("cmd");
        shell.args(["/D", "/S", "/C", command]);
        shell
    } else {
        let mut shell = Command::new("sh");
        shell.args(["-c", command]);
        shell
    }
}

/// Kills the process and every child it started (a test runner spawns more processes).
async fn kill_tree(pid: u32) {
    let result = if cfg!(windows) {
        Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
    } else {
        Command::new("kill")
            .args(["-9", &format!("-{pid}")])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
    };
    if let Err(err) = result {
        tracing::warn!(pid, error = %err, "cannot kill test process tree");
    }
}

fn tail(text: &str, chars: usize) -> String {
    let count = text.chars().count();
    if count <= chars {
        return text.to_string();
    }
    let kept: String = text.chars().skip(count - chars).collect();
    format!("[... {} earlier characters cut ...]\n{kept}", count - chars)
}

async fn read_all<R: tokio::io::AsyncRead + Unpin>(pipe: Option<R>, buffer: &mut Vec<u8>) {
    use tokio::io::AsyncReadExt;
    if let Some(mut pipe) = pipe {
        let _ = pipe.read_to_end(buffer).await;
    }
}

fn combined_output(out: &[u8], err: &[u8]) -> String {
    let mut output = String::from_utf8_lossy(out).to_string();
    let stderr = String::from_utf8_lossy(err);
    if !stderr.trim().is_empty() {
        output.push_str("\n--- stderr ---\n");
        output.push_str(&stderr);
    }
    output
}

pub async fn run_tests(dir: &Path, command: &str, timeout: Duration) -> Result<TestRun> {
    let started = std::time::Instant::now();
    let mut child = shell(command)
        .current_dir(dir)
        .env_clear()
        .envs(allowed_env())
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("cannot start test command `{command}`"))?;
    let pid = child.id();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let mut out = Vec::new();
    let mut err = Vec::new();
    // The child stays owned here, so on timeout the tree is killed while the shell still
    // exists; dropping the shell first would orphan the processes it started.
    let finished = {
        let work = async {
            tokio::join!(read_all(stdout, &mut out), read_all(stderr, &mut err));
            child.wait().await
        };
        tokio::time::timeout(timeout, work).await
    };

    match finished {
        Ok(status) => {
            let status = status.context("test command did not finish")?;
            Ok(TestRun {
                command: command.to_string(),
                exit_code: status.code(),
                timed_out: false,
                passed: status.success(),
                duration_secs: started.elapsed().as_secs(),
                output: tail(&combined_output(&out, &err), OUTPUT_TAIL_CHARS),
            })
        }
        Err(_) => {
            if let Some(pid) = pid {
                kill_tree(pid).await;
            }
            let _ = child.kill().await;
            let partial = combined_output(&out, &err);
            Ok(TestRun {
                command: command.to_string(),
                exit_code: None,
                timed_out: true,
                passed: false,
                duration_secs: started.elapsed().as_secs(),
                output: tail(
                    &format!(
                        "{partial}\n[timed out after {} s; the process tree was killed]",
                        timeout.as_secs()
                    ),
                    OUTPUT_TAIL_CHARS,
                ),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reports_success_failure_and_output() {
        let dir = tempfile::tempdir().unwrap();
        let ok = run_tests(dir.path(), "echo all good", Duration::from_secs(30))
            .await
            .unwrap();
        assert!(ok.passed && ok.exit_code == Some(0) && ok.output.contains("all good"));

        let failed = run_tests(dir.path(), "exit 3", Duration::from_secs(30))
            .await
            .unwrap();
        assert!(!failed.passed);
        assert_eq!(failed.exit_code, Some(3));
    }

    #[tokio::test]
    async fn runs_in_the_workspace_without_secrets() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("marker.txt"), "here").unwrap();
        // SAFETY: variable name unique to this test.
        unsafe { std::env::set_var("AI_TEAM_SECRET_TEST_KEY", "leak") };

        let command = if cfg!(windows) {
            "type marker.txt && echo [%AI_TEAM_SECRET_TEST_KEY%]"
        } else {
            "cat marker.txt && echo [$AI_TEAM_SECRET_TEST_KEY]"
        };
        let run = run_tests(dir.path(), command, Duration::from_secs(30))
            .await
            .unwrap();
        assert!(run.output.contains("here"), "{}", run.output);
        assert!(
            !run.output.contains("leak"),
            "secrets must not reach tests: {}",
            run.output
        );
    }

    #[tokio::test]
    async fn kills_commands_that_hang() {
        let dir = tempfile::tempdir().unwrap();
        let command = if cfg!(windows) {
            "ping -n 30 127.0.0.1 >nul"
        } else {
            "sleep 30"
        };
        let run = run_tests(dir.path(), command, Duration::from_secs(2))
            .await
            .unwrap();
        assert!(run.timed_out && !run.passed);
        assert!(run.duration_secs < 15, "took {} s", run.duration_secs);
    }

    #[test]
    fn keeps_the_end_of_long_output() {
        let long = "a".repeat(100) + "THE END";
        let kept = tail(&long, 10);
        assert!(kept.ends_with("aaaTHE END") && kept.contains("earlier characters cut"));
    }
}
