# Telete

Telete runs a serial queue for committed source on macOS. A project JSON config
selects the repository, initial private runtime directory, check command, and
optional preparation and deployment commands. Each job runs one exact commit in
a private checkout.

Install the release binary and manual for your user account:

```sh
./ci/install.sh
```

The installer builds with Cargo and installs `~/.local/bin/telete` and
`~/.local/share/man/man1/telete.1`. It replaces the binary atomically after a
successful build. Use `./ci/install.sh --prefix /path/to/prefix` to install into
another prefix's `bin/` and `share/man/man1/` directories.

Add this line to your shell profile (for example, `~/.zshrc`) once, then open a
new shell or run the line in your current shell:

```sh
export PATH="$HOME/.local/bin:$PATH"
```

You can then invoke `telete` from any directory. Project operations still select
their configuration explicitly:

```sh
telete --config /path/to/project/telete.json status
```

Build the development CLI, then run this repository's workflow:

```sh
cargo build --locked --bin telete
./ci.sh submit --follow
```

`submit` defaults to the project's current `HEAD`. Supply a revision to run
another commit. Submission starts execution in the background; `--follow` also
observes the job until it settles or becomes blocked. Press Ctrl-C to stop
following without cancelling the job. No initialization or prior runtime
directory is required.

Create a development worktree and a branch named after its destination:

```sh
telete worktree ../feature-a --config ci/workflow.json
```

The branch starts at the configured repository's `HEAD` when invoked. Worktree
creation is independent of the queue and does not create or inspect runtime
state. The first queue mutation binds the Git common directory to the selected
runtime directory. The original checkout and all linked worktrees then share
that queue, including worktrees created with Git. Submit from each checkout using
its corresponding config to select its own commit and workflow. Separate clones
keep separate queues. Queue operation results report the effective runtime
directory as `state_root`.

The wrapper selects `ci/workflow.json`, which points to this repository and
`.telete/`. Its check stage runs formatting, Clippy, tests, and a CLI build; its
deployment stage runs `ci/install.sh` only after checks succeed. Both stages run
the submitted commit. Set `TELETE_BIN` to an absolute built executable when using
another target directory. Run `./ci.sh status` or `./ci.sh wait JOB` in another
terminal to inspect work. `wait` follows indefinitely unless given `--timeout`.
Use `cancel JOB` to request cancellation explicitly. Telete manages the worker
while unfinished work exists and stops it after the queue settles and cleanup
finishes.

Before committing a release, bump its version:

```sh
./ci/bump-version.sh          # patch: 0.1.0 -> 0.1.1
./ci/bump-version.sh minor    # minor: 0.1.0 -> 0.2.0
./ci/bump-version.sh major    # major: 0.1.0 -> 1.0.0
```

The helper updates the package version in `Cargo.toml`, the package's lockfile
entry, and the manual together. Minor and major bumps reset lower components.
Review and commit the changes before submitting: deployment installs the
committed version without bumping it. Check the installed version with
`telete --version`. Use the Telete version that owns existing runtime state;
incompatible runtime schemas are rejected rather than migrated automatically.

For another project, run `telete --config path/to/telete.json submit`, optionally
with `--follow`. Read the
[queue contract](docs/queue-operate.md) for configuration, execution, and recovery.
The [source layout](docs/source-layout.md) describes the standalone Rust package.

The [telete(1) manual](docs/telete.1) covers commands, configuration, execution
context, exit statuses, files, recovery, and examples. View it from this checkout:

```sh
man ./docs/telete.1
```

The installer also installs the manual. If your system does not find it:

```sh
man -M "$HOME/.local/share/man" telete
```
