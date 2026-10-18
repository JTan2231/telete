//! Chancery's append-only command usage journal.
//!
//! Registration and recording are separate operations. Recording never creates
//! a database or registration, retries a write, or decides product success.

#![allow(clippy::missing_errors_doc, clippy::must_use_candidate)]

pub mod cli;

use std::fs::{self, OpenOptions};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags, params};
use serde::Serialize;

const APPLICATION_ID: i64 = 0x4348_5553;
const SCHEMA_VERSION: i64 = 1;
const BUSY_TIMEOUT: Duration = Duration::from_millis(100);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("usage storage unavailable")]
    Io(#[from] std::io::Error),
    #[error("usage database operation failed")]
    Sqlite(#[from] rusqlite::Error),
    #[error("{0}")]
    Invalid(&'static str),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Explicit database selection takes precedence over the current user's home.
pub fn default_path() -> Result<PathBuf> {
    let path =
        if let Some(path) = std::env::var_os("CHANCERY_USAGE_DB").filter(|path| !path.is_empty()) {
            PathBuf::from(path)
        } else {
            PathBuf::from(std::env::var_os("HOME").ok_or(Error::Invalid("HOME is unavailable"))?)
                .join("Library/Application Support/Chancery/usage.sqlite3")
        };
    if !path.is_absolute() {
        return Err(Error::Invalid("usage database path must be absolute"));
    }
    Ok(path)
}

/// The current executing Codex thread, not an inferred root or parent thread.
pub fn thread_from_env() -> Result<Option<String>> {
    match std::env::var("CODEX_THREAD_ID") {
        Ok(value) if value.is_empty() => Ok(None),
        Ok(value) => {
            validate_id(&value)?;
            Ok(Some(value))
        }
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(_) => Err(Error::Invalid("CODEX_THREAD_ID must be UTF-8")),
    }
}

fn validate_id(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._".contains(&b))
    {
        return Err(Error::Invalid(
            "usage identities require 1–128 ASCII letters, digits, dots, dashes or underscores",
        ));
    }
    Ok(())
}

fn private_file(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o077 != 0 {
        return Err(Error::Invalid(
            "usage database must be a private regular file",
        ));
    }
    Ok(())
}

fn connect(path: &Path, write: bool) -> Result<Connection> {
    private_file(path)?;
    let flags = if write {
        OpenFlags::SQLITE_OPEN_READ_WRITE
    } else {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    };
    let connection = Connection::open_with_flags(path, flags | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    connection.busy_timeout(BUSY_TIMEOUT)?;
    connection.pragma_update(None, "foreign_keys", true)?;
    Ok(connection)
}

fn check_schema(connection: &Connection) -> Result<()> {
    let application: i64 = connection.pragma_query_value(None, "application_id", |r| r.get(0))?;
    let version: i64 = connection.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if application != APPLICATION_ID || version != SCHEMA_VERSION {
        return Err(Error::Invalid(
            "unsupported usage database; initialize it through Chancery",
        ));
    }
    Ok(())
}

/// An existing version-one journal. Ordinary opens never initialize or migrate.
pub struct Store(Connection);

impl Store {
    /// Initialize an empty journal or validate the current schema. Never migrate
    /// a foreign or unsupported database, or rewrite existing rows.
    pub fn initialize(path: &Path) -> Result<Self> {
        if !path.is_absolute() {
            return Err(Error::Invalid("usage database path must be absolute"));
        }
        let parent = path
            .parent()
            .ok_or(Error::Invalid("usage database needs a parent"))?;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
        {
            Ok(_) => (),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(e) => return Err(e.into()),
        }
        let mut connection = connect(path, true)?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let version: i64 = transaction.pragma_query_value(None, "user_version", |r| r.get(0))?;
        let application: i64 =
            transaction.pragma_query_value(None, "application_id", |r| r.get(0))?;
        if version == 0 && application == 0 {
            let tables: i64 = transaction.query_row(
                "SELECT count(*) FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%'",
                [],
                |r| r.get(0),
            )?;
            if tables != 0 {
                return Err(Error::Invalid("refusing to initialize a foreign database"));
            }
            transaction.execute_batch(include_str!("schema.sql"))?;
            transaction.pragma_update(None, "application_id", APPLICATION_ID)?;
            transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }
        check_schema(&transaction)?;
        transaction.commit()?;
        Ok(Self(connection))
    }

    pub fn open(path: &Path) -> Result<Self> {
        let connection = connect(path, true)?;
        check_schema(&connection)?;
        Ok(Self(connection))
    }

    pub fn read(path: &Path) -> Result<Self> {
        let connection = connect(path, false)?;
        check_schema(&connection)?;
        Ok(Self(connection))
    }

    pub fn register_system(&self, id: &str) -> Result<()> {
        validate_id(id)?;
        self.0.execute(
            "INSERT INTO systems(id) VALUES (?1) ON CONFLICT DO NOTHING",
            [id],
        )?;
        Ok(())
    }

    pub fn register_command(&self, system: &str, command: &str) -> Result<()> {
        validate_id(system)?;
        validate_id(command)?;
        self.0.execute(
            "INSERT INTO commands(system_id,id) VALUES (?1,?2) ON CONFLICT DO NOTHING",
            params![system, command],
        )?;
        Ok(())
    }

    /// A request-scoped thread must be supplied explicitly by service callers.
    /// This method never consults process environment.
    pub fn record(&self, system: &str, command: &str, thread: &str) -> Result<i64> {
        validate_id(system)?;
        validate_id(command)?;
        validate_id(thread)?;
        let recorded_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|d| i64::try_from(d.as_secs()).ok())
            .ok_or(Error::Invalid(
                "system time is outside the supported Unix range",
            ))?;
        self.0.execute("INSERT INTO usage(recorded_at,codex_thread_id,system_id,command_id) VALUES (?1,?2,?3,?4)", params![recorded_at, thread, system, command])?;
        Ok(self.0.last_insert_rowid())
    }

    pub fn systems(&self) -> Result<Vec<String>> {
        Ok(self
            .0
            .prepare("SELECT id FROM systems ORDER BY id")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// One row per registered command, including zero uses in the selected scope.
    pub fn counts(&self, filter: &Filter) -> Result<Vec<CommandCount>> {
        filter.validate()?;
        Ok(self.0.prepare("SELECT c.system_id,c.id,count(u.id),max(u.recorded_at) FROM commands c LEFT JOIN usage u ON u.system_id=c.system_id AND u.command_id=c.id AND (?1 IS NULL OR u.recorded_at>=?1) AND (?2 IS NULL OR u.recorded_at<?2) AND (?3 IS NULL OR u.codex_thread_id=?3) AND (?4=0 OR u.codex_thread_id IS NULL) WHERE (?5 IS NULL OR c.system_id=?5) GROUP BY c.system_id,c.id ORDER BY c.system_id,c.id")?
            .query_map(params![filter.since,filter.until,filter.thread,filter.unattributed,filter.system], |r| Ok(CommandCount { system_id:r.get(0)?,command_id:r.get(1)?,invocations:r.get(2)?,last_recorded_at:r.get(3)? }))?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Insertion order, with a database-local cursor and a bounded page.
    pub fn events(&self, filter: &Filter, after: i64, limit: usize) -> Result<EventPage> {
        filter.validate()?;
        if after < 0 || !(1..=1000).contains(&limit) {
            return Err(Error::Invalid(
                "usage cursor must be nonnegative and limit must be 1–1000",
            ));
        }
        let mut items = self.0.prepare("SELECT id,recorded_at,codex_thread_id,system_id,command_id FROM usage WHERE id>?1 AND (?2 IS NULL OR recorded_at>=?2) AND (?3 IS NULL OR recorded_at<?3) AND (?4 IS NULL OR codex_thread_id=?4) AND (?5=0 OR codex_thread_id IS NULL) AND (?6 IS NULL OR system_id=?6) ORDER BY id LIMIT ?7")?
            .query_map(params![after,filter.since,filter.until,filter.thread,filter.unattributed,filter.system,i64::try_from(limit+1).unwrap_or(1001)], |r| Ok(Event {id:r.get(0)?,recorded_at:r.get(1)?,codex_thread_id:r.get(2)?,system_id:r.get(3)?,command_id:r.get(4)?}))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let has_more = items.len() > limit;
        items.truncate(limit);
        let next_cursor = items.last().map_or(after, |item| item.id);
        Ok(EventPage {
            items,
            next_cursor,
            has_more,
        })
    }
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct Filter {
    pub system: Option<String>,
    pub thread: Option<String>,
    pub unattributed: bool,
    /// Inclusive Unix seconds.
    pub since: Option<i64>,
    /// Exclusive Unix seconds.
    pub until: Option<i64>,
}

impl Filter {
    fn validate(&self) -> Result<()> {
        if self.thread.is_some() && self.unattributed {
            return Err(Error::Invalid("thread and unattributed filters conflict"));
        }
        if self.since.is_some_and(|s| s < 0)
            || self.until.is_some_and(|u| u < 0)
            || matches!((self.since,self.until), (Some(s),Some(u)) if s>=u)
        {
            return Err(Error::Invalid(
                "usage time range must be nonnegative with since before until",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Serialize)]
pub struct CommandCount {
    pub system_id: String,
    pub command_id: String,
    pub invocations: i64,
    pub last_recorded_at: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct Event {
    pub id: i64,
    pub recorded_at: i64,
    pub codex_thread_id: Option<String>,
    pub system_id: String,
    pub command_id: String,
}

#[derive(Debug, Serialize)]
pub struct EventPage {
    pub items: Vec<Event>,
    pub next_cursor: i64,
    pub has_more: bool,
}

pub fn register_system(id: &str) -> Result<()> {
    Store::open(&default_path()?)?.register_system(id)
}

pub fn register_command(system: &str, command: &str) -> Result<()> {
    Store::open(&default_path()?)?.register_command(system, command)
}

/// Record an agent invocation, or skip it before accessing storage.
pub fn record(system: &str, command: &str) -> Result<Option<i64>> {
    if ["CHANCERY_USAGE_DISABLED", "CHANCERY_USAGE_INTERNAL"]
        .iter()
        .any(|name| std::env::var_os(name).as_deref() == Some(std::ffi::OsStr::new("1")))
    {
        return Ok(None);
    }
    let Some(thread) = thread_from_env()? else {
        return Ok(None);
    };
    record_with_thread(system, command, &thread).map(Some)
}

/// Explicit request-scoped recording; the caller owns agent attribution.
pub fn record_with_thread(system: &str, command: &str, thread: &str) -> Result<i64> {
    Store::open(&default_path()?)?.record(system, command, thread)
}

/// Command dispatch preserves the product result after a bounded recording error.
pub fn observe(system: &str, command: &str) {
    if let Err(error) = record(system, command) {
        eprintln!("chancery usage: {error}");
    }
}
