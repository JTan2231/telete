# Telete

Telete runs a serial queue for committed source on macOS. A workflow selects a
check command and optional integration, preparation, acceptance, and deployment
stages. The foreground worker retains exact source identities, frozen commands,
and their results in a private state directory.

Build the CLI, then run this repository's workflow:

```sh
cargo build --locked --bin telete
./ci.sh init
./ci.sh resume
./ci.sh submit HEAD
./ci.sh worker
```

The wrapper selects `.telete/` and `ci/workflow.json`. Set `TELETE_BIN` to an
absolute built executable when using another target directory. Run `./ci.sh status`
or `./ci.sh wait JOB` in another terminal to inspect work. `worker --once` advances
one lifecycle tick, not a complete job.

For another repository, supply `--state`, initialize with `--repo`, and select
an absolute workflow JSON path when submitting. Read the
[queue contract](docs/queue-operate.md) for setup, workflow configuration, retained
outcomes, and recovery. The [source layout](docs/source-layout.md) describes the
standalone Rust package.
