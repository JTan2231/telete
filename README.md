# Telete

Telete runs a serial queue for committed source. Repository configuration selects
the check command and optional integration, preparation, acceptance, deployment,
repair, and notification stages. A foreground worker needs no service installation.

Build the CLI, then use this repository's workflow:

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
one lifecycle tick, not a complete CI job.

Read the [queue contract](chancery/manuals/queue-operate.md) for project setup,
command configuration, retained outcomes, and Cell compatibility. The
[source layout](docs/source-layout.md) identifies the copied dependencies.
