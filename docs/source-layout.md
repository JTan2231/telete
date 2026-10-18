# Source layout

Telete is one standalone Rust package. `Cargo.toml` declares its dependencies and
`Cargo.lock` pins their versions. Build the command with
`cargo build --locked --bin telete`.

`src/main.rs` defines the CLI. The manager coordinates the queue lifecycle; the
store holds runtime state; Git helpers create exact-commit private checkouts.
Invocation, process, and broker modules resolve commands, supervise execution,
and correlate results. Path helpers manage private local storage. The workflow
module validates project JSON configuration and resolves its repository and
initial runtime paths. The repository queue module resolves the shared queue
binding in Git's common directory and creates development worktrees.

`ci.sh` selects this repository's `ci/workflow.json` project config, which points
to `.telete/` for runtime state. `ci/check.sh` runs formatting, Clippy, tests, and a
CLI build. After checks succeed, `ci/install.sh` builds the release binary and
installs it and the manual under `~/.local/`, or a selected `--prefix`. The same
installer supports bootstrapping directly. The tests under `src/` check queue
and execution behavior.

`Cargo.toml` owns the package version. Before committing a release,
`ci/bump-version.sh [patch|minor|major]` updates that version, its lockfile entry,
and the manual header together; patch is the default. Deployment installs the
version from the submitted commit without changing it.

Read [Operate Telete](queue-operate.md) for the command and execution contract.
