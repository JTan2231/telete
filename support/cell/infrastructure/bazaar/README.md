# Bazaar

Bazaar stores strings under stable IDs with append-only versions. It also loads
selected prompt versions and prepares their text for callers. Programs use its
in-process Rust API. The CLI uses the same store and owns explicit prompt imports.

```sh
bazaar init
bazaar update example.prompt --file prompt.txt
bazaar get example.prompt
bazaar get example.prompt --version 1
bazaar history example.prompt
```

Read the [product overview](chancery/overview.md) for the feature contracts and
operating procedure. The [Rust API entry point](docs/rust-api.md) directs
programs to those same contracts.

Installed documentation is available through `chancery product bazaar`,
`chancery show ID`, and `chancery resolve ID`.
