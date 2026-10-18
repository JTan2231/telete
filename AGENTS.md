# Telete instructions

Do not overcomplicate or overarchitect things.

Telete runs a serial queue for committed repository workflows on macOS. Every
command explicitly selects its private state directory. Keep worktrees, temporary
files, compiler targets, downloads, and caches below that directory. Keep the CLI
and repository configuration sufficient for foreground operation.

Submission freezes the selected workflow, parameters, and controlled environment.
Preserve exact source identities, stage order, conditional source acceptance, and
retained command correlation. An interrupted command without terminal evidence
remains unresolved; recovery must not replay uncertain effects.

Repository workflows own their checks. Trusted configured commands own preparation,
deployment, output formats, credentials, and any stronger domain success claims.
Telete owns queue state and execution records. Keep source and operational records
private. Use ordinary Rust data types and runtime correlation checks.

Keep dependencies standalone and the implementation focused on the configured
workflow lifecycle. Test changes with focused local Rust checks.
