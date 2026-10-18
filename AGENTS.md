# Telete instructions

Semantics-Project: telete

Telete runs configured repository workflows. New project queues use the selected
private state directory for their workspaces, temporary files, and caches. Keep
the repository CLI and configuration sufficient for foreground operation on macOS.

Telete also remains Cell's CI authority. Use Cell's state, refs, worktrees, service
identity, and executable for Cell delivery. Host setup owns Cell's shared workspace and signing
selections. Preserve those formats, the selected volume and certificate identity,
and Telete's admission, worker, deployment, and release-publication maintenance
boundaries. Product installation instructions retain their own authority.

Use `telete submit` or its `ci.sh` wrappers for every Cell product deployment.
Explicit `--deploy` requests recheck and prepare accepted source. Product
settings are frozen at submission and passed unchanged to selected installers.
Keep Telete's explicit bootstrap and self-maintenance installation procedure.

Use ordinary Rust data types and runtime correlation checks. Do not add checked
evidence types, a public CI client, or an Iatreion status integration.

The standalone Cargo workspace includes the existing Cell Rust dependency closure
under `support/cell`. Preserve the copied package versions and provider interfaces.
Do not introduce a dependency on a sibling Cell checkout.

Keep provider authority with each provider. Use the supported Rust interfaces
for Chancery and Usher. Keep source and operational records private. The two
plain prompt files under `prompts/` are retained inactive text; Telete has no
model execution or outcome notification integration. Test changes with focused
local Rust checks.
