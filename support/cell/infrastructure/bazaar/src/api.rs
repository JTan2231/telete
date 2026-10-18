//! Public in-process access to Bazaar. No operation invokes a CLI or model.
//!
//! ```text
//! use bazaar::api::{Reader, Writer};
//! let database = std::path::Path::new("/absolute/private/bazaar.sqlite3");
//! let mut writer = Writer::initialize(&database)?;
//! let saved = writer.update("example.prompt", "Read {{input}} carefully.")?;
//! let reader = Reader::open(&database)?;
//! assert_eq!(reader.get("example.prompt", Some(saved.version))?, saved);
//! assert_eq!(reader.history("example.prompt")?, vec![1]);
//! ```

use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::time::Duration;

const APPLICATION_ID: i64 = 0x425A_4152;
const SCHEMA_VERSION: i64 = 1;

/// One complete immutable value. Content is returned without interpretation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Record {
    pub id: String,
    pub version: i64,
    pub content: String,
}

/// Failure to open, read, or append Bazaar state.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("ID must not be empty")]
    EmptyId,
    #[error("version must be positive")]
    InvalidVersion,
    #[error("ID or requested version was not found")]
    NotFound,
    #[error("the version number cannot be increased")]
    VersionExhausted,
    #[error("unsupported or uninitialized Bazaar database")]
    UnsupportedDatabase,
    #[error("refusing to initialize a nonempty or foreign database")]
    ForeignDatabase,
    #[error("Bazaar requires an absolute database path with a private regular parent directory")]
    InvalidPath,
    #[error("Bazaar requires a private regular database file")]
    InvalidFile,
    #[error("Bazaar database integrity check failed")]
    Integrity,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
}

/// A Bazaar operation result.
pub type Result<T> = std::result::Result<T, Error>;

/// Read-only access to an existing database. This type has no write operations.
pub struct Reader {
    connection: Connection,
}

/// Explicit append access. Use [`Reader`] for ordinary consumers.
pub struct Writer {
    connection: Connection,
}

impl Reader {
    /// Open an existing database without initialization, recovery, or writes.
    ///
    /// # Errors
    /// Returns an error for missing, foreign, unsupported, inaccessible, or
    /// nonprivate state. No files or directories are created.
    pub fn open(database: impl AsRef<Path>) -> Result<Self> {
        let connection = connect(database.as_ref(), false)?;
        check_identity(&connection)?;
        Ok(Self { connection })
    }

    /// Read the latest version, or the exact positive version supplied.
    ///
    /// # Errors
    /// Returns [`Error::NotFound`] for an unknown ID or version. Empty IDs,
    /// nonpositive versions, and database read failures are errors.
    pub fn get(&self, id: &str, version: Option<i64>) -> Result<Record> {
        validate_id(id)?;
        if version.is_some_and(|value| value <= 0) {
            return Err(Error::InvalidVersion);
        }
        let query = if version.is_some() {
            "SELECT id, version, content FROM versions WHERE id = ?1 AND version = ?2"
        } else {
            "SELECT id, version, content FROM versions WHERE id = ?1 AND ?2 IS NULL ORDER BY version DESC LIMIT 1"
        };
        self.connection
            .query_row(query, params![id, version], |row| {
                Ok(Record {
                    id: row.get(0)?,
                    version: row.get(1)?,
                    content: row.get(2)?,
                })
            })
            .optional()?
            .ok_or(Error::NotFound)
    }

    /// List all retained version numbers for one ID, newest first.
    ///
    /// # Errors
    /// Returns [`Error::NotFound`] for an unknown ID. Empty IDs and database
    /// read failures are errors.
    pub fn history(&self, id: &str) -> Result<Vec<i64>> {
        validate_id(id)?;
        let mut statement = self
            .connection
            .prepare("SELECT version FROM versions WHERE id = ?1 ORDER BY version DESC")?;
        let versions: Vec<i64> = statement
            .query_map([id], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        if versions.is_empty() {
            return Err(Error::NotFound);
        }
        Ok(versions)
    }

    /// Check `SQLite` integrity without reading content into the result.
    ///
    /// # Errors
    /// Returns an error when `SQLite` reports corruption or cannot perform the read.
    pub fn check(&self) -> Result<()> {
        let integrity: String = self
            .connection
            .query_row("PRAGMA quick_check", [], |row| row.get(0))?;
        if integrity != "ok" {
            return Err(Error::Integrity);
        }
        Ok(())
    }
}

impl Writer {
    /// Create empty state, finish an interrupted empty initialization, or open
    /// compatible existing state. Existing versions are preserved.
    ///
    /// # Errors
    /// Returns an error for an invalid or nonprivate path, foreign or unsupported
    /// state, storage failure, or lock contention beyond the `SQLite` busy wait.
    pub fn initialize(database: impl AsRef<Path>) -> Result<Self> {
        let database = database.as_ref();
        let parent = parent_directory(database)?;
        if !parent.exists() {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)?;
        }
        check_parent(parent)?;
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(database)
        {
            Ok(file) => file.sync_all()?,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        let mut connection = connect(database, true)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let identity = database_identity(&transaction)?;
        if identity == (0, 0) {
            let objects: i64 = transaction.query_row(
                "SELECT count(*) FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%'",
                [],
                |row| row.get(0),
            )?;
            if objects != 0 {
                return Err(Error::ForeignDatabase);
            }
            transaction.execute_batch(include_str!("schema.sql"))?;
        } else {
            check_identity(&transaction)?;
        }
        transaction.commit()?;
        fs::File::open(parent)?.sync_all()?;
        Ok(Self { connection })
    }

    /// Open existing state for explicit appends, without initialization.
    ///
    /// # Errors
    /// Returns an error for missing, foreign, unsupported, inaccessible, or
    /// nonprivate state. No files or directories are created.
    pub fn open(database: impl AsRef<Path>) -> Result<Self> {
        let connection = connect(database.as_ref(), true)?;
        check_identity(&connection)?;
        Ok(Self { connection })
    }

    /// Append complete content and return the committed record.
    ///
    /// Every call appends, including identical content. Version numbers start at
    /// one independently for each ID. Concurrent writers allocate versions inside
    /// one `SQLite` transaction. After uncertain completion, inspect history before
    /// repeating an update: updates have no idempotency key.
    ///
    /// # Errors
    /// Empty IDs, exhausted version numbers, storage errors, and lock contention
    /// beyond the `SQLite` busy wait are errors. Content may be empty or contain
    /// arbitrary UTF-8, including whitespace and NUL characters.
    pub fn update(&mut self, id: &str, content: &str) -> Result<Record> {
        validate_id(id)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let latest: i64 = transaction.query_row(
            "SELECT COALESCE(MAX(version), 0) FROM versions WHERE id = ?1",
            [id],
            |row| row.get(0),
        )?;
        let version = latest.checked_add(1).ok_or(Error::VersionExhausted)?;
        transaction.execute(
            "INSERT INTO versions(id, version, content) VALUES (?1, ?2, ?3)",
            params![id, version, content],
        )?;
        transaction.commit()?;
        Ok(Record {
            id: id.to_owned(),
            version,
            content: content.to_owned(),
        })
    }
}

fn validate_id(id: &str) -> Result<()> {
    if id.is_empty() {
        return Err(Error::EmptyId);
    }
    Ok(())
}

fn parent_directory(database: &Path) -> Result<&Path> {
    if !database.is_absolute() || database.file_name().is_none() {
        return Err(Error::InvalidPath);
    }
    database.parent().ok_or(Error::InvalidPath)
}

fn check_parent(parent: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(parent)?;
    if !metadata.is_dir() || metadata.permissions().mode() & 0o077 != 0 {
        return Err(Error::InvalidPath);
    }
    Ok(())
}

fn connect(database: &Path, writable: bool) -> Result<Connection> {
    check_parent(parent_directory(database)?)?;
    let metadata = fs::symlink_metadata(database)?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o077 != 0 {
        return Err(Error::InvalidFile);
    }
    let flags = if writable {
        OpenFlags::SQLITE_OPEN_READ_WRITE
    } else {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    };
    let connection = Connection::open_with_flags(database, flags)?;
    connection.busy_timeout(Duration::from_secs(5))?;
    connection.execute_batch("PRAGMA temp_store = MEMORY;")?;
    if writable {
        connection.execute_batch("PRAGMA synchronous = FULL;")?;
    } else {
        connection.execute_batch("PRAGMA query_only = ON;")?;
    }
    Ok(connection)
}

fn database_identity(connection: &Connection) -> Result<(i64, i64)> {
    Ok((
        connection.pragma_query_value(None, "application_id", |row| row.get(0))?,
        connection.pragma_query_value(None, "user_version", |row| row.get(0))?,
    ))
}

fn check_identity(connection: &Connection) -> Result<()> {
    if database_identity(connection)? != (APPLICATION_ID, SCHEMA_VERSION) {
        return Err(Error::UnsupportedDatabase);
    }
    Ok(())
}
