# Telete instructions

Do not overcomplicate or overarchitect things.

Telete runs a serial queue for committed repository workflows on macOS. Every
public command selects a project config. The config owns the source repository,
commands, parameters, and initial runtime path. The first queue mutation binds
the Git common directory to one runtime path; all linked worktrees and configs
then use that shared queue. Worktree creation only creates a Git branch and
worktree; it does not inspect or change queue state. Create private runtime state
on demand; no init command or pre-existing state is required.

Telete manages worker startup and shutdown. Submission runs in the background;
following only observes work, and interrupting observation does not cancel it.
Keep one worker while unfinished work exists, including paused or blocked work,
and stop it after settlement and cleanup. Users do not manage a worker lifecycle.
A private supervisor restarts its worker; losing that supervisor or restarting
the machine requires a later queue mutation to arrange execution again.

Run the selected committed revision directly, defaulting to HEAD resolved at
submission. Do not add integration, accepted baselines, source acceptance, or
project branch updates. Preserve stage order and exact command correlation.
An interrupted command without terminal evidence remains unresolved; recovery
must not replay uncertain effects.

Configured commands own their checks, preparation, deployment, output formats,
credentials, caches, environment choices, and stronger domain success claims.
Commands use ordinary inherited environment and permissions. Reserve TELETE_
variables for Telete's context. Do not add implicit runtime environment policy or
reproducibility guarantees.

Telete owns operational queue state, private checkouts, and current execution
diagnostics. Keep retention minimal and bounded; successful runs need only a
small marker. Do not turn runtime state into a historical job archive. Keep
source and operational records private. Use ordinary Rust data types and runtime
correlation checks.

Keep dependencies standalone and the implementation focused on the configured
workflow lifecycle. Test changes with focused local Rust checks.
