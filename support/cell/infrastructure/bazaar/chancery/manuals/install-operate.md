# Install and operate Bazaar

Use this procedure to install or recover Bazaar programs, initialize empty
private state, or inspect installation metadata. Read
`chancery resolve bazaar.install.operate` for this procedure with the required
`bazaar.installation` feature contract. That feature owns detailed package,
state, access, compatibility, and recovery guarantees.

Bazaar installs no daemon, schedule, or model requester. The procedure creates
no string versions and performs no caller migration. Installation changes owned
program and provider selectors. Initialization can create empty private state.
Read-only checks change neither selection nor stored versions.

## Select the operation and prerequisites

1. Select program installation, empty-state initialization, a read-only check,
   or exact retained-release recovery. Obtain authorization for the selected
   changes. Keep content updates and data restoration separate.
2. Select the database. Ordinary CLI commands default to
   `~/.local/share/bazaar/bazaar.sqlite3`; global `--database ABSOLUTE_PATH`
   selects another private database. Deployment configures only the default.
3. For a database operation, check that any existing immediate parent and database
   are regular and private. Group and other permissions and symlinks at those
   two paths are refused. Initialization can create absent paths and requires
   empty or compatible state. Database checks require initialized schema-one
   state; program-only checks do not. Stop for foreign or unsupported state.
   Do not recreate it to conceal a failed check.
4. Select an authorized program and provider candidate for
   installation, or an exact owned retained release for program recovery.
   Stop for foreign selectors, or stale expected selection.

The current user owns Bazaar programs and state. Telete runs declared deployment
instructions in order; Bazaar owns selection and setup. Product installer APIs
remain available for setup and explicit recovery. Installation and inspection grant no authority
to append content, migrate callers, delete history, send externally, or run an
agent. Protect private stored content outside source and release files.

## Install programs

Submit the selected committed source from the Cell root:

```sh
./ci.sh submit COMMIT --deploy bazaar
```

Verify the retained manager outcome. The manager integrates, validates, attempts
bounded repairs, deploys, and emails the outcome. CI and its Telete manager are
the deployment route. Bazaar accepts no deployment settings or
runtime service dependencies. The Bazaar deployment recipe initializes an empty default
database or checks its existing schema. Installation runs no separate artifact-integrity, database-integrity, or
readiness gate. Preserve unresolved installation evidence after an I/O failure.

The underlying program-selection API accepts a supplied candidate:

```sh
bazaar-install install --binary /absolute/candidate/bazaar --bundle /absolute/bazaar/chancery
bazaar-install inspect
```

Use `--home ABSOLUTE_PATH` and `--expected-current absent|releases/ID` when
an explicit installation home or guarded selection is required. Direct
installation selects programs only. Initialize state separately when creation
is intended; do not treat successful program selection as database readiness.

## Initialize and verify state

1. Initialize only the intended empty or compatible database:

   ```sh
   bazaar init
   ```

   Initialization creates missing directories with mode 0700 and the database
   with mode 0600. It preserves compatible versions and can finish interrupted
   empty initialization. Stop if the database is foreign or unsupported.
2. Check its identity and integrity:

   ```sh
   bazaar doctor
   ```

   Doctor is read-only and returns no stored content. A successful check confirms
   the selected schema-one database and SQLite integrity at that observation.
   It does not establish content meaning or future availability.
3. Register the installed command inventory separately:

   ```sh
   bazaar --register-usage
   ```

   Registration adds command identities in Chancery and creates no strings.
4. If the Chancery reader is installed, confirm the matching installed overview
   and procedure:

   ```sh
   chancery product bazaar
   chancery show bazaar.install.operate
   chancery resolve bazaar.install.operate
   ```

   Chancery reads documentation; it establishes no database or program readiness.
   Bazaar installation and core state operations remain useful without that
   executable. Program inspection reads installation metadata without checking artifact integrity.

Use the global `--database` option for initialization and doctor on another
absolute private database. Init and doctor print readable results by default.
Add `--json` to request schema-one JSON. Operational errors exit 1 and use text
on stderr by default, or `ok:false` with `error.detail` on stdout with `--json`.
Invalid command syntax uses a stderr diagnostic and exit 2 in both modes. Treat
either failure as an unsuccessful step.

## Inspect and recover programs

Inspect the selected installation without changing programs:

```sh
bazaar-install inspect
```

Recover only the intended compatible retained program release:

```sh
bazaar-install recover --release /absolute/owned/release
bazaar-install inspect
```

Read selected program/provider metadata after recovery. Database doctor remains
a separate explicit diagnostic operation.
Program-only recovery can preserve an absent database. Recovery preserves the
separate database and never rewrites string versions.
Only schema-one state and version-two installation packages are supported.
Stop if the selected program cannot read current state. Preserve unresolved
installation evidence rather than deleting it or changing stored history.

## Inspect an uncertain append

An uncertain append may have committed. Inspect history and relevant content
through `bazaar.string.read` before deciding whether another append is intended.
`bazaar.string.update` owns that recovery behavior. Program recovery does not
resolve an uncertain content receipt or authorize another append.

## Deployment recipe

`bazaar-install deploy` reads one schema-two Cell recipe request from stdin.
The product command selects supplied programs and initializes compatible default state without adding string versions.
The manifest executor runs this instruction and records its exit status. It does
not inspect application output or create a maintenance hold, drain work, or
recover prior effects. A failed instruction leaves completed changes in place.
Use the product's explicit recovery operation when recovery is required.
