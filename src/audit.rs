use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use serde::Serialize;

/// Per-run audit files.
pub fn default_audit_dir() -> PathBuf {
    Path::new(".ai-team").join("runs")
}

/// Company-level decisions (hiring, status changes) are appended to `hr.jsonl` here.
pub fn hr_audit_dir() -> PathBuf {
    PathBuf::from(".ai-team")
}

#[derive(Debug, Serialize)]
struct AuditEvent<'a, T: Serialize> {
    unix_ms: u64,
    run_id: &'a str,
    span_id: &'a str,
    parent_span_id: Option<&'a str>,
    actor: &'a str,
    event: &'a str,
    payload: &'a T,
}

/// Append-only JSONL audit file for one run. Events carry `span_id`/`parent_span_id`
/// so agent → subagent calls can be rebuilt as a tree.
#[derive(Clone)]
pub struct AuditTrail {
    path: PathBuf,
    // Subagents may write concurrently; one line per event must stay intact.
    write_lock: Arc<Mutex<()>>,
}

impl AuditTrail {
    pub fn create(dir: impl AsRef<Path>, run_id: &str) -> Result<Self> {
        let dir = dir.as_ref();
        fs::create_dir_all(dir)
            .with_context(|| format!("cannot create audit directory: {}", dir.display()))?;

        Ok(Self {
            path: dir.join(format!("{run_id}.jsonl")),
            write_lock: Arc::new(Mutex::new(())),
        })
    }

    pub fn record<T: Serialize>(
        &self,
        run_id: &str,
        span_id: &str,
        parent_span_id: Option<&str>,
        actor: &str,
        event: &str,
        payload: &T,
    ) -> Result<()> {
        let unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system clock is before UNIX epoch")?
            .as_millis() as u64;

        let entry = AuditEvent {
            unix_ms,
            run_id,
            span_id,
            parent_span_id,
            actor,
            event,
            payload,
        };

        let line = serde_json::to_string(&entry).context("cannot serialize audit event")?;

        let _guard = self
            .write_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("cannot open audit file: {}", self.path.display()))?;

        writeln!(file, "{line}").context("cannot append audit event")?;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}
