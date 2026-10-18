# Bazaar

Semantics-Project: bazaar

- Keep raw Bazaar strings opaque, keyed by ID and immutable version.
- Use Cell terminology until a Bazaar Semantics repository is registered.
- Bazaar owns shared prompt selections, rendering, trusted references,
  description expansion, and explicit imports over its string store.
- Products own authored meaning, runtime inputs, models, permissions, schemas,
  tool implementations, request assembly, domain results, and recovery.
  Keep agent execution and Nucleus types outside Bazaar.
- Preserve every committed version. Updates append complete content, including
  when the content matches an earlier version.
- Keep the Rust API in process. Read operations must not create or repair state.
- Keep full feature explanations in the product's Chancery feature contracts.
  Procedures keep prerequisites, effects, stop conditions, and verification and
  require the relevant features. Update the owning feature and affected procedures
  together. Other documentation provides entry points to those explanations.
- Submit committed code changes through `./ci.sh submit COMMIT` from the Cell
  root and verify the manager job outcome. CI includes deployment. Publication,
  caller migration, and live content updates remain separate operations.
