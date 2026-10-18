# Operate Telete

Telete runs a serial queue for committed repository source on macOS. The project
config selects one required check command and optional preparation and deployment
commands. Telete owns the queue and execution records. Configured commands own
the meaning of their checks, outputs, and deployment effects.

## Select a project

Every public command takes `--config` naming a project JSON file. The config path
can be absolute or relative to the current directory. Its `repo` and `state`
paths are required; relative values resolve against the config file's directory.
Paths are normalized to absolute paths.

```json
{
  "repo": ".",
  "state": ".telete",
  "check": {
    "program": "ci/check.sh",
    "args": [],
    "env": {},
    "timeout_seconds": null
  },
  "parameters": {"suite": "all"}
}
```

Submit committed source and start a foreground worker:

```sh
telete --config telete.json submit
telete --config telete.json worker
```

No initialization command or pre-existing runtime directory is needed. Mutating
operations create runtime state on demand, and a fresh queue permits admission.
Use the same configured state directory to operate that queue. Telete rejects
reusing a populated runtime directory with a different repository.

The private state directory holds the operational queue, private checkouts,
command records, output files, and Telete's temporary files. Ignore it in Git
when it is inside the project. State must be user-owned and private. It is
operational data, not project configuration or a permanent job archive.

`status`, `wait`, `gates`, and `maintenance status` do not initialize missing
runtime state. A missing runtime directory has no queued work.

## Select source and commands

`submit [REVISION]` defaults to `HEAD` in the configured repository. Submission
resolves the revision once to a full commit ID and freezes the configured
workflow and parameters. Uncommitted checkout changes are not submitted. Later
config or HEAD changes do not change the submitted job.

Each job runs that exact commit in a clean private checkout. Telete does not
merge source, maintain an accepted baseline, accept commits, or advance project
branches. The execution context identifies one commit throughout the workflow.

The supported workflow fields are:

| Field | Default | Meaning |
| --- | --- | --- |
| `check` | required | Check the selected commit. |
| `prepare` | absent | Run after a successful check. |
| `deploy` | absent | Run after successful preceding stages. |
| `parameters` | `{}` | Pass the supplied object unchanged to commands. |

The order is check, optional preparation, and optional deployment. Deployment can
be selected without preparation. A stage that was not selected does not become
a successful execution. Unknown config and command fields are rejected.

Every command accepts `program`, optional literal `args`, optional `env`
overrides, and optional `timeout_seconds`. Arguments default to an empty array
and overrides to an empty object. An omitted or null timeout is unbounded; a
supplied timeout must be positive. Every command runs in the private checkout.
Programs can be absolute executable paths or normalized paths relative to that
checkout. These rules apply equally to all stages. The selected config
authorizes the commands it names.

Arguments and environment values are literal. Select a shell explicitly when
shell interpretation is needed. Environment names must start with an ASCII
letter or underscore and contain only ASCII letters, digits, or underscores.
Programs, arguments, and environment values cannot contain NUL. `TELETE_` names
are reserved for Telete; other environment variables belong to the project.

Commands inherit the environment at launch and use ordinary permissions, with
their configured overrides and Telete context. Telete does not select a toolchain,
rewrite home or cache locations, impose a filesystem sandbox, or promise
reproducible execution. If the project needs a particular environment, cache,
compiler target directory, or credential reference, select it in its commands
and configuration. Environment values are not resolved relative to the config
file and are not expanded by Telete.

## Command results and context

Check success requires a successful command completion for the selected commit.
Preparation and deployment success likewise require successful completion of
their selected commands. A failed stage stops the job. Commands own output
formats and any stronger claim about the systems they change.

Before launching a command, Telete records its execution identity, selected
executable, arguments, working directory, explicit environment overrides, timeout,
and context. The inherited environment is not frozen or retained.
`TELETE_CONTEXT` names the private JSON context file. Recorded context must match
before launch. Retaining the exact command lets recovery reconcile the same
execution instead of constructing another command from current settings.

The context contains the job and operation identity, stage, checkout,
`source.commit`, parameters, inputs, and outputs. `outputs.directory` names the private output
directory for the command. When preparation was selected and succeeded,
`inputs.preparation` contains its absolute output directory; otherwise it is
null. Telete
defines no package or installation format.

A missing command input closes standard input. A known terminal exit remains
known if output collection fails; collection failure is a separate observation.
A program that fails to start has a recorded start-error result. Neither failure
authorizes another launch under the same execution identity.

## Operate and recover work

The foreground worker continues until stopped. `worker --once` advances one
lifecycle tick; it does not complete a whole job. Use `status` or `status JOB`
to inspect work, and `wait JOB --timeout 60` to observe a job without submitting
it again. Add `--json` for compact JSON output and structured operation errors.
Ordinary operation failures use exit 1; CLI syntax errors use exit 2.

`pause` prevents new claims. `resume` permits claims when no maintenance hold or
unresolved work blocks admission. `cancel JOB` requests an intended stop at a
safe boundary. It cannot establish completion or discard uncertain effects of
an admitted command.

Use `recover JOB` to reconcile retained work. Recovery uses the saved command,
context, and terminal evidence. An existing execution identity can join only an
equal command, including context, explicit overrides, and timeout. Process disappearance
does not establish completion.

An interrupted command without terminal evidence remains unresolved. Telete does
not automatically repeat or roll back its effects. Preserve the runtime
directory while work is active or unresolved. Deleting it loses the evidence
needed to distinguish completed effects from uncertain ones.

The broker serializes command execution. Equal gate identities join the same
recorded result only for the same command. Missing terminal evidence blocks the
resource slot. `gates` exposes current diagnostic records.

Diagnostics are for current operation, not historical browsing. The latest failed
workflow retains its details and private checkout until a newer workflow
finishes successfully or fails. Cancellation does not supersede that failure.
Successful and cancelled jobs release their scratch files and full execution
records; superseded failures do likewise.

Telete retains at most 128 small settled-job markers containing only job ID,
phase, queue sequence, and update time. Markers contain no commit, command,
parameters, environment, or output. Old markers are pruned, and `wait JOB` then
reports that the job is unavailable. Archive any project outputs through the
configured commands when they need a longer lifetime.

## Coordinate maintenance

Use `maintenance hold --owner OWNER`, `maintenance status`, and
`maintenance release --owner OWNER` to coordinate queue admission during
maintenance. Release only the owner acquired by that operation. A release does
not clear operator pause or resolve an interrupted job. Pausing admission does
not drain the queue or stop its worker.
