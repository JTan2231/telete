# Operate Telete

Telete runs a serial queue for committed repository source on macOS. Each job
freezes a workflow that selects a check command and optional integration,
preparation, source acceptance, and deployment. Telete owns the queue journal,
private worktrees, command records, and Git reference updates. Configured commands
own the meaning of their checks, outputs, and deployment effects.

## Initialize a queue

Every command requires `--state` with a normalized absolute private directory.
Initialize a new queue, resume admission, submit committed source, and start a
foreground worker:

```sh
telete --state /absolute/project/.telete init --repo /absolute/project
telete --state /absolute/project/.telete resume
telete --state /absolute/project/.telete submit COMMIT \
  --repo /absolute/project --workflow /absolute/workflow.json
telete --state /absolute/project/.telete worker
```

Initialization starts with admission paused. The state directory must be absent
or empty when creating the queue. Reinitialization cannot replace its repository
or baseline. Use the same state selection for every command.

The selected state directory holds the journal, private candidate worktrees,
process records, temporary files, Cargo downloads, compiler targets, and caches.
Add it to the repository's ignore rules when it is inside the checkout. Private
directories must be user-owned with mode 0700, use normalized paths without
symbolic components, and reside on a filesystem supporting file locks and atomic
record publication. Capacity preflight requires at least 2 GiB free.

To permit integration, initialize with an exact acceptable baseline:

```sh
telete --state /absolute/project/.telete init \
  --repo /absolute/project --accepted-baseline COMMIT
```

Initialization selects that baseline without running the workflow against it.
An integration job requires an accepted baseline. A check without integration
does not require one.

## Select a workflow

`submit` requires `--workflow` naming an absolute regular JSON file. Telete
freezes its contents and the selected controlled environment at submission.
Later file changes do not change the job. Unknown workflow and command fields
are rejected.

```json
{
  "check": {
    "program": "ci/check.sh",
    "args": [],
    "env": {},
    "timeout_seconds": null
  },
  "parameters": {"suite": "all"}
}
```

The required check program must be an absolute path or a normalized relative
path below the exact candidate checkout. An absolute path selects a host
executable. Executable paths do not freeze host executable bytes.

Each command accepts `program`, optional literal `args`, optional `env` overrides,
and optional `timeout_seconds`. Arguments default to an empty array and
environment overrides to an empty object. An omitted or null timeout is
unbounded; a supplied timeout must be positive. Every command runs in the
candidate checkout. Select a shell explicitly to interpret shell syntax.

| Workflow field | Default | Meaning |
| --- | --- | --- |
| `integrate` | `false` | Merge submitted source with one captured accepted base. |
| `prepare` | absent | Run a trusted absolute host command after checking. |
| `accept` | `false` | Conditionally advance accepted source; requires integration. |
| `deploy` | absent | Run a trusted absolute host command; requires preparation. |
| `parameters` | `{}` | Pass the frozen object unchanged in each command context. |

The fixed order is optional integration, check, optional preparation, optional
acceptance, and optional deployment. A stage that was not selected does not
become a successful execution. Put repository choices such as test selection in
the selected command and its parameters.

A workflow may deploy without accepting source. Deployment requires preparation;
acceptance is an independent selection.

Preparation and deployment programs must be absolute trusted host paths.
Candidate source cannot replace those selections. Environment overrides cannot
replace Telete's context or controlled runtime directories. Keep credentials
under their owning systems; use private file references where a command requires
credentials.

Environment names must start with an ASCII letter or underscore and contain only
ASCII letters, digits, or underscores. Programs, arguments, and environment values
cannot contain NUL. A `PATH` override must contain only absolute nonempty
directories. Telete reserves all `TELETE_` names, `HOME`, `TMPDIR`, `TMP`, `TEMP`,
`CARGO_HOME`, `CARGO_TARGET_DIR`, `RUSTUP_HOME`, `XDG_CACHE_HOME`,
`CLANG_MODULE_CACHE_PATH`, and `SWIFT_MODULECACHE_PATH`.

Reuse `--request-id KEY` only for the same frozen submission. A changed input,
workflow, command, parameter, or controlled environment requires a new key.
Reuse returns the original job with its original selections.

## Source and stage results

A job without integration checks the submitted commit in a clean private
checkout, even if that commit is already accepted. Check success requires the
retained successful command completion for that exact candidate. It establishes
neither source acceptance nor deployment.

Telete retains private candidates under `refs/telete/jobs/`. Integration captures
`refs/telete/accepted` when the worker claims the job, then merges against that
fixed base. The journal distinguishes submitted source, integration base, and
tested candidate. A merged candidate's pass does not claim that the submitted
tree passed unchanged. Conflicts stop the job.

Acceptance uses an expected-old update against the captured base. A concurrent
accepted-ref change fails that update; Telete does not select a new base inside
the job. Telete does not advance the development branch. A failed check or
preparation remains failed: correct the source and submit a new job.

Preparation passes its retained successful command record and output directory
to the next stage through `TELETE_CONTEXT`. Commands own the meaning and layout
of their outputs. Telete defines no package or installation format. Deployment
success requires its retained successful command completion; the command owns
any stronger claim about the system it changes. Accepted source remains accepted
after deployment failure.

Check and preparation commands use the macOS filesystem sandbox with the
selected state directory as the permitted write scope. This filesystem
confinement does not isolate queue records from candidate commands running as
the same user. Deployment runs as a trusted host command without that
confinement. Workflow commands use the execution gate; deployment also holds the
deployment lock.

## Retained command context

Telete freezes the permitted user, locale, toolchain, and SDK environment,
controlled runtime paths, and two Cargo build jobs. Each resolved command records
an absolute executable, literal ordered arguments, an absolute working directory,
the complete selected environment, input and timeout policy, and its context.

Before execution, Telete retains the command and writes its owned private JSON
context file. `TELETE_CONTEXT` supplies that path. Commands run with the retained
environment after the inherited process environment is cleared. Context and
environment records are private. A context file must match the retained value
before launch; a mismatch stops execution.

The context contains `id`, `job_id`, `stage`, `checkout`, `source`, `parameters`,
`inputs`, and `outputs`. The source fields `input`, `base`, and `candidate`
identify the submitted commit, captured integration base, and exact candidate.
`source.base` is null without integration. Parameters contain the frozen supplied
object.

`outputs.directory` names the absolute private `outputs/OPERATION` directory
below the job. `inputs.preparation` is null without a preparation handoff;
otherwise it names the retained successful preparation receipt for the same
candidate. That receipt contains `stage`, `candidate`, `operation`, `command`,
`result`, and `stamp`. Its `command.context.value.outputs.directory` identifies
the preparation outputs.

Command arguments are passed directly. A missing input closes standard input.
A known terminal exit remains retained if output collection fails; the
collection failure is a separate observation. An executable that fails to start
has a retained start-error result. Neither failure authorizes another launch
under the same execution identity.

## Operate and recover work

The foreground worker continues until stopped. `worker --once` advances one
lifecycle tick; it does not complete a whole job. Use `status` or `status JOB`
to inspect retained work, and `wait JOB --timeout 60` to observe that job without
resubmitting it. Add the global `--json` option for compact JSON output and
structured operation errors. Ordinary operation failures use exit 1; CLI syntax
errors use exit 2.

`pause` prevents new claims. `resume` permits claims when no maintenance hold
or unresolved work blocks admission. `cancel JOB` requests an intended stop at
a safe boundary. It cannot establish completion or discard uncertain effects
of an admitted command.

Use `recover JOB` to reconcile retained work. Recovery consumes the saved
command, context, and terminal evidence. An existing execution identity can join
only an equal command, including context, environment, and timeout. Telete does
not rebuild admitted commands from current settings. Process disappearance does
not establish completion.

An interrupted command without terminal evidence remains unresolved. Telete does
not automatically repeat or roll back its effects. There is no command to
acknowledge or abandon missing terminal evidence. Retained requests, source,
diagnostics, and receipts remain private; settled worktree cleanup does not
remove their records.

The broker serializes command execution. Equal gate identities join the retained
result only for the same command. Missing terminal evidence blocks the resource
slot. `gates` exposes retained diagnostic records. Records are not automatically
pruned.

## Coordinate maintenance

Use `maintenance hold --owner OWNER`, `maintenance status`, and
`maintenance release --owner OWNER` to coordinate queue admission during
maintenance. Release only the owner acquired by that operation. A release does
not clear operator pause or resolve an interrupted job. Pausing admission does
not drain the queue or stop its worker.
