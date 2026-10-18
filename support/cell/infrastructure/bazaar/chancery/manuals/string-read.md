# Bazaar strings and version history

This feature owns exact string reads and complete version-number history through
the CLI and in-process Rust API. Use it for stored text or a pinned older value.
Use `bazaar.string.update` for appends and `bazaar.installation` for state setup
and integrity. These related references do not add documentation dependencies.

## Read through the CLI

Bazaar stores exact strings under caller-chosen IDs. Each ID has immutable
numbered versions. Read the latest value, an exact older value, or all version
numbers for one ID:

```sh
bazaar get example.prompt
bazaar get example.prompt --version 3
bazaar history example.prompt
bazaar --database /absolute/private/bazaar.sqlite3 get example.prompt
```

Get prints the ID, version, and complete content with readable labels. History
prints every retained version number in descending order. The default output
is text. Use the global `--json` flag for the supported machine interface:

```sh
bazaar get example.prompt --json
bazaar history example.prompt --json
```

JSON get returns `id`, `version`, and exact `content`. JSON history returns `id`
and `versions`. Success uses `{"schema_version":1,"ok":true,"data":...}` on
stdout. With `--json`, operational errors use `ok:false` with `error.detail`
on stdout and exit 1. Without the flag, operational errors use a text diagnostic
on stderr and exit 1. Invalid command syntax uses Clap's stderr diagnostic and
exit 2 in both modes. Rust reads and the JSON schema remain unchanged.

The default database is `~/.local/share/bazaar/bazaar.sqlite3`. Reads require an
initialized schema-one database. The absolute path must have a private regular
parent directory and a private regular database file. Group and other permissions
and symlinks at those two paths are refused. Read-only permissions are supported.
Reads never initialize or repair Bazaar state.

IDs are exact, nonempty, case-sensitive strings. Versions are positive integers
that start at one independently for each ID. Unknown IDs or versions fail.
Latest selects the greatest committed version visible to that SELECT. History
selects every committed version number visible to its SELECT. Separate reads
do not share a snapshot. Bazaar stores no timestamps.

## Read through Rust

Rust programs import `bazaar::api::{Reader, Record, Error}` and use
`bazaar::api::Result<T>` for typed results. Use
`bazaar::api::Reader::open(database)`, followed by
`get(id, None)`, `get(id, Some(version))`, or `history(id)`. Methods return
`Record` or `Vec<i64>` with typed `bazaar::api::Error` failures. The reader
opens SQLite read-only and exposes no write methods. It invokes no CLI.

`Record` has `id: String`, `version: i64`, and `content: String`. History returns
only numbers without loading content. An existing reader observes newly committed
versions in later queries. Each query has its own snapshot.

```rust
fn read(database: &std::path::Path) -> bazaar::api::Result<()> {
    let reader = bazaar::api::Reader::open(database)?;
    let latest = reader.get("example.prompt", None)?;
    let pinned = reader.get("example.prompt", Some(latest.version))?;
    let versions = reader.history("example.prompt")?;
    assert_eq!(latest, pinned);
    assert!(versions.contains(&latest.version));
    Ok(())
}
```

Unknown IDs or versions return `Error::NotFound`. Empty IDs and nonpositive
versions are errors. Invalid paths, nonprivate files, unsupported state,
I/O failure, and SQLite failure also return typed errors. `Reader::open` creates
no files. The `bazaar.installation` feature owns initialization and integrity
checks.

## Meaning, limits, and privacy

Raw string content is opaque UTF-8. Empty strings, whitespace, Unicode, and NUL
characters remain unchanged. Use `bazaar.prompts.prepare` for selected prompt
text and rendering. Products own model settings and execution.
A returned string grants no execution or disclosure authority.

Missing, foreign, or unsupported databases remain errors. Check the selected
path and initialize only when creation is intended. Do not recreate state to
conceal a failed read. Whole content strings and complete version lists use
memory proportional to their size. SQLite waits up to five seconds for
contention; no latency or capacity guarantee is supplied. A connection is not
shared concurrently between threads; callers can open independent handles.

Content and IDs may be private. Get discloses the selected content to its caller.
History discloses only the ID and version numbers. Protect output under the same
boundary as stored text. CLI dispatch separately attempts metadata-only Chancery
usage recording; errors preserve the Bazaar result. Direct API reads record no
usage and modify no database.
