//! SQLite persistence for projects, plans, tasks, runs and Owner decisions, plus a
//! content-addressed artifact store: specifications and implementations live once on disk
//! under their SHA-256, and the database keeps only `artifact:<hash>` references.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, Row, params};
use sha2::{Digest, Sha256};

use super::{
    ProjectStatus, TaskStatus,
    plan::{OrderedTask, TaskPlan},
};
use crate::{
    domain::{ProjectConfig, TeamState},
    llm::UsageTotals,
};

const DATABASE_FILE: &str = "ai-team.db";
const ARTIFACT_PREFIX: &str = "artifact:";
const SCHEMA_VERSION: i64 = 2;

const SCHEMA: &str = "
CREATE TABLE projects (
    id          TEXT PRIMARY KEY,
    config_json TEXT NOT NULL,
    status      TEXT NOT NULL,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);
CREATE TABLE plans (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id    TEXT NOT NULL REFERENCES projects(id),
    proposal_json TEXT NOT NULL,
    status        TEXT NOT NULL,          -- PROPOSED | APPROVED | SUPERSEDED
    note          TEXT,
    created_at    INTEGER NOT NULL
);
CREATE TABLE milestones (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id  TEXT NOT NULL REFERENCES projects(id),
    name        TEXT NOT NULL,
    position    INTEGER NOT NULL,
    report_path TEXT
);
CREATE TABLE tasks (
    id               TEXT PRIMARY KEY,
    project_id       TEXT NOT NULL REFERENCES projects(id),
    milestone_id     INTEGER NOT NULL REFERENCES milestones(id),
    position         INTEGER NOT NULL,
    title            TEXT NOT NULL,
    description      TEXT NOT NULL,
    acceptance_json  TEXT NOT NULL,
    status           TEXT NOT NULL,
    iteration_budget INTEGER NOT NULL,
    state_json       TEXT,
    spec_hash        TEXT,
    impl_hash        TEXT,
    error            TEXT,
    created_at       INTEGER NOT NULL,
    updated_at       INTEGER NOT NULL
);
CREATE TABLE task_dependencies (
    task_id    TEXT NOT NULL REFERENCES tasks(id),
    depends_on TEXT NOT NULL REFERENCES tasks(id),
    PRIMARY KEY (task_id, depends_on)
);
CREATE TABLE runs (
    id          TEXT PRIMARY KEY,
    task_id     TEXT NOT NULL REFERENCES tasks(id),
    outcome     TEXT NOT NULL,
    audit_file  TEXT NOT NULL,
    finished_at INTEGER NOT NULL
);
CREATE TABLE owner_decisions (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id TEXT NOT NULL REFERENCES projects(id),
    task_id    TEXT REFERENCES tasks(id),
    decision   TEXT NOT NULL,
    note       TEXT,
    created_at INTEGER NOT NULL
);
CREATE TABLE artifacts (
    hash       TEXT PRIMARY KEY,
    size       INTEGER NOT NULL,
    created_at INTEGER NOT NULL
);
";

#[derive(Debug, Clone)]
pub struct ProjectRecord {
    pub config: ProjectConfig,
    pub status: ProjectStatus,
}

#[derive(Debug, Clone)]
pub struct MilestoneRecord {
    pub id: i64,
    pub name: String,
    pub position: i64,
    pub report_path: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TaskRecord {
    pub id: String,
    pub project_id: String,
    pub milestone_id: i64,
    pub position: i64,
    pub title: String,
    pub description: String,
    pub acceptance_criteria: Vec<String>,
    pub status: TaskStatus,
    pub iteration_budget: u32,
    pub depends_on: Vec<String>,
    pub spec_hash: Option<String>,
    pub impl_hash: Option<String>,
    pub error: Option<String>,
}

/// Artifact hashes the selected tasks point to: their spec and implementation, and every
/// `artifact:<hash>` reference in their saved state.
fn artifact_refs(
    conn: &Connection,
    sql: &str,
    project_id: Option<&str>,
) -> Result<std::collections::HashSet<String>> {
    let mut statement = conn.prepare(sql)?;
    let map = |row: &Row<'_>| {
        Ok((
            row.get::<_, Option<String>>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<String>>(2)?,
        ))
    };
    let rows = match project_id {
        Some(id) => statement
            .query_map(params![id], map)?
            .collect::<rusqlite::Result<Vec<_>>>()?,
        None => statement
            .query_map([], map)?
            .collect::<rusqlite::Result<Vec<_>>>()?,
    };
    let mut hashes = std::collections::HashSet::new();
    for (spec, implementation, state) in rows {
        hashes.extend(spec);
        hashes.extend(implementation);
        for piece in state.unwrap_or_default().split("artifact:").skip(1) {
            let hash: String = piece.chars().take_while(char::is_ascii_hexdigit).collect();
            if hash.len() == 64 {
                hashes.insert(hash);
            }
        }
    }
    Ok(hashes)
}

/// What `Store::delete_project` removed, and the files left for the caller to delete.
#[derive(Debug, Clone)]
pub struct DeletedProject {
    /// Audit trails of the project's runs, as stored (relative to the CLI's working folder).
    pub audit_files: Vec<PathBuf>,
    pub reports: Vec<PathBuf>,
    pub artifacts_removed: usize,
}

#[derive(Debug, Clone)]
pub struct OwnerDecision {
    pub task_id: Option<String>,
    pub decision: String,
    pub note: Option<String>,
    pub created_at: i64,
}

pub struct Store {
    conn: Mutex<Connection>,
    root: PathBuf,
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or_default()
}

impl Store {
    /// Opens (creating if needed) `<data_dir>/ai-team.db` and `<data_dir>/artifacts/`.
    pub fn open(data_dir: impl AsRef<Path>) -> Result<Self> {
        let root = data_dir.as_ref().to_path_buf();
        fs::create_dir_all(root.join("artifacts"))
            .with_context(|| format!("cannot create data folder {}", root.display()))?;

        let conn = Connection::open(root.join(DATABASE_FILE))
            .with_context(|| format!("cannot open database in {}", root.display()))?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;

        let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        anyhow::ensure!(
            version <= SCHEMA_VERSION,
            "database schema version {version} is newer than this binary supports ({SCHEMA_VERSION})"
        );
        if version < 1 {
            conn.execute_batch(SCHEMA)
                .context("cannot create database schema")?;
        }
        if version < 2 {
            // Phase 4: planning usage per project, merge time per task.
            conn.execute_batch(
                "ALTER TABLE projects ADD COLUMN planning_usage_json TEXT;
                 ALTER TABLE tasks ADD COLUMN merged_at INTEGER;",
            )
            .context("cannot migrate database to version 2")?;
        }
        conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))?;

        Ok(Self {
            conn: Mutex::new(conn),
            root,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    // -- deletion -------------------------------------------------------------

    /// Permanently removes a project and everything recorded about it, in one transaction:
    /// plans, milestones, tasks (with their saved state and usage), dependencies, runs and the
    /// Owner's decisions. Artifacts no other project references are removed too. Returns the
    /// files the caller must delete (audit trails of the runs, milestone reports).
    pub fn delete_project(&self, project_id: &str) -> Result<DeletedProject> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let exists: bool = tx
            .query_row(
                "SELECT COUNT(*) FROM projects WHERE id = ?1",
                params![project_id],
                |row| row.get::<_, i64>(0),
            )
            .map(|count| count > 0)?;
        anyhow::ensure!(exists, "unknown project {project_id}");

        let strings = |sql: &str| -> Result<Vec<String>> {
            let mut statement = tx.prepare(sql)?;
            let rows = statement
                .query_map(params![project_id], |row| row.get::<_, Option<String>>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows.into_iter().flatten().collect())
        };
        let audit_files = strings(
            "SELECT audit_file FROM runs WHERE task_id IN (SELECT id FROM tasks WHERE project_id = ?1)",
        )?;
        let reports = strings("SELECT report_path FROM milestones WHERE project_id = ?1")?;

        let owned = artifact_refs(
            &tx,
            "SELECT spec_hash, impl_hash, state_json FROM tasks WHERE project_id = ?1",
            Some(project_id),
        )?;

        let in_project = "SELECT id FROM tasks WHERE project_id = ?1";
        tx.execute(
            &format!(
                "DELETE FROM task_dependencies WHERE task_id IN ({in_project}) OR depends_on IN ({in_project})"
            ),
            params![project_id],
        )?;
        tx.execute(
            &format!("DELETE FROM runs WHERE task_id IN ({in_project})"),
            params![project_id],
        )?;
        for table in [
            "owner_decisions",
            "tasks",
            "milestones",
            "plans",
            "projects",
        ] {
            let column = if table == "projects" {
                "id"
            } else {
                "project_id"
            };
            tx.execute(
                &format!("DELETE FROM {table} WHERE {column} = ?1"),
                params![project_id],
            )?;
        }

        // Only this project's artifacts may go, and only those no remaining task uses
        // (artifacts are shared by content). Other projects' artifacts are never touched.
        let referenced = artifact_refs(
            &tx,
            "SELECT spec_hash, impl_hash, state_json FROM tasks",
            None,
        )?;
        let orphans: Vec<String> = owned
            .into_iter()
            .filter(|hash| !referenced.contains(hash))
            .collect();
        for hash in &orphans {
            tx.execute("DELETE FROM artifacts WHERE hash = ?1", params![hash])?;
        }
        tx.commit()?;
        drop(conn);

        for hash in &orphans {
            let _ = fs::remove_file(self.artifact_path(hash));
        }
        Ok(DeletedProject {
            audit_files: audit_files.into_iter().map(PathBuf::from).collect(),
            reports: reports.into_iter().map(PathBuf::from).collect(),
            artifacts_removed: orphans.len(),
        })
    }

    // -- artifacts ----------------------------------------------------------

    /// Stores content once under its SHA-256 and returns the hash.
    pub fn put_artifact(&self, content: &str) -> Result<String> {
        let hash: String = Sha256::digest(content.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let path = self.artifact_path(&hash);
        if !path.exists() {
            fs::write(&path, content)
                .with_context(|| format!("cannot write artifact {}", path.display()))?;
        }
        self.conn().execute(
            "INSERT OR IGNORE INTO artifacts (hash, size, created_at) VALUES (?1, ?2, ?3)",
            params![hash, content.len() as i64, now()],
        )?;
        Ok(hash)
    }

    pub fn artifact(&self, hash: &str) -> Result<String> {
        let path = self.artifact_path(hash);
        fs::read_to_string(&path).with_context(|| format!("missing artifact {}", path.display()))
    }

    pub fn artifact_path(&self, hash: &str) -> PathBuf {
        self.root.join("artifacts").join(format!("{hash}.md"))
    }

    // -- projects -----------------------------------------------------------

    pub fn add_project(&self, config: &ProjectConfig) -> Result<()> {
        let exists = self
            .conn()
            .query_row(
                "SELECT 1 FROM projects WHERE id = ?1",
                params![config.project_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        anyhow::ensure!(!exists, "project {} already exists", config.project_id);

        let at = now();
        self.conn().execute(
            "INSERT INTO projects (id, config_json, status, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?4)",
            params![
                config.project_id,
                serde_json::to_string(config)?,
                ProjectStatus::Planning.as_str(),
                at
            ],
        )?;
        Ok(())
    }

    pub fn project(&self, project_id: &str) -> Result<ProjectRecord> {
        self.conn()
            .query_row(
                "SELECT config_json, status FROM projects WHERE id = ?1",
                params![project_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?
            .with_context(|| format!("unknown project {project_id}"))
            .and_then(|(config, status)| {
                Ok(ProjectRecord {
                    config: serde_json::from_str(&config)?,
                    status: status.parse()?,
                })
            })
    }

    pub fn projects(&self) -> Result<Vec<ProjectRecord>> {
        let conn = self.conn();
        let mut statement = conn.prepare("SELECT config_json, status FROM projects ORDER BY id")?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|(config, status)| {
                Ok(ProjectRecord {
                    config: serde_json::from_str(&config)?,
                    status: status.parse()?,
                })
            })
            .collect()
    }

    /// Replaces a project's stored configuration (the id cannot change).
    pub fn update_project_config(&self, config: &ProjectConfig) -> Result<()> {
        let changed = self.conn().execute(
            "UPDATE projects SET config_json = ?2, updated_at = ?3 WHERE id = ?1",
            params![config.project_id, serde_json::to_string(config)?, now()],
        )?;
        anyhow::ensure!(changed == 1, "unknown project {}", config.project_id);
        Ok(())
    }

    pub fn add_planning_usage(&self, project_id: &str, usage: &UsageTotals) -> Result<()> {
        let mut total = self.planning_usage(project_id)?;
        total.add(usage);
        self.conn().execute(
            "UPDATE projects SET planning_usage_json = ?2 WHERE id = ?1",
            params![project_id, serde_json::to_string(&total)?],
        )?;
        Ok(())
    }

    pub fn planning_usage(&self, project_id: &str) -> Result<UsageTotals> {
        let json: Option<String> = self
            .conn()
            .query_row(
                "SELECT planning_usage_json FROM projects WHERE id = ?1",
                params![project_id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        Ok(match json {
            Some(json) => serde_json::from_str(&json)?,
            None => UsageTotals::default(),
        })
    }

    pub fn mark_merged(&self, task_id: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE tasks SET merged_at = ?2 WHERE id = ?1",
            params![task_id, now()],
        )?;
        Ok(())
    }

    pub fn set_project_status(&self, project_id: &str, status: ProjectStatus) -> Result<()> {
        self.conn().execute(
            "UPDATE projects SET status = ?2, updated_at = ?3 WHERE id = ?1",
            params![project_id, status.as_str(), now()],
        )?;
        Ok(())
    }

    // -- plans --------------------------------------------------------------

    /// Stores a validated proposal as the project's pending plan (an older pending one is
    /// superseded).
    pub fn save_plan_proposal(
        &self,
        project_id: &str,
        plan: &TaskPlan,
        note: Option<&str>,
    ) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute(
            "UPDATE plans SET status = 'SUPERSEDED' WHERE project_id = ?1 AND status = 'PROPOSED'",
            params![project_id],
        )?;
        tx.execute(
            "INSERT INTO plans (project_id, proposal_json, status, note, created_at)
             VALUES (?1, ?2, 'PROPOSED', ?3, ?4)",
            params![project_id, serde_json::to_string(plan)?, note, now()],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn pending_plan(&self, project_id: &str) -> Result<Option<TaskPlan>> {
        self.conn()
            .query_row(
                "SELECT proposal_json FROM plans
                 WHERE project_id = ?1 AND status = 'PROPOSED' ORDER BY id DESC LIMIT 1",
                params![project_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|json| serde_json::from_str(&json).context("corrupt stored plan"))
            .transpose()
    }

    /// Adds one task written by the Owner (no dependencies) under `milestone`, created if
    /// needed. A project without a plan becomes runnable. Returns the new task id.
    pub fn add_owner_task(
        &self,
        project_id: &str,
        milestone: &str,
        title: &str,
        description: &str,
        acceptance: &[String],
        iteration_budget: u32,
    ) -> Result<String> {
        let at = now();
        let mut conn = self.conn();
        let tx = conn.transaction()?;

        let milestone_id = match tx
            .query_row(
                "SELECT id FROM milestones WHERE project_id = ?1 AND name = ?2",
                params![project_id, milestone],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
        {
            Some(id) => {
                // A new task reopens the milestone: its report is rewritten once it is done.
                tx.execute(
                    "UPDATE milestones SET report_path = NULL WHERE id = ?1",
                    params![id],
                )?;
                id
            }
            None => {
                let position: i64 = tx.query_row(
                    "SELECT COALESCE(MAX(position) + 1, 0) FROM milestones WHERE project_id = ?1",
                    params![project_id],
                    |row| row.get(0),
                )?;
                tx.execute(
                    "INSERT INTO milestones (project_id, name, position) VALUES (?1, ?2, ?3)",
                    params![project_id, milestone, position],
                )?;
                tx.last_insert_rowid()
            }
        };

        let (count, position): (i64, i64) = tx.query_row(
            "SELECT COUNT(*), COALESCE(MAX(position) + 1, 0) FROM tasks WHERE project_id = ?1",
            params![project_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let id = format!("{project_id}-T{:02}", count + 1);
        tx.execute(
            "INSERT INTO tasks (id, project_id, milestone_id, position, title, description,
                                acceptance_json, status, iteration_budget, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10)",
            params![
                id,
                project_id,
                milestone_id,
                position,
                title,
                description,
                serde_json::to_string(acceptance)?,
                TaskStatus::Planned.as_str(),
                iteration_budget,
                at
            ],
        )?;
        // Owner tasks need no plan approval; a finished project reopens for the new work.
        tx.execute(
            "UPDATE projects SET status = ?2, updated_at = ?3
             WHERE id = ?1 AND status IN ('PLANNING', 'DONE')",
            params![project_id, ProjectStatus::Planned.as_str(), at],
        )?;
        tx.commit()?;
        Ok(id)
    }

    /// Creates the milestones, tasks and dependencies of an approved plan in one transaction.
    /// Task ids are `<project>-T01`, `<project>-T02`, ... in execution order.
    pub fn approve_plan(
        &self,
        project_id: &str,
        plan: &TaskPlan,
        ordered: &[OrderedTask],
        iteration_budget: u32,
    ) -> Result<Vec<String>> {
        let at = now();
        let mut conn = self.conn();
        let tx = conn.transaction()?;

        let mut milestone_ids = Vec::new();
        for (position, milestone) in plan.milestones.iter().enumerate() {
            tx.execute(
                "INSERT INTO milestones (project_id, name, position) VALUES (?1, ?2, ?3)",
                params![project_id, milestone.name.trim(), position as i64],
            )?;
            milestone_ids.push(tx.last_insert_rowid());
        }

        let id_of = |key: &str| {
            ordered
                .iter()
                .position(|o| o.task.key.trim() == key.trim())
                .map(|index| format!("{project_id}-T{:02}", index + 1))
        };

        let mut ids = Vec::new();
        for (index, item) in ordered.iter().enumerate() {
            let id = format!("{project_id}-T{:02}", index + 1);
            tx.execute(
                "INSERT INTO tasks (id, project_id, milestone_id, position, title, description,
                                    acceptance_json, status, iteration_budget, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10)",
                params![
                    id,
                    project_id,
                    milestone_ids[item.milestone],
                    index as i64,
                    item.task.title.trim(),
                    item.task.description.trim(),
                    serde_json::to_string(&item.task.acceptance_criteria)?,
                    TaskStatus::Planned.as_str(),
                    iteration_budget,
                    at
                ],
            )?;
            ids.push(id);
        }
        for (index, item) in ordered.iter().enumerate() {
            for dependency in &item.task.depends_on {
                let dependency_id = id_of(dependency).context("validated plan lost a task")?;
                tx.execute(
                    "INSERT INTO task_dependencies (task_id, depends_on) VALUES (?1, ?2)",
                    params![ids[index], dependency_id],
                )?;
            }
        }

        tx.execute(
            "UPDATE plans SET status = 'APPROVED' WHERE project_id = ?1 AND status = 'PROPOSED'",
            params![project_id],
        )?;
        tx.execute(
            "UPDATE projects SET status = ?2, updated_at = ?3 WHERE id = ?1",
            params![project_id, ProjectStatus::Planned.as_str(), at],
        )?;
        tx.commit()?;
        Ok(ids)
    }

    // -- milestones ---------------------------------------------------------

    pub fn milestones(&self, project_id: &str) -> Result<Vec<MilestoneRecord>> {
        let conn = self.conn();
        let mut statement = conn.prepare(
            "SELECT id, name, position, report_path FROM milestones
             WHERE project_id = ?1 ORDER BY position",
        )?;
        let rows = statement
            .query_map(params![project_id], |row| {
                Ok(MilestoneRecord {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    position: row.get(2)?,
                    report_path: row.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn mark_milestone_reported(&self, milestone_id: i64, report_path: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE milestones SET report_path = ?2 WHERE id = ?1",
            params![milestone_id, report_path],
        )?;
        Ok(())
    }

    // -- tasks --------------------------------------------------------------

    const TASK_COLUMNS: &str = "id, project_id, milestone_id, position, title, description,
        acceptance_json, status, iteration_budget, spec_hash, impl_hash, error";

    fn task_from_row(row: &Row<'_>) -> rusqlite::Result<(TaskRecord, String, String)> {
        Ok((
            TaskRecord {
                id: row.get(0)?,
                project_id: row.get(1)?,
                milestone_id: row.get(2)?,
                position: row.get(3)?,
                title: row.get(4)?,
                description: row.get(5)?,
                acceptance_criteria: Vec::new(),
                status: TaskStatus::Planned,
                iteration_budget: row.get(8)?,
                depends_on: Vec::new(),
                spec_hash: row.get(9)?,
                impl_hash: row.get(10)?,
                error: row.get(11)?,
            },
            row.get::<_, String>(6)?,
            row.get::<_, String>(7)?,
        ))
    }

    fn finish_task(
        &self,
        conn: &Connection,
        (mut task, acceptance, status): (TaskRecord, String, String),
    ) -> Result<TaskRecord> {
        task.acceptance_criteria = serde_json::from_str(&acceptance)?;
        task.status = status.parse()?;
        let mut statement = conn.prepare(
            "SELECT depends_on FROM task_dependencies WHERE task_id = ?1 ORDER BY depends_on",
        )?;
        task.depends_on = statement
            .query_map(params![task.id], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<String>>>()?;
        Ok(task)
    }

    /// Tasks of a project in execution order.
    pub fn tasks(&self, project_id: &str) -> Result<Vec<TaskRecord>> {
        let conn = self.conn();
        let mut statement = conn.prepare(&format!(
            "SELECT {} FROM tasks WHERE project_id = ?1 ORDER BY position",
            Self::TASK_COLUMNS
        ))?;
        let raw = statement
            .query_map(params![project_id], Self::task_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        raw.into_iter()
            .map(|row| self.finish_task(&conn, row))
            .collect()
    }

    pub fn task(&self, task_id: &str) -> Result<TaskRecord> {
        let conn = self.conn();
        let raw = conn
            .query_row(
                &format!("SELECT {} FROM tasks WHERE id = ?1", Self::TASK_COLUMNS),
                params![task_id],
                Self::task_from_row,
            )
            .optional()?
            .with_context(|| format!("unknown task {task_id}"))?;
        self.finish_task(&conn, raw)
    }

    pub fn set_task_status(&self, task_id: &str, status: TaskStatus) -> Result<()> {
        self.conn().execute(
            "UPDATE tasks SET status = ?2, updated_at = ?3 WHERE id = ?1",
            params![task_id, status.as_str(), now()],
        )?;
        Ok(())
    }

    pub fn add_iterations(&self, task_id: &str, extra: u32) -> Result<()> {
        self.conn().execute(
            "UPDATE tasks SET iteration_budget = iteration_budget + ?2, updated_at = ?3
             WHERE id = ?1",
            params![task_id, extra, now()],
        )?;
        Ok(())
    }

    /// Persists the run state of a task. Artifact contents go to the artifact store; the row
    /// keeps `artifact:<hash>` references.
    pub fn save_task_state(
        &self,
        task_id: &str,
        status: TaskStatus,
        state: &TeamState,
    ) -> Result<()> {
        let mut stored = state.clone();
        let mut hashes = [None, None];
        for (slot, artifact) in [&mut stored.specification, &mut stored.implementation]
            .into_iter()
            .enumerate()
        {
            if let Some(artifact) = artifact.as_mut()
                && !artifact.content.starts_with(ARTIFACT_PREFIX)
            {
                let hash = self.put_artifact(&artifact.content)?;
                artifact.content = format!("{ARTIFACT_PREFIX}{hash}");
                hashes[slot] = Some(hash);
            }
        }

        self.conn().execute(
            "UPDATE tasks SET status = ?2, state_json = ?3, spec_hash = COALESCE(?4, spec_hash),
                              impl_hash = COALESCE(?5, impl_hash), error = ?6, updated_at = ?7
             WHERE id = ?1",
            params![
                task_id,
                status.as_str(),
                serde_json::to_string(&stored)?,
                hashes[0],
                hashes[1],
                state.error,
                now()
            ],
        )?;
        Ok(())
    }

    /// Loads the saved run state with artifact contents restored.
    pub fn load_task_state(&self, task_id: &str) -> Result<Option<TeamState>> {
        let json: Option<String> = self
            .conn()
            .query_row(
                "SELECT state_json FROM tasks WHERE id = ?1",
                params![task_id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        let Some(json) = json else {
            return Ok(None);
        };

        let mut state: TeamState = serde_json::from_str(&json).context("corrupt task state")?;
        for artifact in [&mut state.specification, &mut state.implementation]
            .into_iter()
            .flatten()
        {
            if let Some(hash) = artifact.content.strip_prefix(ARTIFACT_PREFIX) {
                artifact.content = self.artifact(hash)?;
            }
        }
        Ok(Some(state))
    }

    pub fn record_run(
        &self,
        run_id: &str,
        task_id: &str,
        outcome: &str,
        audit_file: &str,
    ) -> Result<()> {
        self.conn().execute(
            "INSERT OR REPLACE INTO runs (id, task_id, outcome, audit_file, finished_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![run_id, task_id, outcome, audit_file, now()],
        )?;
        Ok(())
    }

    pub fn runs_of(&self, task_id: &str) -> Result<Vec<(String, String, String)>> {
        let conn = self.conn();
        let mut statement = conn.prepare(
            "SELECT id, outcome, audit_file FROM runs WHERE task_id = ?1 ORDER BY finished_at, rowid",
        )?;
        let rows = statement
            .query_map(params![task_id], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // -- owner decisions ----------------------------------------------------

    pub fn add_owner_decision(
        &self,
        project_id: &str,
        task_id: Option<&str>,
        decision: &str,
        note: Option<&str>,
    ) -> Result<()> {
        self.conn().execute(
            "INSERT INTO owner_decisions (project_id, task_id, decision, note, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![project_id, task_id, decision, note, now()],
        )?;
        Ok(())
    }

    pub fn owner_decisions(&self, project_id: &str) -> Result<Vec<OwnerDecision>> {
        let conn = self.conn();
        let mut statement = conn.prepare(
            "SELECT task_id, decision, note, created_at FROM owner_decisions
             WHERE project_id = ?1 ORDER BY id",
        )?;
        let rows = statement
            .query_map(params![project_id], |row| {
                Ok(OwnerDecision {
                    task_id: row.get(0)?,
                    decision: row.get(1)?,
                    note: row.get(2)?,
                    created_at: row.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}
