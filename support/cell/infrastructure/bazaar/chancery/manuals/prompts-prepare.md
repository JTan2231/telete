# Prepare selected prompt text

Bazaar owns shared prompt selection and text preparation over its versioned
string store. Use this feature to load exact component versions, render a
template, expand trusted references, or prepare descriptions. Read
`bazaar.prompts.import` for explicit content publication. Products own authored
meaning, runtime inputs, models, permissions, schemas, tool implementations,
request assembly, domain results, and recovery. No prompt grants execution
authority.

## Select exact component versions

Rust programs use `bazaar::prompts::{Prompts, Selection, Error, render}` and
`bazaar::prompts::Result<T>`. `Prompts::load(owner)` selects the latest
`cell.prompts.OWNER` string. `Prompts::at(owner, version)` selects an exact
selection version. `Prompts::open(database, owner, version)` selects an absolute
database explicitly; `version` is `None` for latest or `Some(version)` for an
exact version. These methods run in process and invoke no CLI or model.

The default database is `~/.local/share/bazaar/bazaar.sqlite3`. An absolute
`CELL_BAZAAR_DATABASE` overrides it for `load` and `at`.
`bazaar::prompts::database_path()` returns that path. The default requires an
absolute `HOME`. Reads require private initialized schema-one Bazaar state.
They never initialize, repair, migrate, or create missing text. An interactive
override does not configure a scheduled process environment.

Each product selection is an ordinary immutable Bazaar string with this format:

```json
{"schema_version":1,"entries":{"weaver.narrative.instructions":1,"weaver.tools.read_decisions.description":1,"weaver.tools.submit_document.description":1}}
```

`Selection` has `schema_version: u32` and `entries: BTreeMap<String, i64>`.
The format rejects unknown fields, unsupported schema versions, and empty
entry maps. Each map value is an exact positive component version.
`Prompts` reads the selection once, then reads every named component at that
exact version. Its public `selection` field is the selected Bazaar `Record`.
`text(id)` returns the complete selected component content. An ID outside the
selection is an error. No database handle survives preparation.

Cell's imported owners are `annals`, `conatus`, `krisis`, `semantics`, `platter`,
`weaver`, `emt`, and `telete`. Publish component versions before publishing
the complete selection that names them. This ordering gives a consistent
selected set without a transaction across separate reads. A new selection
affects new preparations. An already loaded `Prompts` retains its selected
content even if another writer appends later. Bazaar stores no timestamps.

## Render and expand trusted text

Use `Prompts::render(id, values)` to render a selected component. Use
`render(template, values)` for an explicit template. Both take named
`&[(&str, String)]` values that the caller has already formatted.
Templates accept `{name}` and positional `{}` placeholders. Positional values
use keys `0`, `1`, and so on. Doubled braces produce literal braces.
A `:?` suffix selects the same caller-formatted value; Bazaar does not apply
Rust formatting. Missing values, unterminated placeholders, and unescaped closing
braces are errors.

Rendering is one pass. Inserted values cannot introduce another placeholder
or prompt lookup. Extra supplied values are not rendered unless the template
references them. Bazaar does not escape or validate the meaning of caller data.

Use `Prompts::expand(text)` for explicit `<bazaar:ID>` references in trusted
instruction fields. It replaces each reference with the exact selected text.
Inserted component text is not expanded again. An unterminated reference or an
ID outside the selection is an error. Do not apply expansion to user directions,
source documents, or tool results.

Use `Prompts::descriptions(&mut serde_json::Value)` for caller-owned tool or
schema JSON. It walks arrays and objects and expands string fields named
`description`. Other text stays unchanged. This method prepares text; it does
not choose tool structure, validate a schema, or register a toolset. A failure
can leave earlier description fields expanded. Rebuild the caller-owned value
before retrying or admitting a request.

## Preserve requester compatibility

Selection version 1 is Cell's immutable migration baseline. Keep it and every
referenced component version. Keep compatibility components required by
historical jobs when retiring a caller. Retained Bazaar IDs and versions remain
unchanged.

The numeric `toolset_version(historical_max)` helper adds the selection version
above the caller's historical range and rejects exhaustion.
`Prompts::for_toolset_version(owner, version, historical_max)` resolves the
corresponding selection. Historical toolset versions select baseline version 1.
These helpers operate on numbers and do not depend on Nucleus types.

Existing Cell mappings add 2 for Annals, 3 for Platter, and 1 for Krisis,
Semantics, and Weaver. Products retain the original registered text for old
toolset identities. Annals also versions schema IDs when their editable
descriptions change. Structural schema changes remain product code and
requester compatibility changes.

Products retain exact requests or selection versions under their recovery
rules. CI Manager freezes its repair instructions and template before its
first model request and retains every rendered request. Annals records the
selection in its examination prompt version and context digest. Platter captures
it with packet inputs. Other requesters retain their resolved requests; EMT
retains its existing limits on temporary exchange content.

Annals library revisions remain immutable domain snapshots. Bazaar supplies
their initial authored documents. Changing an assigned library revision remains
an explicit product operation. User directions, work documents, and tool results
remain runtime input.

## Failure, privacy, and limits

Missing selections or components, invalid private paths, unsupported database
or selection formats, and invalid templates stop preparation with typed
`Error` failures. Resolve the selected path, content, or template deliberately.
Do not recreate state or substitute the repository seed after a failed read.
Preparation retains no state, starts no agent, registers no schema, and changes
no installed caller.

Loaded content and rendered output can contain private text. Keep them within
the caller's private boundary. Direct Rust reads record no CLI usage. The
rendering helpers require memory proportional to their input and output.
SQLite waits up to five seconds for contention. No capacity, throughput,
content-size, or wall-clock completion guarantee is supplied.

Database schema one, string versions, selection format one, requester schema
and toolset identities, product release, and this contract version are separate
identities. The raw `bazaar::api` types and behavior remain unchanged. This
feature relies on local Bazaar state only; no external service or installed
catalog is a runtime dependency.
