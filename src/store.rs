//! Private durable queue records.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::model::{CommitId, JobId};
use crate::workflow::{StageRun, Workflow};

const SCHEMA: u32 = 2;
const APPLICATION: u32 = 0x5445_4c45;

pub(crate) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub(crate) fn private_directory(path: &Path) -> Result<()> {
    crate::paths::ensure_private(path)
}

pub(crate) fn atomic_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("state file has no parent")?;
    private_directory(parent)?;
    if let Ok(metadata) = fs::symlink_metadata(path) {
        ensure!(
            metadata.is_file()
                && !metadata.file_type().is_symlink()
                && metadata.permissions().mode() & 0o777 == 0o600,
            "state file must be private and regular"
        );
    }
    let temporary = parent.join(format!(".{}.tmp", uuid::Uuid::now_v7()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if temporary.exists() {
        let _ = fs::remove_file(temporary);
    }
    result
}

pub(crate) fn atomic_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    atomic_bytes(path, &bytes)
}

pub(crate) fn lock(path: &Path, blocking: bool) -> Result<crate::paths::FileLock> {
    crate::paths::lock(path, blocking)
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    pub state_root: PathBuf,
    pub repository: PathBuf,
    pub common_git_dir: PathBuf,
    pub accepted_baseline: Option<CommitId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Phase {
    Queued,
    Integrating,
    Checking,
    Preparing,
    Accepting,
    Deploying,
    Blocked,
    Succeeded,
    Failed,
    Cancelled,
}

impl Phase {
    pub(crate) fn terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Integrating => "integrating",
            Self::Checking => "checking",
            Self::Preparing => "preparing",
            Self::Accepting => "accepting",
            Self::Deploying => "deploying",
            Self::Blocked => "blocked",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Operation {
    pub phase: Phase,
    pub name: String,
    pub candidate: CommitId,
    pub result: PathBuf,
    pub started: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Cleanup {
    pub state: String,
    pub registration: Option<PathBuf>,
    pub recorded: bool,
    pub updated: u64,
    pub error: Option<String>,
}

impl Default for Cleanup {
    fn default() -> Self {
        Self {
            state: "pending".into(),
            registration: None,
            recorded: false,
            updated: now(),
            error: None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
// Persist orthogonal facts independently of the phase: cancellation, acceptance,
// and uncertain operations can coexist and must survive recovery.
pub(crate) struct Job {
    pub id: JobId,
    pub sequence: u64,
    pub revision: u64,
    pub request_id: String,
    pub phase: Phase,
    pub cancel_requested: bool,
    pub created: u64,
    pub updated: u64,
    pub input: CommitId,
    pub workflow: Workflow,
    pub environment: BTreeMap<String, String>,
    pub base: Option<CommitId>,
    pub candidate: Option<CommitId>,
    pub accepted: bool,
    pub runs: Vec<StageRun>,
    pub operation: Option<Operation>,
    pub outcome: Option<Phase>,
    pub outcome_message: Option<String>,
    pub stopped_phase: Option<Phase>,
    pub unresolved: bool,
    pub last_error: Option<String>,
    pub cleanup: Cleanup,
}

impl Job {
    pub(crate) fn output(&self) -> Result<serde_json::Value> {
        let mut value = serde_json::to_value(self)?;
        let workflow = &self.workflow;
        value["stages"] = serde_json::Value::Array(
            [
                ("integrate", workflow.integrate, Phase::Integrating),
                ("check", true, Phase::Checking),
                ("prepare", workflow.prepare.is_some(), Phase::Preparing),
                ("accept", workflow.accept, Phase::Accepting),
                ("deploy", workflow.deploy.is_some(), Phase::Deploying),
            ]
            .into_iter()
            .map(|(stage, requested, phase)| {
                let run = self.runs.iter().rev().find(|run| {
                    run.stage == stage && self.candidate.as_ref() == Some(&run.candidate)
                });
                let active = self
                    .operation
                    .as_ref()
                    .is_some_and(|operation| operation.phase == phase)
                    || self.phase == phase;
                let stopped = self.stopped_phase == Some(phase);
                let status = if !requested {
                    "not_requested"
                } else if stage == "integrate" && self.candidate.is_some() && self.base.is_some()
                    || stage == "accept" && self.accepted
                {
                    "succeeded"
                } else if let Some(run) = run {
                    if run.result.success() {
                        "succeeded"
                    } else {
                        "failed"
                    }
                } else if (active || stopped) && self.unresolved {
                    "unresolved"
                } else if stopped && self.outcome == Some(Phase::Failed) {
                    "failed"
                } else if active {
                    "running"
                } else if self.phase.terminal() || self.outcome.is_some() {
                    "not_run"
                } else {
                    "pending"
                };
                serde_json::json!({
                    "stage": stage,
                    "status": status,
                    "candidate": self.candidate,
                    "operation": run.map(|run| &run.operation),
                })
            })
            .collect(),
        );
        Ok(value)
    }
}

pub(crate) struct Store {
    db: Connection,
}

fn row_u64(row: &rusqlite::Row<'_>, name: &str) -> rusqlite::Result<u64> {
    let value: i64 = row.get(name)?;
    u64::try_from(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Integer,
            Box::new(error),
        )
    })
}

/// Inspect retained state before initializing directories. Reading must not
/// apply writable `SQLite` pragmas or create journal sidecars.
pub(crate) fn probe_config(root: &Path) -> Result<Option<Config>> {
    if !crate::paths::private_directory_exists(root)? {
        return Ok(None);
    }
    let path = root.join("queue.sqlite3");
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        metadata.is_file()
            && !metadata.file_type().is_symlink()
            && metadata.uid() == rustix::process::getuid().as_raw()
            && metadata.mode() & 0o777 == 0o600,
        "queue database must be a private user-owned regular file"
    );
    let db = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    db.busy_timeout(Duration::from_secs(30))?;
    let version: u32 = db.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    let application: u32 = db.query_row("PRAGMA application_id", [], |row| row.get(0))?;
    ensure!(
        version == SCHEMA && application == APPLICATION,
        "unsupported or foreign Telete journal"
    );
    let text: Option<String> = db
        .query_row("SELECT value FROM control WHERE key='config'", [], |row| {
            row.get(0)
        })
        .optional()?;
    let config = text.context("Telete initialization is incomplete")?;
    Ok(Some(serde_json::from_str(&config)?))
}

impl Store {
    pub(crate) fn open(root: &Path, create: bool) -> Result<Self> {
        ensure!(
            create || root.exists(),
            "Telete is not initialized; run telete init"
        );
        private_directory(root)?;
        let path = root.join("queue.sqlite3");
        if !path.exists() {
            ensure!(create, "Telete is not initialized; run telete init");
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)?
                .sync_all()?;
        }
        let metadata = fs::symlink_metadata(&path)?;
        ensure!(
            metadata.is_file()
                && !metadata.file_type().is_symlink()
                && metadata.permissions().mode() & 0o777 == 0o600,
            "queue database must be private and regular"
        );
        let db = Connection::open(&path)?;
        db.busy_timeout(Duration::from_secs(30))?;
        db.execute_batch("PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;")?;
        let version: u32 = db.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        let application: u32 = db.query_row("PRAGMA application_id", [], |row| row.get(0))?;
        if version == 0 && application == 0 && create {
            let objects: i64 = db.query_row(
                "SELECT count(*) FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%'",
                [],
                |row| row.get(0),
            )?;
            ensure!(objects == 0, "refusing to initialize foreign state");
            db.execute_batch(&format!("BEGIN IMMEDIATE;
                CREATE TABLE control(key TEXT PRIMARY KEY,value TEXT NOT NULL);
                CREATE TABLE holds(owner TEXT PRIMARY KEY,created INTEGER NOT NULL);
                CREATE TABLE jobs(sequence INTEGER PRIMARY KEY AUTOINCREMENT,id TEXT UNIQUE NOT NULL,
                    request_id TEXT UNIQUE NOT NULL,phase TEXT NOT NULL,cancel_requested INTEGER NOT NULL DEFAULT 0,
                    revision INTEGER NOT NULL DEFAULT 0,created INTEGER NOT NULL,updated INTEGER NOT NULL,data TEXT NOT NULL);
                INSERT INTO control VALUES('paused','true');
                PRAGMA application_id={APPLICATION}; PRAGMA user_version={SCHEMA}; COMMIT;"))?;
        } else {
            ensure!(
                version == SCHEMA && application == APPLICATION,
                "unsupported or foreign Telete journal"
            );
        }
        Ok(Self { db })
    }

    pub(crate) fn transaction<T>(&self, operation: impl FnOnce() -> Result<T>) -> Result<T> {
        self.db.execute_batch("BEGIN IMMEDIATE")?;
        match operation() {
            Ok(value) => {
                self.db.execute_batch("COMMIT")?;
                Ok(value)
            }
            Err(error) => {
                let _ = self.db.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }

    pub(crate) fn get<T: serde::de::DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        let value = self
            .db
            .query_row("SELECT value FROM control WHERE key=?", [key], |row| {
                row.get::<_, String>(0)
            })
            .optional()?;
        value
            .map(|text| serde_json::from_str(&text).map_err(Into::into))
            .transpose()
    }

    pub(crate) fn set<T: Serialize>(&self, key: &str, value: &T) -> Result<()> {
        self.db.execute(
            "INSERT INTO control VALUES (?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, serde_json::to_string(value)?],
        )?;
        Ok(())
    }

    pub(crate) fn config(&self) -> Result<Config> {
        self.get("config")?
            .context("Telete initialization is incomplete")
    }
    pub(crate) fn paused(&self) -> Result<bool> {
        Ok(self.get::<bool>("paused")?.unwrap_or(true))
    }

    fn decode(row: &rusqlite::Row<'_>) -> rusqlite::Result<Job> {
        let text: String = row.get("data")?;
        let mut job: Job = serde_json::from_str(&text).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?;
        job.sequence = row_u64(row, "sequence")?;
        job.revision = row_u64(row, "revision")?;
        job.cancel_requested = row.get("cancel_requested")?;
        job.created = row_u64(row, "created")?;
        job.updated = row_u64(row, "updated")?;
        let identity: String = row.get("id")?;
        let phase: String = row.get("phase")?;
        if identity != job.id.0 || phase != job.phase.name() {
            return Err(rusqlite::Error::InvalidQuery);
        }
        Ok(job)
    }

    pub(crate) fn job(&self, id: &str) -> Result<Job> {
        self.db
            .query_row("SELECT * FROM jobs WHERE id=?", [id], Self::decode)
            .optional()?
            .context("unknown Telete job")
    }

    pub(crate) fn by_request(&self, request: &str) -> Result<Option<Job>> {
        Ok(self
            .db
            .query_row(
                "SELECT * FROM jobs WHERE request_id=?",
                [request],
                Self::decode,
            )
            .optional()?)
    }

    pub(crate) fn insert(&self, job: &Job) -> Result<Job> {
        self.db.execute(
            "INSERT INTO jobs(id,request_id,phase,created,updated,data) VALUES(?,?,?,?,?,?)",
            params![
                job.id.0,
                job.request_id,
                job.phase.name(),
                i64::try_from(job.created)?,
                i64::try_from(job.updated)?,
                serde_json::to_string(job)?
            ],
        )?;
        self.job(&job.id.0)
    }

    pub(crate) fn save(&self, job: &mut Job) -> Result<()> {
        job.updated = now();
        let changed = self.db.execute("UPDATE jobs SET phase=?,updated=?,data=?,revision=revision+1 WHERE id=? AND revision=?",
            params![job.phase.name(),i64::try_from(job.updated)?,serde_json::to_string(job)?,job.id.0,i64::try_from(job.revision)?])?;
        ensure!(
            changed == 1,
            "Telete job changed concurrently; reload before continuing"
        );
        job.revision += 1;
        job.cancel_requested = self.job(&job.id.0)?.cancel_requested;
        Ok(())
    }

    pub(crate) fn jobs(&self) -> Result<Vec<Job>> {
        Ok(self
            .db
            .prepare("SELECT * FROM jobs ORDER BY sequence")?
            .query_map([], Self::decode)?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub(crate) fn active(&self) -> Result<Option<Job>> {
        let active = self
            .jobs()?
            .into_iter()
            .filter(|job| !job.phase.terminal() && job.phase != Phase::Queued)
            .collect::<Vec<_>>();
        ensure!(
            active.len() <= 1,
            "multiple active Telete jobs; admission is stopped"
        );
        Ok(active.into_iter().next())
    }

    pub(crate) fn queued(&self) -> Result<Option<Job>> {
        Ok(self
            .jobs()?
            .into_iter()
            .find(|job| job.phase == Phase::Queued))
    }

    pub(crate) fn cancel(&self, id: &str) -> Result<()> {
        let job = self.job(id)?;
        if job.phase.terminal() {
            return Ok(());
        }
        self.db
            .execute("UPDATE jobs SET cancel_requested=1 WHERE id=?", [id])?;
        Ok(())
    }

    pub(crate) fn holds(&self) -> Result<Vec<String>> {
        Ok(self
            .db
            .prepare("SELECT owner FROM holds ORDER BY owner")?
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub(crate) fn hold(&self, owner: &str) -> Result<()> {
        self.db.execute(
            "INSERT OR IGNORE INTO holds VALUES(?,?)",
            params![owner, i64::try_from(now())?],
        )?;
        Ok(())
    }
    pub(crate) fn release(&self, owner: &str) -> Result<()> {
        self.db
            .execute("DELETE FROM holds WHERE owner=?", [owner])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_requires_explicit_state_binding_and_rejects_unknown_fields() -> Result<()> {
        let value = serde_json::json!({
            "state_root": "/state", "repository": "/repository",
            "common_git_dir": "/repository/.git", "accepted_baseline": null,
        });
        let config: Config = serde_json::from_value(value.clone())?;
        assert_eq!(config.state_root, PathBuf::from("/state"));
        assert!(config.accepted_baseline.is_none());
        let mut missing = value.clone();
        missing
            .as_object_mut()
            .context("configuration object")?
            .remove("state_root");
        assert!(serde_json::from_value::<Config>(missing).is_err());
        let mut unknown = value;
        unknown["unknown"] = serde_json::json!(true);
        assert!(serde_json::from_value::<Config>(unknown).is_err());
        Ok(())
    }

    #[test]
    fn initialized_store_is_private_paused_and_rejects_foreign_state() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let root = temporary.path().join("state");
        let store = Store::open(&root, true)?;
        assert!(store.paused()?);
        assert!(store.active()?.is_none());
        store.hold("fixture")?;
        store.hold("fixture")?;
        assert_eq!(store.holds()?, vec!["fixture"]);
        store.release("different-owner")?;
        assert_eq!(store.holds()?, vec!["fixture"]);
        assert_eq!(
            fs::metadata(root.join("queue.sqlite3"))?
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        drop(store);
        let foreign = Connection::open(root.join("queue.sqlite3"))?;
        foreign.execute_batch("PRAGMA application_id=0")?;
        drop(foreign);
        assert!(Store::open(&root, false).is_err());
        Ok(())
    }

    #[test]
    fn atomic_state_does_not_follow_symbolic_files() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let root = temporary.path().join("state");
        private_directory(&root)?;
        let outside = temporary.path().join("outside");
        fs::write(&outside, "original")?;
        std::os::unix::fs::symlink(&outside, root.join("result.json"))?;
        assert!(atomic_bytes(&root.join("result.json"), b"changed").is_err());
        assert_eq!(fs::read_to_string(outside)?, "original");
        Ok(())
    }
}
