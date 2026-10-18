# Source layout

Telete is one standalone Rust package. `Cargo.toml` declares its dependencies and
`Cargo.lock` pins their versions. Build the command with
`cargo build --locked --bin telete`.

`src/main.rs` defines the CLI. The manager and workflow runner coordinate the
queue lifecycle; the store retains its journal; Git helpers own private candidate
operations. Invocation, process, and broker modules freeze commands, supervise
execution, and retain correlated results. Path helpers enforce private local
storage. The workflow module validates the caller's JSON configuration.

`ci.sh` selects this repository's `.telete/` state and `ci/workflow.json` workflow.
`ci/check.sh` runs formatting, Clippy, tests, and a CLI build. The tests under
`src/` check queue and execution behavior.

Read [Operate Telete](queue-operate.md) for the command and execution contract.
