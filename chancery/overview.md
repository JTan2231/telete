# Telete

Telete orchestrates checks and delivery of committed source in Rust. A project
workflow supplies a check command and optional trusted preparation and deployment
commands. Integration and acceptance are explicit selections.
Telete owns the executable, journal, private worktrees, Git references,
command records, and Cell's explicitly installed user service.

A check-only job starts from the submitted commit and needs no accepted-source
ref, Cell inventory, validation report, signing policy, or provider preflight.
Its result describes the exact candidate checked. Integration captures one
accepted base when the worker claims the job. Optional acceptance conditionally
advances that ref after the required selected stages succeed.

Project workflows require a separate project queue. Baseline-free initialization
selects project mode; `init --project --accepted-baseline COMMIT` creates a
project queue with an integration baseline. Project acceptance uses
`refs/telete/project-accepted`.
Cell queues retain `refs/telete/accepted` and reject project workflows. Project
queues reject Cell submissions. Initialization cannot replace a queue's mode
or baseline.

Root and product `ci.sh` wrappers select installed Telete. CI and Telete are the
sole Cell product deployment route. Submission freezes
product settings and explicit deployment selections. Selected deployments of
accepted source still pass through validation and signed preparation.

Explicit host setup writes Cell's shared external-volume and signing selections.
It preserves the selected volume and signing identity. Telete maintenance guards
coordinate signing changes with queue work, deployment, and release publication.
Queue operation remains separate from host setup.

The manager freezes the selected workflow and owns source acceptance. Each
candidate supplies its relative check executable. Trusted caller configuration
selects absolute preparation and deployment executables. Commands receive exact
source identities, frozen parameters, and retained stage handoffs through
`TELETE_CONTEXT`. Recovery consumes their retained invocations and preserves
uncertain effects.

For Cell, the candidate validator owns gate selection. The broker owns gate
admission and process results. Production preparation owns compilation, staging,
and configured native signing. The deployment executor performs product-owned
instructions in order. Cell validation requires process completion and its
correlated aggregate report.

Chancery owns provider document formats. Usher owns Cell
membership recognition. Telete uses their supported Rust interfaces and retains
its own domain decisions.

Cell retains deterministic formatting and Clippy autofix. Remaining check and
compilation failures stay failed; correct the source and submit a new job.
`prompts/repair-instructions.txt` and `prompts/repair-prompt.txt` retain authored
text as inactive assets. No workflow reads or executes them.

New project queues initialized with explicit `--state` and `init --project` use
that private local directory as their working area. Worktrees, temporary files,
Cargo downloads, targets, and caches stay below it. Existing queues retain their
previous external workspace selection and shared Cargo locations. Cell's native
route keeps its exact external APFS volume and signing requirements.

Project operation needs the built CLI and a foreground worker, not a service
installation. Check execution uses the existing macOS filesystem sandbox. This
change adds no Linux execution, generic service setup, or recovery workflow.

Read `telete.queue.operate` for delivery commands, retained state, and recovery.
Read `telete.signing.operate` for shared host storage, signing identity, and
maintenance. Telete has no public Rust client, Iatreion probe, or checked evidence
type protocol. Its command interface uses ordinary records and runtime checks.
