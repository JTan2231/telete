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

Submit committed source for background execution:

```sh
telete --config telete.json submit
```

Add `--follow` to observe the submitted job. Ctrl-C stops observation without
cancelling work. Use `cancel JOB` for an explicit cancellation request.

No initialization command or pre-existing runtime directory is needed. The first
queue mutation creates runtime state on demand and binds Git's common directory
to the selected config's absolute `state` path. A fresh queue permits admission.
Telete rejects reusing a populated runtime directory with a different repository.

The private state directory holds the operational queue, private checkouts,
command records, output files, and Telete's temporary files. Ignore it in Git
when it is inside the project. State must be user-owned and private. It is
operational data, not project configuration or a permanent job archive.

`status`, `wait`, `gates`, and `maintenance status` do not initialize missing
runtime state. Without a shared queue binding, a missing runtime directory has
no queued work. If a binding exists but its runtime directory or queue database
is missing, operations fail and require restoration of the operational state.

## Create worktrees

```sh
telete worktree ../feature-a --config telete.json
```

`worktree PATH` resolves `HEAD` once in the configured repository, then creates a
new branch and Git worktree at that commit. A relative destination resolves
against the invocation's current directory. The branch name is the destination's
last normalized path component and must be a valid literal Git branch name:
`../feature-a` creates `feature-a`. An existing destination or branch is an
error; Telete does not adopt an existing worktree. Uncommitted
changes stay in the source checkout. This command uses Git alone: it does not
submit work, inspect runtime state, bind a queue, or coordinate with a worker.
Its result contains only `path`, `branch`, and `commit`.

## Share a queue

The first queue mutation binds the Git common directory to the selected config's
absolute `state` path. Telete keeps the private binding at
`G/telete/queue.json`, where `G` is Git's common directory. From then on, the
original checkout, all linked worktrees, and all configs selecting those
checkouts use this one runtime directory. This includes manually created Git
worktrees. Separate clones have separate Git common directories and keep
independent queues.

Telete coordinates initial binding and queue ownership internally. Development
worktree creation remains independent of the queue before and after binding.

After binding, `state` remains a required config field but cannot redirect the
queue. A copied config's relative `.telete` path therefore does not create a
second queue in its checkout. Telete rejects conflicting existing runtime state
rather than combining queues.

The selected config still determines the source repository, commands, and
parameters. Use the config inside each checkout when its `repo` is relative:

```sh
telete --config ../feature-a/telete.json submit
telete --config ../feature-b/telete.json submit
telete --config telete.json status
```

These submissions join the same queue and retain their own selected commits,
workflows, and parameters. Passing the original checkout's config while standing
in a child still selects the original repository's `HEAD`. Queue order is the
order submissions are admitted to the shared queue.

Queue operation results include the effective absolute runtime directory in the
top-level `state_root` field. Preserve the shared Git repository, queue binding,
and runtime directory while work is queued, active, or unresolved.
If the binding survives but its runtime directory or queue database is missing,
restore that operational state before continuing. Telete refuses to create an
empty replacement queue behind an existing binding.

## Select source and commands

`submit [REVISION]` defaults to `HEAD` in the configured repository. Submission
resolves the revision once to a full commit ID and freezes the configured
workflow and parameters. Uncommitted checkout changes are not submitted. Later
config or HEAD changes do not change the submitted job.

Before reporting successful submission, Telete retains the commit and creates
its private execution checkout. Removing the submitting linked worktree does
not change this retained source; the shared Git repository and runtime must
remain available.

Submission arranges background execution without waiting for the job to finish.
Use `submit --follow` to observe the new job as well. Following does not own the
worker, and closing the terminal or interrupting observation does not cancel
submitted work.

Submission prints its job receipt before following. If worker startup fails
after the job was admitted, the receipt still identifies that job and includes
`worker_error`; the command then reports an error and exits unsuccessfully.
Inspect the admitted job rather than submitting it again. With `--follow`, a
second JSON result reports the final observation.

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

Commands inherit the managed worker's environment at launch and use ordinary
permissions, with their configured overrides and Telete context. The supervisor
inherits the environment of the invocation that starts it, and its workers
inherit that environment. Jobs joining an existing worker use that worker's
environment. Telete does not select a toolchain,
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

Telete manages a background worker while unfinished work exists. The worker
waits while pause, maintenance holds, or unresolved work prevent progress, and
exits after the queue settles and cleanup finishes. Retained failure diagnostics
and settled-job markers alone do not keep a worker running. Users do not start,
stop, or restart workers.

A private supervisor restarts its worker if it exits unexpectedly while work
remains. Both processes exit after settlement and cleanup. This is not a login
service: if the supervisor itself is lost, or after logout or reboot, the next
queue mutation arranges execution again. Read-only observation does not restart
execution. Restarting a worker does not authorize another launch of an uncertain
command.

Use `status` or `status JOB` to inspect work, and `wait JOB` to observe a job
without submitting it again. Waiting has no timeout by default; supply
`--timeout 60` to limit observation. Both `wait` and `submit --follow` stop
observing when the job succeeds, fails, is cancelled, or becomes blocked.
Ctrl-C stops observation without cancelling the job. Add `--json` for compact
JSON output and structured operation errors. `wait` and `submit --follow` exit 0
only when the observed job succeeded; failure, cancellation, blockage, and an
observation timeout use exit 1. Ctrl-C uses exit 130. Ordinary operation failures
use exit 1; CLI syntax errors use exit 2.

For a bound repository, these operations act on the shared queue from every
linked checkout. Only one worker can own it. Pause, maintenance holds,
cancellation, recovery, and retained diagnostics are shared too. An unresolved
command blocks later jobs from every checkout.

`pause` prevents new claims. `resume` permits claims when no maintenance hold or
unresolved work blocks admission. `cancel JOB` requests an intended stop at a
safe boundary. It cannot establish completion or discard uncertain effects of
an admitted command.

Use `recover JOB` to reconcile retained work. Recovery uses the saved command,
context, and terminal evidence. An existing execution identity can join only an
equal command, including context, explicit overrides, and timeout. Process disappearance
does not establish completion. Telete coordinates recovery with its worker and
continues remaining eligible work automatically; no worker restart is required.

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
These retention limits apply to the shared queue as a whole, not separately to
each worktree.

## Coordinate maintenance

Use `maintenance hold --owner OWNER`, `maintenance status`, and
`maintenance release --owner OWNER` to coordinate queue admission during
maintenance. Release only the owner acquired by that operation. A release does
not clear operator pause or resolve an interrupted job. Pausing admission does
not drain the queue. Resuming or releasing the final hold allows eligible work
to continue automatically.
