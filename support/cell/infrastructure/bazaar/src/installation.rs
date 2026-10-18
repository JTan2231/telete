//! Bazaar program selection and empty-state initialization.

use cell_install::{adapter::Context, legacy::LegacySpec, simple::Spec, transaction::LockKind};

#[must_use]
pub fn specification() -> Spec {
    Spec {
        product: "bazaar",
        application: "Bazaar",
        source_directory: "infrastructure/bazaar",
        provider_source: "infrastructure/bazaar/chancery",
        legacy_provider_path: "share/chancery/bazaar",
        legacy: &LegacySpec {
            format: "none",
            manifest: "manifest.txt",
            metadata: &[],
            proofs: &[],
            providers: &[],
            hash_path_lines: false,
        },
        wrapper: None,
        lock_kind: LockKind::Shlock,
        lock_at_state: false,
    }
}

/// Configure schema-one state without adding string versions.
///
/// # Errors
/// Returns an error for unsupported settings or unavailable or incompatible state.
fn configure(context: &Context) -> cell_install::Result<()> {
    if context
        .request
        .settings
        .as_ref()
        .is_some_and(|value| value != &serde_json::json!({}))
    {
        return Err(cell_install::Error::new(
            "Bazaar has no deployment settings",
        ));
    }
    let database = crate::database_path(&context.home);
    let map_error = |error: crate::api::Error| cell_install::Error::new(error.to_string());
    crate::api::Writer::initialize(&database).map_err(map_error)?;
    Ok(())
}

/// Install declared files and apply this product's requested setup.
/// # Errors
/// Returns installation or setup failures.
pub fn deploy(context: &cell_install::adapter::Context) -> cell_install::Result<()> {
    cell_install::simple::deploy_program(&specification(), env!("CARGO_PKG_VERSION"), context)?;
    configure(context)?;
    Ok(())
}
