# Import prompt components and selections

Use this feature to import reviewed Cell prompt components into one private
Bazaar database. Bazaar owns initialization, component appends, and complete
owner-selection publication. Read `bazaar.prompts.prepare` for selected reads,
rendering, and requester compatibility. Import starts no agent, deploys no
caller, sends no message, and changes no schedule.

## Review and import

1. Review the complete import document and select the intended private database.
   The repository seed is `infrastructure/bazaar/seed.json`. It contains active
   Cell prompt components, separately named project-writing prompts, library
   documents, and compatibility text. It is an explicit import artifact and
   never a runtime fallback. Keep private edits outside source and releases.
2. Run the explicit importer from the Cell checkout:

   ```sh
   bazaar --database /absolute/private/bazaar.sqlite3 import-prompts infrastructure/bazaar/seed.json
   ```

   Omit `--database` only when the default
   `~/.local/share/bazaar/bazaar.sqlite3` is intended. The CLI database selection
   is independent of the prompt reader's `CELL_BAZAAR_DATABASE` override.
3. Read each resulting `cell.prompts.OWNER` and verify its exact referenced
   component versions through `bazaar get`. Use the same database for all
   callers of one installed product. An interactive environment override does
   not configure Clockwork or another scheduled process.
4. Deploy the callers through their own procedure only after required selections
   are available. Import the original seed before publishing later edits.
   Preserve selection version 1 and all its referenced versions.

The local CLI returns a readable summary by default. Global `--json` returns
schema-one `{"schema_version":1,"ok":true,"data":...}` on stdout. Data has
`imported_ids`, `appended_text_versions`, and `owner_selections` counts.
`imported_ids` is the number of distinct component IDs in the import document.
`appended_text_versions` counts new component appends, excluding selection
appends. `owner_selections` counts the complete owner sets processed, including
selections already identical. These counts describe this completed import;
they do not prove caller deployment or remain a latest-state observation.
No timestamp is stored or returned.

Operational errors exit 1. Text mode prints a diagnostic on stderr; JSON mode
prints `ok:false` with `error.detail` on stdout. Invalid CLI syntax uses Clap's
stderr diagnostic and exit 2 in both modes.

Rust programs use `bazaar::prompts::import(database: &Path, input: &Path)`.
It returns `bazaar::prompts::Result<ImportReport>` with the same report fields
or a typed `Error`. It runs in process and records no CLI usage.

## Input and publication

The import file must contain UTF-8 JSON with schema version 1, a nonempty
`entries` array, and no unknown fields:

```json
{"schema_version":1,"entries":[{"id":"weaver.narrative.instructions","content":"Write the requested narrative."}]}
```

Each entry contains one unique exact `id` and complete string `content`.
The ID's first dot-separated segment must be `annals`, `conatus`, `krisis`,
`semantics`, `platter`, `weaver`, `emt`, or `telete`. Duplicate IDs and
unknown owners fail before state initialization. The example shows the input
shape; import a complete set for every supplied owner. A partial set removes
omitted component keys from that owner's next selection.

The importer initializes only the selected absolute private path through
`Writer::initialize`. It accepts absent, empty, or compatible schema-one state
and preserves unrelated IDs. Foreign or unsupported state remains an error.
The same private regular path and permission rules as `bazaar.installation`
apply.

For each component, the importer reuses identical latest content or appends
one complete new version. Unlike a raw `bazaar update`, a repeated identical
component does not append. After every component exists, it publishes each
owner's complete schema-one selection with the exact component versions.
An identical latest selection is reused. Separate component and selection
appends have separate transactions. Publication is per owner and provides no
atomic cutover of all requesters.

## Recover and edit

An interrupted import can leave unused component versions or some published
owner selections. Earlier committed versions remain intact. Repeat the same
reviewed import to finish; identical latest content is reused. After an
uncertain write, inspect history and relevant content before deciding whether
another append is intended. Concurrent unrelated appends can change which
record is latest; inspect selected versions rather than inferring state from
an old receipt.

For a later prompt edit, append complete text with `bazaar update`. Then append
the complete `cell.prompts.OWNER` selection with the returned component versions.
Publish components first. To roll back a selection, append its previous complete
content. Do not overwrite or remove history. Adding a new seed ID does not
replace retained versions of an existing component.

This feature authorizes only the reviewed import's database initialization and
content publication. It does not authorize caller migration, library revision
assignment, model or permission changes, history deletion, state restoration,
agent execution, or external disclosure.

## Privacy, compatibility, and limits

Import documents and stored content can contain private text. Protect the input
file, database, and diagnostics within the selected owner's boundary. The
success report contains counts and no component text. CLI dispatch separately
attempts metadata-only Chancery usage recording; errors preserve the import
result. Content and IDs are not recorded in that journal.

The importer reads the complete JSON document and content into memory.
SQLite waits up to five seconds for contention. No import-size, throughput,
completion-time, or cross-owner atomicity guarantee is supplied. The importer
uses local files and Bazaar only; it invokes no external service or catalog.

Database schema one, string versions, import format one, selection format one,
product release, and this contract version are separate identities. Import
preserves the current formats and existing IDs and versions. Program recovery
preserves stored content and cannot restore an earlier selection by itself.
