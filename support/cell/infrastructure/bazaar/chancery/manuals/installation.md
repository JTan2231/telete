# Bazaar installation and private state

Submit deployments from the Cell root with
`./ci.sh submit COMMIT --deploy bazaar` and verify the retained manager job
outcome. CI and Telete are the deployment route. The installer APIs below are
underlying product setup and explicit retained-release recovery interfaces.

This feature owns program publication, database initialization, integrity checks,
and compatible recovery. Use `chancery show bazaar.install.operate` for the
procedure. Use `chancery resolve bazaar.install.operate` to read that procedure
with this complete contract.

Bazaar is an on-demand CLI and in-process Rust library. It installs no daemon,
schedule, network listener, or model requester. The current user owns its
programs and private SQLite state. There is no application-level authentication.

## Program publication

The installer retains the supplied programs and provider bundle in a release
archive. It publishes regular executable files beneath
`~/Library/Application Support/Bazaar/install/runtime/`. Updates and recovery
replace those files at the same actual paths. Public command selectors use this runtime tree. Provider directory selectors
use the selected archive, so each catalog read selects one retained bundle. The installer uses product and catalog locks;
each file replacement is atomic. The complete tree is not one atomic update.
`--expected-current absent|releases/ID` guards the recorded release selection.
Foreign public selectors are refused. An instruction failure retains completed
file and selector changes for explicit recovery.

Opaque UUID release IDs name retained archives. Installation and recovery do not compare
artifact hashes, component versions, or retained file inventories. They do not
run executable probes, native-signature audits, database integrity checks,
dependency probes, or readiness checks. Inspection reads recorded installation
metadata and selectors; it is not an integrity result.

The release contains `bazaar`, `bazaar-install`, its recovery installer, and the
Bazaar provider. Releases live under
`~/Library/Application Support/Bazaar/install/releases/ID`.
The archive UUID identifies retained bytes for recovery. It is not part of the
running program pathname.

```sh
bazaar-install install --binary ABSOLUTE_BINARY --bundle ABSOLUTE_BUNDLE
bazaar-install inspect
bazaar-install recover --release ABSOLUTE_RELEASE_DIRECTORY
```

Direct installation selects programs only. The Bazaar deployment recipe initializes the
default database through `Writer::initialize`; it adds no integrity or readiness
gate. Initialization's ordinary state and schema rules still apply. The separate
`bazaar doctor` command remains available for an explicit diagnostic request.
The manifest executor runs declared instructions in order and uses exit status only.

## State identity and access

The default database is `~/.local/share/bazaar/bazaar.sqlite3`. Global
`--database ABSOLUTE_PATH` selects another database for ordinary CLI commands.
Rust programs use `bazaar::database_path(home)` to obtain the default path.
Deployment configures only the default database.

The path must be absolute. The immediate parent must be a private regular
directory, and the database must be a private regular file. Group and other
permissions and symlinks at those two paths are refused. Initialization creates
missing directories with mode 0700 and the database with mode 0600. Readers can
use mode 0500 directories and mode 0400 database files.

Only Bazaar database schema one and version-two installation packages are
supported. Database schema, CLI output schema, string version, entry contract
version, and product release are independent identities. Unsupported state
remains an error; no state migration or compatibility window is supplied.

The single application table contains `id`, `version`, and `content`. Its
compound primary key identifies each immutable version. SQLite stores the
application ID and schema version separately. No timestamp, configuration
schema, selected-version pointer, or content digest is stored. This description
does not make arbitrary direct SQL a supported interface.

## Initialization and integrity checks

```sh
bazaar init
bazaar doctor
bazaar --database /absolute/private/bazaar.sqlite3 init
bazaar --database /absolute/private/bazaar.sqlite3 doctor
```

Init creates schema in empty state and can finish an interrupted empty
initialization. It preserves compatible versions. Nonempty foreign databases
and unsupported identities fail without alteration. Initialization creates no
string versions and validates no stored content format.

Doctor opens state read-only and checks database identity and SQLite
`quick_check`. It returns no stored content. It establishes neither the meaning
of stored text nor future availability. Init and doctor print short readable
results by default. Use the global `--json` flag for their existing schema-one
JSON on stdout. With that flag, operational errors use `ok:false` with
`error.detail` on stdout and exit 1. Without it, operational errors use a text
diagnostic on stderr and exit 1. Invalid command syntax uses Clap's stderr
diagnostic and exit 2 in both modes. Rust state operations remain unchanged.

Rust programs import `bazaar::api::{Reader, Writer, Error}` and use
`bazaar::api::Result<T>` for typed results:

```rust
fn initialize(database: &std::path::Path) -> bazaar::api::Result<()> {
    let _writer = bazaar::api::Writer::initialize(database)?;
    bazaar::api::Reader::open(database)?.check()
}
```

Only `Writer::initialize` creates state. `Reader::open` and `Writer::open`
require initialized state and create no files. `Reader::check` returns `()` or
a typed error and reads no content into its result. These methods run in process
and invoke no CLI, daemon, or model. Direct API calls record no CLI usage.

Invalid paths, nonprivate files, unsupported or foreign state, integrity failure,
I/O failure, and SQLite failure are typed API errors. SQLite waits up to five
seconds for contention. No capacity, throughput, or hard latency guarantee is
supplied. Open separate handles when callers need concurrent connections;
a connection is not shared concurrently between threads.

## Recovery and retention

Retained-release recovery preserves the separate database. It never restores
or rewrites string versions. Compatible commands can finish across program
selection. A program rollback does not restore older data or make unsupported
state compatible.

Every committed string version remains retained. There is no pruning, deletion,
or data restoration command.

Private stored content stays outside source and release files. The reviewed
repository prompt seed is an explicit import artifact, not runtime state.
Installation,
initialization, inspection, and usage registration authorize no content update,
caller migration, external send, or agent execution. `bazaar --register-usage`
separately registers command identities in Chancery and creates no strings.
CLI dispatch attempts metadata-only usage recording; errors preserve the Bazaar
result. No installed catalog is required for core state operations.

Read `bazaar.string.read` for exact content and history queries. Read
`bazaar.string.update` for append transactions and uncertain update recovery.
Read `bazaar.prompts.prepare` for selected prompt reads and rendering. Read
`bazaar.prompts.import` for explicit content imports. Installation never runs
that importer. Those references are related reading, not required documentation
dependencies.

## Deployment recipe

`bazaar-install deploy` reads one schema-two Cell recipe request from stdin.
The product command selects supplied programs and initializes compatible default state without adding string versions.
The manifest executor runs this instruction and records its exit status. It does
not inspect application output or create a maintenance hold, drain work, or
recover prior effects. A failed instruction leaves completed changes in place.
Use the product's explicit recovery operation when recovery is required.
