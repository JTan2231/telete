# Operate Telete

Telete runs a serial queue for committed repository source on macOS. The project
config selects one required check command and optional fix, analysis, repair,
preparation, and deployment commands. Telete owns the queue and execution
records. Configured commands own the meaning of their checks, outputs, and
deployment effects.

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

Each job starts from that exact commit in a clean private checkout. An optional
`fix` command edits tracked source before checks. Telete records those edits in
one direct child commit with `Telete <telete@localhost>` as both author and
committer. No changes produce no new commit. The command keeps a detached HEAD;
Telete stages existing tracked paths with its own index. New untracked files,
build outputs, and command staging are excluded.

Telete retains the resulting commit at `refs/telete/results/JOB`. It creates this
ref even when corrections produce no changes or the fix command fails. A failed
fix command stops the workflow after retaining its edits. Cancellation and
scratch cleanup preserve the ref. The ref belongs to source history and remains
until the project explicitly removes it. Telete does not merge or advance project
branches; the project can integrate the returned commit.

`commit` in job status remains the submitted SHA. When a workflow retains
source for fixes, analysis, repair, or a batch, `resulting_commit` and
`result_ref` identify that source. Subsequent commands run on the resulting
commit unless a batched release selects combined source below.
Commands after fixes must preserve HEAD and tracked source, except for automatic
repair and the selected batch composition stage. A violation fails the job.
Resuming the job consumes saved command evidence and reuses its result commit
instead of applying corrections again.

The supported workflow fields are:

| Field | Default | Meaning |
| --- | --- | --- |
| `fix` | absent | Apply tracked source corrections before checks. |
| `analyze` | absent | Analyze source once and trigger repair on ordinary findings. |
| `check` | required | Build or check the resulting source, including accepted repairs. |
| `repair` | absent | Run a script to create a committed repair from a failed command's output. |
| `prepare` | absent | Run after a successful check. |
| `deploy` | absent | Run after successful preceding stages. |
| `batch` | absent | Combine compatible deployment requests before preparation and deployment. |
| `parameters` | `{}` | Pass the supplied object unchanged to commands. |

The normal order is optional fixes, optional analysis, check, optional
preparation, and optional deployment. The repair command runs when eligible work
needs repair. Deployment can be selected without preparation. Unknown config and
command fields are rejected.

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

## Batch deployment requests

Select `batch` to run each submitted change's individual fixes, analysis, and
check, then combine compatible deployment requests into one release. A batch
requires `deploy`. Preparation and deployment run once on the combined commit;
Telete does not add another analysis or check.

```json
{
  "repo": ".",
  "state": ".telete",
  "check": {"program": "ci/check.sh"},
  "prepare": {"program": "ci/prepare.sh"},
  "deploy": {"program": "ci/deploy.sh"},
  "batch": {
    "target": "production",
    "base": "main",
    "compose": {"program": "ci/compose.sh"}
  }
}
```

`target` names the shared deployment destination, using one to 128 ASCII
letters, digits, underscores, or hyphens. `base` selects the initial release
source and is resolved to a commit at submission. After a successful batched
deployment, the next group uses that target's last successful release instead.
Telete pins the submitted bootstrap base at `refs/telete/jobs/base-JOB` until
request cleanup.
Use the same target for commands that deploy to the same destination. Compatible
requests have the same target, composition, preparation, deployment, and
parameters; their individual fixes, analysis, and checks may differ.

When the first member finishes its check, Telete closes the group over compatible
requests already admitted to the queue. Those requests finish their individual
CI before the shared release starts. An incompatible request for the same target
closes that compatible segment; later compatible requests cannot jump ahead of
it. Later submissions enter the next group,
even if the first group has not deployed yet. A group waiting for members does
not occupy the execution slot. A member awaiting repair keeps its group waiting
while other CI can run. Telete does not silently omit failed members or let a
later release for the same target overtake a pending one.

Cancelling any frozen member cancels the group at a safe boundary. Complete an
interrupted repair handoff on any member before cancelling. A group with a
failed member and a member awaiting repair stays pending until explicit
cancellation, preserving the repair evidence.

The first ready job identifies the release. The configured `compose` command
runs in its private checkout, starting at the group's release base. Its context
contains `inputs.batch.base` and `inputs.batch.members` in queue order. Every
member includes `job_id`, its final checked `commit`, `submitted_commit`,
`analyzed_commit`, `repaired_commit`, and `parameters`. Project commands own the
merge policy, conflicts, release identifiers, and package format.

The compose command must leave a clean, detached HEAD at the combined commit
and write that full commit ID to `outputs.directory/commit`. The commit must
descend from the release base and every member's checked source. Telete retains
it at `refs/telete/releases/RELEASE`, then runs the configured preparation and
deployment on it. These ancestry checks establish which source was included;
configured commands own the release's domain success claims. No project branch
is advanced.

For example, a project using `jq` can make this `ci/compose.sh` executable and
commit it in its base source:

```sh
#!/bin/sh
set -eu
jq -r '.inputs.batch.members[].commit' "$TELETE_CONTEXT" |
while IFS= read -r revision; do
    git -c user.name=Telete -c user.email=telete@localhost \
        -c core.hooksPath=/dev/null -c commit.gpgSign=false \
        merge --no-edit "$revision"
done
git rev-parse HEAD > "$(jq -r '.outputs.directory' "$TELETE_CONTEXT")/commit"
```

Status distinguishes each member's checked source from `release_commit`, and
`release_id` links members to their shared outcome. Members wait in
`waiting_release`; the leader runs `composing`, then preparation and deployment.
`wait` and `submit --follow` observe the requested deployment through that shared
outcome. Individual submitted, analyzed, and repaired identities remain intact.

Composition failure prevents deployment. The existing cancellation and command
recovery rules apply to release work too: missing terminal evidence never
authorizes replay. Release refs survive checkout cleanup, and
`refs/telete/releases/targets/TARGET` retains the latest confirmed release source.
Successful completion markers retain only individual source and shared release
identities. A shared failure retains the leader and actual failed-member
diagnostics until a newer workflow succeeds or fails. Preserve durable packages and
release records in project outputs.

Telete 0.5 reads earlier queue records. Let a 0.4 worker settle before submitting
batched workflows, and keep 0.5 or later while batching records remain.

## Command results and context

Check success requires a successful command completion for the selected commit.
Preparation and deployment success likewise require successful completion of
their selected commands. An ordinary analysis failure or an ordinary check
failure with analysis or repair configured enters the repair flow. Other
failures stop the job. Commands own output formats and any stronger claim about
the systems they change.

Before launching a command, Telete records its execution identity, selected
executable, arguments, working directory, explicit environment overrides,
timeout, any standard input, and context. The inherited environment is not
frozen or retained.
`TELETE_CONTEXT` names the private JSON context file. Recorded context must match
before launch. Retaining the exact command lets recovery reconcile the same
execution instead of constructing another command from current settings.

The context contains the job and operation identity, stage, checkout,
`source.commit`, parameters, inputs, and outputs. For a workflow with fixes,
analysis, repair, or batching,
`source.submitted_commit` preserves the submitted SHA and `source.result_ref`
names the retained result. `source.commit` identifies the submitted source in
the fix command and the resulting source in later individual commands. Batched
composition selects the release base; preparation and deployment select the
combined release source. `source.checked_commit` preserves the individual
source, and `source.release_id` identifies a frozen group.

`outputs.directory` names the private output directory for the command. With
fixes selected, `inputs.fix` is null during fixes and names the completed fix
command's output directory in subsequent commands. When preparation was selected and succeeded,
`inputs.preparation` contains its absolute output directory; otherwise it is
null. Telete
defines no package or installation format.

With analysis selected, `source.analyzed_commit` identifies the analyzed source
from the analysis stage onward. After accepted repairs, `source.repaired_commit`
identifies the accepted repair and `source.commit` selects that source for every
later command. `inputs.analysis` is null during analysis and names its output
directory in later stages. Analysis outputs remain available after agent repair.

Standard input is closed except during automatic repair. A known terminal exit
remains known if output collection fails; collection failure is a separate
observation.
A program that fails to start has a recorded start-error result. Neither failure
authorizes another launch under the same execution identity.

## Repair a failed analysis or check

An optional `analyze` command runs after automatic fixes and before `check`.
It must preserve the selected commit and tracked source. A successful analysis
continues to `check`. A known nonzero exit without a signal, timeout, start error,
or output collection error triggers repair. An ordinary `check` failure
also triggers repair if `analyze` or `repair` is configured. The command owns the
meaning of its findings. Process failures retain the usual failure and recovery
behavior.

### Automatic repair

Set the optional `repair` command in the workflow, for example
`"repair": {"program": "ci/repair.sh"}`. Telete runs it once per job when
analysis or check first triggers repair. The job shows `repairing` while
the command uses the execution slot. Standard input contains the failed
command's captured stdout followed immediately by its captured stderr,
with no added separator.

The script runs in the job's private checkout. It must exit successfully
with a clean tracked tree and a new descendant commit at detached HEAD.
Telete accepts that commit through the same handoff as `repair JOB REVISION`
and requeues the job at `check`. A known script failure or invalid repair
commit fails the job and retains diagnostics. An interruption without
terminal evidence blocks the job until the command can be reconciled.
If the next check triggers repair, the job waits for manual repair.

### Manual repair

An `awaiting_repair` job releases the execution slot so other queued jobs can run.
The waiting job retains its checkout, source ref, findings, and command outputs
until explicit repair or cancellation. Waiting alone does not keep a worker
running. `status` lists these jobs under `awaiting_repair`; `wait` returns a
terminal observation with exit 1 while the job remains pending.

Commit repairs in a development checkout, then accept that exact revision:

```sh
telete --config telete.json repair JOB REVISION
```

The revision defaults to `HEAD` in the configured repository. It must be a new
commit descended from the current job source. The command explicitly accepts those
repairs, retains them at the job's result ref, and queues the same job after
analysis. It preserves the submitted and analyzed identities. It does not run
analysis or automatic fixes again. An interrupted repair can be completed by
repeating `repair` with the same revision. Complete an interrupted repair before
cancelling its job. A different revision is rejected once
an interrupted repair identity has been recorded.

If analysis failed, its status becomes `accepted_after_repair` and the original
result remains recorded. Later commands receive the repaired source and any
original analysis outputs. Those outputs describe the originally analyzed
commit. Project commands own their final build and artifact identity rules.

Telete 0.5 reads existing 0.3 and 0.4 queue records. Let commands from an earlier
worker settle before submitting newly supported workflows. Keep 0.4 or later
while analysis or manual repair records remain, and 0.5 or later while batching
records remain. Jobs configured for automatic repair require a build that
supports it.

Use `cancel JOB` to discard a waiting job at its safe boundary. Use `recover JOB`
to reconcile an uncertain command using matching terminal evidence.

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
observing when the job succeeds, fails, is cancelled, becomes blocked, or
waits for manual repair.
Ctrl-C stops observation without cancelling the job. Add `--json` for compact
JSON output and structured operation errors. `wait` and `submit --follow` exit 0
only when the observed job succeeded. Failure, cancellation, blockage, waiting
for manual repair, and an observation timeout use exit 1. Ctrl-C uses exit 130.
Ordinary operation failures use exit 1; CLI syntax errors use exit 2.

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
For a failed shared release, these details include its leader and the actual
failed member's diagnostics.
Successful and cancelled jobs release their scratch files and full execution
records; superseded failures do likewise.

Telete retains at most 128 small settled-job markers containing job ID, phase,
queue sequence, and update time. Jobs with completed fixes or a successful
batched check also retain the
submitted commit, resulting commit, and result ref. Jobs with analysis also retain
the analyzed and accepted repair identities. Batched members also retain
`release_id` and `release_commit`. Markers contain no command,
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
