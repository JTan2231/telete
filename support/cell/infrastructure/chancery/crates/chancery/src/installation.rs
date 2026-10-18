//! Product-owned installation layout and predecessor release proof.
use cell_install::legacy::{LegacyProof, LegacyProvider, LegacySpec};
use cell_install::simple::Spec;
use cell_install::transaction::LockKind;

#[must_use]
pub fn specification() -> Spec {
    Spec {
        product: "chancery",
        application: "Chancery",
        source_directory: "infrastructure/chancery",
        provider_source: "infrastructure/chancery/provider",
        legacy_provider_path: "share/chancery",
        legacy: &LegacySpec {
            format: "1",
            manifest: "manifest.txt",
            metadata: &["version"],
            proofs: &[
                LegacyProof {
                    key: "binary_sha256",
                    paths: &["bin/chancery"],
                },
                LegacyProof {
                    key: "deployer_sha256",
                    paths: &["package/deploy-user.sh"],
                },
            ],
            providers: &[LegacyProvider {
                key: "provider_sha256",
                provider: "chancery",
                path: "share/chancery",
                version_key: "version",
            }],
            hash_path_lines: false,
        },
        wrapper: None,
        lock_kind: LockKind::Directory,
        lock_at_state: false,
    }
}
