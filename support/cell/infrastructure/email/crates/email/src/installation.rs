//! Product-owned installation layout and predecessor release proof.
use cell_install::legacy::{LegacyProof, LegacyProvider, LegacySpec};
use cell_install::simple::Spec;
use cell_install::transaction::LockKind;

/// Apply only Email-owned setup; never return credential bytes to the coordinator.
/// # Errors
/// Rejects malformed settings, invalid credential sources and unsafe storage.
fn configure(context: &cell_install::adapter::Context) -> cell_install::Result<()> {
    let setup: crate::settings::Setup = serde_json::from_value(
        context
            .request
            .settings
            .clone()
            .unwrap_or_else(|| serde_json::json!({})),
    )?;
    crate::settings::configure(&setup)
        .map_err(|error| cell_install::Error::new(error.to_string()))?;
    Ok(())
}

#[must_use]
pub fn specification() -> Spec {
    Spec {
        product: "email",
        application: "Email",
        source_directory: "infrastructure/email",
        provider_source: "infrastructure/email/chancery",
        legacy_provider_path: "share/chancery/email",
        legacy: &LegacySpec {
            format: "1",
            manifest: "manifest.txt",
            metadata: &["version"],
            proofs: &[
                LegacyProof {
                    key: "payload_sha256",
                    paths: &["libexec/email"],
                },
                LegacyProof {
                    key: "frontend_sha256",
                    paths: &["bin/email", "package/email"],
                },
                LegacyProof {
                    key: "deployer_sha256",
                    paths: &["package/deploy-user.sh"],
                },
            ],
            providers: &[LegacyProvider {
                key: "provider_sha256",
                provider: "email",
                path: "share/chancery/email",
                version_key: "version",
            }],
            hash_path_lines: false,
        },
        wrapper: Some(include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../packaging/macos/email"
        ))),
        lock_kind: LockKind::Directory,
        lock_at_state: false,
    }
}

/// Install declared files and apply this product's requested setup.
/// # Errors
/// Returns installation or setup failures.
pub fn deploy(context: &cell_install::adapter::Context) -> cell_install::Result<()> {
    cell_install::simple::deploy_program(&specification(), env!("CARGO_PKG_VERSION"), context)?;
    configure(context)?;
    Ok(())
}
