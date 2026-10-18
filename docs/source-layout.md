# Source layout

The root package is copied from Cell's `infrastructure/telete` at source commit
`79b406279df18e56210026c2e1f3fa1121c916d9`. It retains the native Cell workflow
and adds local project queue storage.

`support/cell` contains the five local Rust dependencies needed to build the
copied package: Cell Install, Chancery, Chancery Usage, Iatreion API, and Usher.
Their existing package versions and provider interfaces are retained. Build
paths do not refer to a sibling Cell checkout. Embedded scripts and fixtures
retain the relative layout required by those packages.

`prompts/repair-instructions.txt` and `prompts/repair-prompt.txt` retain Telete's
authored repair text as ordinary inactive files. No runtime reader, prompt store,
selection manifest, import procedure, or model execution uses these files.

`product.sh` describes Telete's native Cell inventory entry. It does not register
this standalone repository with Cell. The repository's own check is
`ci/workflow.json`, selected by `ci.sh`.

The Cell program installer and its service remain explicit native Cell
operations. Build and invoke the CLI directly for an ordinary project queue.
