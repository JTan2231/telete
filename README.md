# Telete

Telete runs a serial queue for committed source on macOS. A project JSON config
selects the repository, private runtime directory, check command, and optional
preparation and deployment commands. Each job runs one exact commit in a private
checkout.

Build the CLI, then run this repository's workflow:

```sh
cargo build --locked --bin telete
./ci.sh submit
./ci.sh worker
```

`submit` defaults to the project's current `HEAD`. Supply a revision to run
another commit. No initialization or prior runtime directory is required.

The wrapper selects `ci/workflow.json`, which points to this repository and
`.telete/`. Set `TELETE_BIN` to an absolute built executable when using another
target directory. Run `./ci.sh status` or `./ci.sh wait JOB` in another terminal
to inspect work. `worker --once` advances one lifecycle tick, not a complete job.

For another project, run `telete --config path/to/telete.json submit` and
`telete --config path/to/telete.json worker`. Read the
[queue contract](docs/queue-operate.md) for configuration, execution, and recovery.
The [source layout](docs/source-layout.md) describes the standalone Rust package.

The [telete(1) manual](docs/telete.1) covers commands, configuration, execution
context, exit statuses, files, recovery, and examples. View it from this checkout:

```sh
man ./docs/telete.1
```

To install the manual for your user account:

```sh
install -d "$HOME/.local/share/man/man1"
install -m 0644 docs/telete.1 "$HOME/.local/share/man/man1/telete.1"
man -M "$HOME/.local/share/man" telete
```
