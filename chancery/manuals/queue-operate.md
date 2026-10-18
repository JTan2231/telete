# Operate Telete

Telete checks committed source and runs selected delivery stages. A project
supplies its workflow through `--workflow` in a separate project queue. Cell
queues use the native Cell workflow. Root and product `ci.sh` wrappers select
the installed `telete` command. This is the sole Cell product deployment route.
Product installers are underlying interfaces and own explicit recovery.
Telete retains private candidates under `refs/telete/jobs/`. Selected project
integration and acceptance use `refs/telete/project-accepted`. Cell uses
`refs/telete/accepted`. Telete does not advance development `main`.

## Set up the Cell host

Ordinary project queues run the built CLI directly and require no Cell host
setup or service installation. The commands in this section configure Cell.
Cell installation requires configured storage, an initialized queue, and a
usable signing identity:

```sh
telete storage configure --volume /Volumes/CellWork
telete storage status
telete signing create-local
telete signing status
```

The selected volume must be mounted, external, writable APFS with ownership
enabled. Storage publication is create-only. An unavailable volume never
selects another queue. Restore the selected volume and identity.

Signing uses the shared host policy unless the selected Telete state has an
explicit override. Keep the exact selected certificate and Keychain identity.
Before changing signing, pause and drain the queue, stop the worker, and settle
release publication, Telete recovery, and retained legacy manual deployment
effects. Configuration does not do those operations for the operator. Failed
preflight preserves the policy; interrupted
initial creation can leave Keychain effects that require explicit inspection
and selection of that exact identity.

Read `telete.signing.operate` for the complete shared storage and signing promise,
configuration format, initial creation, identity selection, and rotation limits.

## Initialize and control a queue

Select a normalized absolute local directory with `--state` and explicitly use
`init --project` to create a new local project queue. Initialization starts with
admission paused. Use the same state selection for every command:

```sh
telete --state /absolute/project/.telete init --project --repo /absolute/project
telete --state /absolute/project/.telete resume
telete --state /absolute/project/.telete submit COMMIT \
  --repo /absolute/project --workflow /absolute/workflow.json
telete --state /absolute/project/.telete worker
```

The selected directory holds the journal, private candidate worktrees, process
records, temporary files, Cargo downloads, compiler targets, and caches. Add it
to the repository's ignore rules when it is inside the checkout. Local paths
require private user-owned mode-0700 directories, normalized paths without
symbolic components, and a filesystem that supports the existing locking and
atomic-record operations. The capacity preflight requires 2 GiB free. Local
queue commands do not read the Cell workspace selector or probe an APFS volume.

The foreground worker continues until stopped. `worker --once` advances one
lifecycle tick; it does not complete an entire CI job. Use `status` and `wait`
from another terminal to observe retained work.

For project integration, select project mode and supply an exact acceptable
baseline:

```sh
telete --state /absolute/project/.telete init --project \
  --repo /absolute/project --accepted-baseline COMMIT
```

These are alternative initializations of a new project queue. Reinitialization
cannot replace a queue's repository, mode, baseline, or workspace binding. New
local project queues retain an optional `workspace` path equal to their selected
state root in the existing queue configuration. Existing queues without that
field retain their Cell-managed workspace and shared Cargo locations, including
older project queues. No implicit conversion or relocation is supplied. Missing,
foreign, incomplete, or mismatched journals do not select local mode. Only an
explicit new project initialization can create local state. A new state directory
must be absent or empty and private.

Omitting `--state` retains the Cell default. Cell state stays below its selected
external workspace in a `telete` or `telete-*` directory. Cell volume identity,
APFS, ownership, capacity, signing, and maintenance checks remain enforced.
Project initialization without explicit local selection retains that legacy
storage route. Project submissions
require `--workflow`; Cell submissions cannot use a project queue. Project
workflows cannot use a Cell queue or advance Cell's accepted ref.

Initialize Cell with an exact acceptable baseline and omit `--project`.
Initialization does not validate that baseline:

```sh
telete init --repo /absolute/cell --accepted-baseline COMMIT
telete status
telete resume
telete submit COMMIT --repo /absolute/cell --run-tests
telete worker --once
```

After Cell installation, run `telete service start` to start its installed worker.
Run `telete worker` for a directly operated worker. Use `telete pause` to prevent
new claims. Use `telete cancel JOB` for an intended stop at a safe boundary. Use
`telete recover JOB` to reconcile retained work. Use `telete wait JOB --timeout
60` to observe the same job without resubmission. Status is a CLI observation;
it is not an Iatreion probe or public CI client.

Use `telete cancel JOB` to abandon one blocked, unaccepted Cell checking job
without an aggregate report. Pause the queue first. The retained evidence must
show one of these conditions:

- The exact bounded legacy dispatcher timed out and has its terminal receipt.
- The exact candidate-validator bootstrap failed with a normal nonzero exit.
  It has no timeout, signal, start error, or output-capture error. No dispatcher
  was admitted under any supported dispatcher identity family.

Both paths require terminal settled child evidence, exclusive compiler and
deployment resources, an unchanged accepted base and exact private candidate,
a clean worktree, and no other queued work, preparation, source acceptance, or
deployment. Project workflow jobs cannot use this abandonment.

The command retains the original operation in `validation-N.abandoned.json`,
clears its unresolved intent, and sets the job phase to `cancelled`. The failed
outcome, process identities, receipts, and queue pause remain
retained. Cancellation starts no invocation, replays no validation, and claims
no pass. Submit the corrected source with a new request ID for another delivery
job. Use a matching executor for this recovery; building it does not install
Telete.

Reuse `--request-id KEY` only for the same frozen submission. A changed input,
workflow, command, parameter, test policy, deployment selection, or setting
needs a new key. Reuse returns the original job
with its original selections.

Cell jobs skip tests unless `--run-tests` is supplied. `--no-deploy` freezes
the choice to skip Cell deployment.

## Select a project workflow

Select the initialized project queue with `--state`. Supply `--workflow` with
an absolute regular JSON file. Telete freezes its contents at submission.
Later file changes do not change the job. Unknown
workflow and command fields are rejected. For example:

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

The check executable must be an absolute path or a normalized relative path
below the exact candidate checkout. An absolute path selects a host executable. An executable
path does not freeze the executable's bytes.

Each command has `program`, optional literal `args`, optional `env` overrides,
and optional `timeout_seconds`. Arguments default to an empty array. Environment
overrides default to an empty object. An omitted or null timeout is unbounded;
a supplied timeout must be positive. Every command runs in the candidate
checkout. Select a shell explicitly to interpret shell syntax.

The workflow has these additional fields:

| Field | Default | Meaning |
| --- | --- | --- |
| `integrate` | `false` | Merge the submitted commit with one captured project accepted base. |
| `prepare` | absent | Run an absolute trusted host command after checking. |
| `accept` | `false` | Conditionally advance project accepted source; requires `integrate`. |
| `deploy` | absent | Run an absolute trusted host command; requires `prepare`. |
| `parameters` | `{}` | Pass the frozen object unchanged in each command context. |

The fixed order is optional integration, check, optional preparation, optional
acceptance, and optional deployment. Telete refuses acceptance without
integration and deployment without preparation. A stage that was not selected
does not become a successful execution.

Preparation and deployment executables must be absolute host paths. Candidate
source cannot replace these selections. The complete controlled environment is
frozen with the declared overrides. Overrides cannot replace Telete's context
or controlled runtime directories. Keep credentials under their provider
authorities; use private file references where a command requires credentials.

Environment names must start with an ASCII letter or underscore and contain
only ASCII letters, digits, or underscores. Programs, arguments, and environment
values cannot contain NUL. A `PATH` override must contain only absolute nonempty
directories. Telete reserves all `TELETE_` names, `HOME`, `TMPDIR`, `TMP`, `TEMP`,
`CARGO_HOME`, `CARGO_TARGET_DIR`, `RUSTUP_HOME`, `XDG_CACHE_HOME`,
`CLANG_MODULE_CACHE_PATH`, `SWIFT_MODULECACHE_PATH`, and `CHANCERY_USAGE_INTERNAL`.

Do not combine `--workflow` with Cell's `--run-tests`, `--skip-tests`, `--deploy`, `--settings`,
or `--no-deploy` flags. Put project choices in `parameters` and the selected
commands.

A job without integration checks the submitted commit in a clean private
checkout. It performs the check even when that commit is already accepted.
It needs no accepted ref, Cell product inventory, Cell validator build,
aggregate Cell report, preparation record, or signing selection. Check success
requires the retained successful command completion for that exact candidate.
It does not establish source acceptance or deployment.

Integration captures `refs/telete/project-accepted` when the worker claims the
job. Candidate merges retain that base. The journal distinguishes the
submitted source, integration base, and tested candidate. A merged
candidate's pass does not report that the submitted tree passed unchanged.
Acceptance uses an expected-old update of the project ref against the captured
base. A concurrent ref change fails that update; Telete does not select a new
base inside the job.

An absent preparation command performs no signing lookup. The declared provider
dependencies remain required documentation for the corresponding Cell stages.

A failed project check or preparation remains failed. Correct the source and
submit a new job; Telete does not request model repair or send outcome messages.

Generic preparation passes its retained successful command record and output
destinations to the next stage through `TELETE_CONTEXT`. Commands own the
meaning and layout of their outputs. Telete supplies no generic package or
installation format. Generic deployment success requires its retained
successful command completion; the command owns any stronger domain claim.
An interrupted command without terminal evidence remains unresolved. Telete
does not automatically repeat or roll back its effects. There is no generic
abandonment or acknowledgement command for missing terminal evidence.

Project check and preparation commands use the macOS filesystem sandbox with
the selected working area as the permitted write scope. This is the existing
filesystem confinement; it does not isolate queue records from the same user's
candidate commands.
Deployment runs as a trusted host command without that confinement. All project
commands use the heavy gate; deployment also holds Telete's deployment lock.

## Select Cell deployment

Use an explicit deployment set for an installation or redeployment:

```sh
telete submit COMMIT --repo /absolute/cell --deploy PRODUCT \
  --settings /absolute/private/settings.json
```

The optional settings file must be an absolute regular JSON file that maps
canonical product IDs to objects. Every key requires an explicit `--deploy`
selection. Use `krisis` for its settings, including with `--deploy decisions`.
Telete freezes the values at submission and passes each product's object and the
selected dependency settings to its installer. Later file changes do not change
the job. Settings should reference private credential files instead of containing
credential bytes. Omission uses each product's existing configuration or defaults.
Product code validates the settings' meaning.

An explicit deployment continues through CI validation, production preparation,
and deployment when the submitted source is already accepted. Without that
selection, an accepted submission retains the `already_included` outcome. Telete
integrates the submission with accepted source; this is not a historical-source
restore operation. Explicit deployment does not expand product test selection.
Use a new request ID for each intended new delivery.

Retained jobs and deployment operations without settings remain readable.
Install the matching Telete before admitting jobs with nonempty settings; older
Telete cannot read that job option. Telete's own bootstrap and program maintenance
continue to use its guarded `telete install` procedure below.

## Coordinate admission maintenance

Use `telete maintenance hold --owner OWNER`, `maintenance status`, and
`maintenance release --owner OWNER` to coordinate queue admission during maintenance. Release
only the owner acquired by that operation. A release does not clear operator
pause or an unresolved job.

## Retain command execution

Project jobs freeze their workflow commands and parameters. Cell jobs freeze
the repository-relative check script and host Telete executable path. All new
jobs freeze the permitted host environment at submission. The environment selects
ordinary user, locale, toolchain, and SDK values, controlled tool and scratch
paths, and two Cargo build jobs. This policy freezes executable paths, not host
executable bytes. Telete resolves separate bootstrap, check, preparation, and
deployment commands. Each resolved command contains an absolute executable,
literal ordered arguments, an absolute working directory, the complete selected
environment, input and timeout policy, and the exact context value and its
absolute file path. Cell bootstrap selects its candidate-specific target directory.

Telete retains that command before execution. It writes the retained context to
the command's owned JSON file and supplies its path as `TELETE_CONTEXT`. A command
with context clears the inherited process environment and uses the retained
environment. Context and environment are private records. Credentials remain
under their existing provider authorities; settings should identify private
credential files instead of containing credential bytes.

The context contains `id`, `job_id`, `stage`, `checkout`, `source`, `parameters`,
`inputs`, and `outputs`. `source.input`, `source.base`, and `source.candidate`
identify the submitted commit, fixed integration base, and exact candidate.
`source.base` is null without integration. Project parameters contain the frozen
supplied object. Cell parameters contain the frozen options and signing policy. Inputs and outputs
identify stage-specific records, including the applicable preparation handoff.
The context file is a private user-owned regular file, created once and checked
against the retained value before launch. A mismatch stops execution.

For project commands, `outputs.directory` is the absolute private
`outputs/OPERATION` directory below the job. `inputs.preparation` is null when
there is no preparation handoff. Otherwise it names the absolute retained
successful preparation receipt for the same candidate. That receipt contains
`stage`, `candidate`, `operation`, `command`, `result`, and `stamp`. Its
`command.context.value.outputs.directory` identifies the preparation outputs.
There is no additional generic output format.

Command arguments are passed directly; a shell runs only when the selected
executable is explicitly a shell. A missing input closes standard input. An
omitted timeout is unbounded, and a supplied timeout must be positive.

Recovery uses the saved command and context. An existing execution identity can
join only an equal command, including its context contents, environment, and
timeout. A retained admission without terminal evidence remains unresolved and
does not authorize another launch. A known terminal exit remains retained if
output collection fails; the collection failure is a separate observation.
An executable that fails to start has a retained start-error result. That result
does not authorize another launch under the same identity.
Process disappearance does not establish completion. Cell stages also require
their correlated domain records.

Retained commands without context keep their original environment behavior,
arguments, time limits, and correlation. Telete does not rebuild admitted
commands from current settings or change their meaning during recovery.
Install matching Telete before admitting jobs with a frozen command policy;
older programs cannot read that policy. Installation remains a separate operation.

## Validate Cell source

Each claimed job captures Telete's accepted source as its fixed base and merges
the submitted commit in a private worktree. Conflicts stop the job. Deterministic
formatting and Clippy autofix can produce a new private candidate. Validation compares each candidate to the same
base, including deletions and both rename paths.

The validator loads literal product descriptors and Cargo metadata. Telete's
own descriptor is `infrastructure/telete/product.sh`. Other product descriptors
remain in `pipeline/products/`. Product, provider, package, and executable
identities remain separate.

Product changes select their owner. Explicit prompt and shared platform inputs
also select their consumers. Changes to `cell-install` or `cell-maintenance`
select their direct local Cargo consumers. Common Telete validation inputs
select the full product and platform inventory. Root `Cargo.toml` and
`Cargo.lock` changes do not select all products. Other local Cargo dependencies
do not expand product selection.

With tests enabled, each selected platform product adds the shared
`cell-install` tests. A platform product with a direct local `cell-maintenance`
dependency also adds that suite. Adding these tests does not select more
products. New product introductions follow the same rules.

Default deployment uses the selected products, except Telete. An explicit
`--deploy PRODUCT` list selects deployment products, and `--no-deploy` prevents
deployment. Test selection does not add products to an explicit deployment list.

Contract 2 narrows product selection and adds the shared platform tests above.
Validation receipts remain schema 1. Retained receipts keep their recorded
scope; this change does not migrate or reinterpret them.

The host compiles the exact candidate's Telete validator in an isolated target
directory. Candidate code owns validation. Host Telete code owns promotion,
production preparation, and signing. Missing candidate support fails explicitly.
New dispatches run the candidate's regular `pipeline/cell/check.sh` file through
`/bin/sh`, with the exact compiled validator and validation arguments. A candidate
without this entry point fails; there is no direct-validator fallback. Telete
requires a clean checkout of the exact candidate before dispatch. Each deterministic autofix
selects the script and validator from its new candidate under the frozen policy.
The script path does not freeze the first candidate's script contents. Telete
requires both successful command completion and the correlated aggregate report.
Recovery checks the same completion before consuming a report. Retained dispatches
keep their original command, gate identity, and completion checks.
The validator runs native structure, provider, shell syntax, formatting, Clippy,
selected nextest, and release checks. Deferred release checks run in trusted
production preparation. Receipts state when tests were skipped.

The candidate validator dispatcher has no whole-validation deadline. Telete also
sets no aggregate deadline for Cargo metadata, Clippy, selected tests, formatting,
tool builds, release builds, or production builds. Git, provider, signing, and
individual probes retain their time limits.
Unbounded dispatchers use distinct gate identities. Retained command requests
keep their recorded time limits and exact correlation; a different command cannot
reuse their gate identity. Process completion and the aggregate validation report
remain separate requirements for validation success.

Install the pinned nextest runner explicitly with `telete prepare-tools` before
jobs that run tests. Validation does not download a missing runner.

Machine-applicable Clippy suggestions and formatting can produce one raw Git
patch from an isolated snapshot. The manager records a new private candidate
and validates it again. The original candidate remains unchanged.

The broker admits one heavy gate and two light gates. Equal gate identities
join the retained result only for the same command. Different commands cannot
reuse an identity. Missing terminal evidence blocks the resource slot; process
disappearance is not success. `telete gates` exposes retained diagnostic records.
No automatic pruning or uncertain-gate replay is supplied.

Remaining check failures stay failed. Correct the source and submit a new job.
The plain files `prompts/repair-instructions.txt` and
`prompts/repair-prompt.txt` retain authored repair text as inactive source assets.
No workflow reads or executes them.

## Prepare and execute Cell deployment

For new jobs, the manager invokes preparation and deployment through private
Cell work commands in the host Telete executable selected at submission. Each
command checks the retained job, candidate, and admitted operation or deployment
request. These commands do not submit work or advance accepted source. They are
internal worker entry points, not separate delivery interfaces.

The commands have no outer execution deadline. Preparation retains its existing
compiler gates and staging receipts. Deployment
retains its existing instruction receipts and uncertainty rules. The manager
reads those records to decide the next phase; command exit alone cannot establish
deployment success. No new outer broker gate changes settlement or recovery.

Preparation supplies its existing retained preparation record as the deployment
handoff. Telete checks source, product scope, signing policy, and staged-output
correlation before source acceptance and deployment. A mutable latest-output path
does not replace that handoff. Compilation, signing, and infrastructure failures
remain failures and do not request model repair.

The deployment receipt remains authoritative for completed instruction effects
if the outer command subsequently fails. Recovery accounts for those effects
without repeating completed instructions. Missing terminal or instruction
evidence does not authorize automatic replay.

Telete builds selected production packages together, stages declared binaries,
and signs native commands with the frozen certificate and
stable product identifiers. Telete reads the shared host signing selection by
default. `telete signing configure --certificate-sha1 FINGERPRINT --keychain
/absolute/keychain --identifier-namespace local.cell` selects an existing
certificate in Telete's own configuration. Add `--host` to change the shared
Cell policy. Initial `signing create-local` also selects the shared policy;
an existing Telete override retains precedence. Configuration and installation require
paused, drained Telete work and settled Telete deployment.

Use `telete signing status` to inspect the selected identity. Keychain access or
native authorization can require the user. Signing and preparation failures
stop before source acceptance. There is no alternate certificate or unsigned fallback.

Inspect retained gate receipts and staging effects when preparation stops without
a final receipt. Stop the worker and pause the queue. Use `telete
acknowledge-preparation JOB` only to abandon that blocked preparation. The command
requires exclusive worker and resource ownership, exact terminal child receipts,
unchanged accepted source, and no source acceptance or deployment.
It retains the original operation in an acknowledgement record,
preserves the failed outcome, and cleans the owned
worktree. It does not replay preparation, publish staged candidates, or report
preparation success. A repeated acknowledgement joins the same record. Submit a
new commit with a new request ID after correcting the failure.

Promotion checks runtime source, scope, and policy correlation and uses an
expected-old reference update. Telete retains the facts in its journal.

Deployment executes exactly the selected product manifests' `run`, `copy`, and
`link` instructions in order. Product installers read package assets from the
exact source worktree. Product installation commands own program
selection, state, configuration, schedules, and service lifecycle. A completed
instruction does not establish product health or domain success. A failed
instruction retains earlier effects. An interrupted instruction is uncertain
and is not automatically repeated or rolled back.

New deployment `run` instructions have no execution deadline when the manifest
omits `timeout_seconds`. An explicit positive limit remains enforced.

Use the matching executor to reconcile or acknowledge previously interrupted
runs that used the implicit deadline before replacing that executor. Retained
process requests keep their recorded limits. Replacement does not migrate those
requests or resume remaining instructions.

Inspect the retained deployment before using `telete acknowledge-deployment
REQUEST`. Acknowledgement requires exclusive deployment ownership, records the
uncertainty, and releases the interrupted operation. It does not undo effects,
run remaining instructions, or establish product health. Then use `telete
recover JOB` to reconcile the job with that retained outcome.

Accepted source remains accepted after deployment failure.
Private requests, source, diagnostics, and receipts remain
retained; settled worktree cleanup does not remove their records.

## Install only on explicit request

Build Telete and invoke `telete install` only when installation is intended. The
queue must be initialized, paused, and settled first. The
installer publishes the executing binary and matching embedded provider in its
own retained release archive. It publishes regular executable files at the fixed
`~/Library/Application Support/Telete/install/runtime/bin/telete` path. Both the
public `~/.local/bin/telete` command and the `org.cell.telete` user service use
that runtime file. Updates preserve its actual executable path. Each runtime
file replacement is atomic; the complete tree is not one atomic update.
The matching provider directory selector follows the selected retained archive.
Operational state stays external.

Installation leaves the Telete service stopped. Start it explicitly, then
resume queue admission.

Use `telete service status`, `service start`, and `service stop` for this service.
Service stop requires paused, drained work. It also permits one blocked deployment
whose exact successful receipt matches its unchanged accepted source and product
scope, with no queued work, active
operation, or live compiler or deployment child. The stop holds exclusive settled
resource ownership and preserves the job and receipts for `telete recover JOB`;
it does not repeat deployment.

Service stop also permits one paused Cell queue with a blocked, unaccepted
failed preparation. Its unresolved operation must name the exact current
production generation, candidate, and owned result path. Both final preparation
receipts must be absent. No other queued or unresolved work, preparation result,
source acceptance, or deployment may be present. The accepted base and private
candidate must remain unchanged, and the worktree must be clean and belong to
the configured repository.

For jobs with a frozen command policy, the outer preparation invocation under
`commands/production-N` must match the exact job command. Its retained process
request and terminal result must correlate and show a normal nonzero exit,
without a timeout, signal, start error, or output-capture error. Missing or
abnormal outer completion blocks stop. Legacy jobs without that outer command
retain their existing evidence requirements.

A present, dangling, or inaccessible final preparation receipt or active
deployment record blocks this stop exception.

The stop holds exclusive settled compiler and deployment resources. It changes
no job, operation, outcome, or receipt and does not replay
preparation. After the worker stops, use the existing guarded
`telete acknowledge-preparation JOB` to abandon that preparation. Correct the
failure and submit a new request. Project workflow jobs cannot use this stop
exception. Installation and signing maintenance still require a drained queue.

Service stop also permits one paused blocked validation with matching job and
canonical `autofix` reports. The command requires the exact successful unbounded
dispatcher, the exact retained `autofix.patch`, unchanged accepted base and
private candidate, and a clean worktree.
It requires no queued work, preparation, acceptance, deployment,
or live compiler or deployment child. The stop holds exclusive settled
resources and preserves the unresolved job, operation, failure, and receipts.
Use `telete recover JOB` after the service stops to reconcile the retained job.
Stopping does not settle the job or replay validation.

Program replacement and signing maintenance still require paused, drained work. Installation,
Semantics project registration, queue resume, and live delivery
are separate operations. A source participation marker does not prove project
registration. No cross-release journal migration window is supplied.
