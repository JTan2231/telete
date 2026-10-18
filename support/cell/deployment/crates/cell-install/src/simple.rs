//! Shared installer entry point for products whose installation selects program
//! bytes without owning a database or service lifecycle.

use crate::adapter::{self, Context};
use crate::legacy::{self, LegacySpec};
use crate::transaction::{
    self as tx, InstallLayout, InstallSnapshot, LockKind, LockSpec, PreparedRelease, ProviderSpec,
    PublicEntry, PublicKind, ReleaseInfo, ReleasePlan, SourceFile,
};
use crate::{Error, Result};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// One product-owned recipe. Its completion is reported by process exit only.
pub type Deployment = fn(&Context) -> Result<()>;

pub struct Spec {
    pub product: &'static str,
    pub application: &'static str,
    pub source_directory: &'static str,
    pub provider_source: &'static str,
    pub legacy_provider_path: &'static str,
    pub legacy: &'static LegacySpec,
    pub wrapper: Option<&'static [u8]>,
    pub lock_kind: LockKind,
    pub lock_at_state: bool,
}

impl Spec {
    #[must_use]
    pub fn layout(&self) -> InstallLayout {
        let state = format!("Library/Application Support/{}", self.application);
        InstallLayout {
            product: self.product.to_owned(),
            application: self.application.to_owned(),
            product_lock: LockSpec {
                path: PathBuf::from(format!(
                    "{state}/{suffix}.update-lock",
                    suffix = if self.lock_at_state { "" } else { "install/" }
                )),
                kind: self.lock_kind.clone(),
            },
            catalog_lock: Some(LockSpec {
                path: PathBuf::from("Library/Application Support/Chancery/.catalog-update-lock"),
                kind: LockKind::Shlock,
            }),
            public: vec![
                self.command_entry(false),
                self.installer_entry(),
                self.provider_entry(false),
            ],
        }
    }

    fn command_entry(&self, _legacy: bool) -> PublicEntry {
        PublicEntry {
            path: PathBuf::from(format!(".local/bin/{}", self.product)),
            artifact: format!("bin/{}", self.product),
            kind: PublicKind::Symlink,
            mode: 0o555,
        }
    }
    fn installer_entry(&self) -> PublicEntry {
        PublicEntry {
            path: PathBuf::from(format!(".local/bin/{}-install", self.product)),
            artifact: format!("bin/{}-install", self.product),
            kind: PublicKind::Symlink,
            mode: 0o555,
        }
    }
    fn provider_entry(&self, old: bool) -> PublicEntry {
        PublicEntry {
            path: PathBuf::from(format!(
                "Library/Application Support/Chancery/providers/{}",
                self.product
            )),
            artifact: if old {
                self.legacy_provider_path.to_owned()
            } else {
                format!("share/chancery/{}", self.product)
            },
            kind: PublicKind::Symlink,
            mode: 0o555,
        }
    }

    /// Read only this product's supported predecessor release.
    ///
    /// # Errors
    /// Returns an error for unsupported or unreadable predecessor metadata.
    pub fn read_legacy(&self, root: &Path) -> Result<ReleaseInfo> {
        if self.legacy.proofs.is_empty() && self.legacy.providers.is_empty() {
            return Err(Error::new("no predecessor release format is supported"));
        }
        legacy::read(
            root,
            self.legacy,
            vec![self.command_entry(true), self.provider_entry(true)],
        )
    }

    /// Read only this product's supported predecessor release.
    ///
    /// # Errors
    /// Returns an error for foreign or unproved predecessor content.
    pub fn legacy(&self, root: &Path) -> Result<ReleaseInfo> {
        if self.legacy.proofs.is_empty() && self.legacy.providers.is_empty() {
            return Err(Error::new("no predecessor release format is supported"));
        }
        let values = legacy::manifest(&root.join(self.legacy.manifest))?;
        if values
            .get("product")
            .is_some_and(|product| product != self.product)
        {
            return Err(Error::new("foreign legacy product"));
        }
        legacy::verify(
            root,
            self.legacy,
            vec![self.command_entry(true), self.provider_entry(true)],
        )
    }

    fn install_root(&self, home: &Path) -> PathBuf {
        home.join("Library/Application Support")
            .join(self.application)
            .join("install")
    }
}

struct Input {
    home: PathBuf,
    binary: Option<PathBuf>,
    bundle: Option<PathBuf>,
    release: Option<PathBuf>,
    expected: Option<String>,
}

fn input(args: &[String]) -> Result<Input> {
    let mut value = Input {
        home: std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| Error::new("HOME is required"))?,
        binary: None,
        bundle: None,
        release: None,
        expected: None,
    };
    let mut args = args.iter();
    while let Some(flag) = args.next() {
        let argument = args
            .next()
            .ok_or_else(|| Error::new("installer flag requires a value"))?;
        match flag.as_str() {
            "--home" => value.home = PathBuf::from(argument),
            "--binary" => value.binary = Some(PathBuf::from(argument)),
            "--bundle" => value.bundle = Some(PathBuf::from(argument)),
            "--release" => value.release = Some(PathBuf::from(argument)),
            "--expected-current" => value.expected = Some(argument.clone()),
            _ => return Err(Error::new("unknown installer flag")),
        }
    }
    if !value.home.is_absolute()
        || [&value.binary, &value.bundle, &value.release]
            .into_iter()
            .flatten()
            .any(|path| !path.is_absolute())
    {
        return Err(Error::new("installer paths must be absolute"));
    }
    Ok(value)
}

fn expected(snapshot: &InstallSnapshot, value: Option<&str>) -> Result<()> {
    let selected = snapshot.current.as_ref().map_or_else(
        || "absent".to_owned(),
        |release| format!("releases/{}", release.release_id),
    );
    if value.is_some_and(|value| value != selected) {
        return Err(Error::new("stale deployment: installed selection changed"));
    }
    Ok(())
}

fn plan(spec: &Spec, release_version: &str, args: &Input, scratch: &Path) -> Result<ReleasePlan> {
    let binary = args
        .binary
        .as_ref()
        .ok_or_else(|| Error::new("--binary is required"))?;
    let bundle = args
        .bundle
        .as_ref()
        .ok_or_else(|| Error::new("--bundle is required"))?;
    let installer = std::env::current_exe()?;
    let mut files = BTreeMap::from([
        (
            format!("bin/{}-install", spec.product),
            SourceFile {
                source: installer.clone(),
                mode: 0o555,
            },
        ),
        (
            "package/install".to_owned(),
            SourceFile {
                source: installer,
                mode: 0o555,
            },
        ),
    ]);
    if let Some(wrapper) = spec.wrapper {
        let path = scratch.join("frontend");
        std::fs::write(&path, wrapper)?;
        for relative in [
            format!("bin/{}", spec.product),
            format!("package/{}", spec.product),
        ] {
            files.insert(
                relative,
                SourceFile {
                    source: path.clone(),
                    mode: 0o555,
                },
            );
        }
        files.insert(
            format!("libexec/{}", spec.product),
            SourceFile {
                source: binary.clone(),
                mode: 0o555,
            },
        );
    } else {
        files.insert(
            format!("bin/{}", spec.product),
            SourceFile {
                source: binary.clone(),
                mode: 0o555,
            },
        );
    }
    let inventory = crate::provider_inventory(
        bundle,
        &crate::InstallSpec {
            product: spec.product,
            application: spec.application,
            commands: &[],
            provider: spec.product,
        },
    )?;
    for relative in inventory.keys() {
        files.insert(
            format!("share/chancery/{}/{relative}", spec.product),
            SourceFile {
                source: bundle.join(relative),
                mode: 0o444,
            },
        );
    }
    Ok(ReleasePlan {
        files,
        versions: BTreeMap::from([
            (spec.product.to_owned(), release_version.to_owned()),
            (
                format!("{}-install", spec.product),
                release_version.to_owned(),
            ),
        ]),
        providers: BTreeMap::from([(
            spec.product.to_owned(),
            ProviderSpec {
                path: format!("share/chancery/{}", spec.product),
                version: release_version.to_owned(),
            },
        )]),
    })
}

fn scratch(directory: Option<&Path>) -> std::io::Result<tempfile::TempDir> {
    match directory {
        Some(directory) => tempfile::Builder::new()
            .prefix("cell-install-")
            .tempdir_in(directory),
        None => tempfile::tempdir(),
    }
}

fn install(
    spec: &Spec,
    release_version: &str,
    args: &Input,
    scratch_directory: Option<&Path>,
) -> Result<ReleaseInfo> {
    let layout = spec.layout();
    let legacy = |root: &Path| spec.read_legacy(root);
    let before = tx::inspect_installation(&layout, &args.home, &legacy)?;
    expected(&before, args.expected.as_deref())?;
    let scratch = scratch(scratch_directory)?;
    let plan = plan(spec, release_version, args, scratch.path())?;
    let prepared = tx::prepare_release(&layout, &args.home, &plan)?;
    let mut transaction = tx::lock_installation(&layout, &args.home, &legacy)?;
    transaction.recheck(&before)?;
    transaction.publish(&prepared, &before, |_| Ok(()))?;
    Ok(prepared.info)
}

/// Copy the supplied product files and select their public paths.
///
/// # Errors
/// Returns file, ownership, locking, or selector publication failures.
pub fn deploy_program(spec: &Spec, version: &str, context: &Context) -> Result<ReleaseInfo> {
    let args = Input {
        home: context.home.clone(),
        binary: Some(context.binary(spec.product)?),
        bundle: Some(context.request.source_root.join(spec.provider_source)),
        release: None,
        expected: None,
    };
    install(spec, version, &args, Some(&context.request.run_dir))
}

fn current(spec: &Spec, home: &Path) -> Result<InstallSnapshot> {
    tx::inspect_installation(&spec.layout(), home, &|root| spec.read_legacy(root))
}

#[allow(clippy::too_many_lines)] // Keep the fixed installer operations and their proofs together.
fn execute(spec: &Spec, release_version: &str, arguments: &[String]) -> Result<Value> {
    let (operation, remaining) = arguments
        .split_first()
        .ok_or_else(|| Error::new("installer operation is required"))?;
    if operation == "adapter" {
        return Err(Error::new(
            "the deployment adapter protocol is retired; use deploy",
        ));
    }
    let args = input(remaining)?;
    let data = match operation.as_str() {
        "install" => json!(install(spec, release_version, &args, None)?),
        "inspect" => json!(current(spec, &args.home)?),
        "recover" => {
            let release = args
                .release
                .as_ref()
                .ok_or_else(|| Error::new("--release is required"))?;
            if release.parent() != Some(spec.install_root(&args.home).join("releases").as_path()) {
                return Err(Error::new("recovery release is outside owned installation"));
            }
            let layout = spec.layout();
            let legacy = |root: &Path| spec.read_legacy(root);
            let info = tx::read_release_at(&layout, release, &legacy)?;
            let before = tx::inspect_detached_installation(&layout, &args.home, &legacy)?;
            expected(&before, args.expected.as_deref())?;
            let mut transaction = tx::lock_installation(&layout, &args.home, &legacy)?;
            transaction.publish(
                &PreparedRelease {
                    root: release.clone(),
                    info: info.clone(),
                },
                &before,
                |_| Ok(()),
            )?;
            json!(info)
        }
        "uninstall" if spec.product == "clockwork" => {
            let layout = spec.layout();
            let legacy = |root: &Path| spec.read_legacy(root);
            let before = current(spec, &args.home)?;
            let mut transaction = tx::lock_installation(&layout, &args.home, &legacy)?;
            for (path, transition) in [
                (
                    args.home
                        .join("Library/Application Support/Clockwork/locks"),
                    true,
                ),
                (args.home.join("Library/LaunchAgents"), false),
            ] {
                match std::fs::symlink_metadata(&path) {
                    Ok(metadata) if metadata.is_dir() => {
                        for entry in std::fs::read_dir(&path)? {
                            let name = entry?.file_name();
                            let name = name.to_string_lossy();
                            if transition && name.ends_with(".transition.json")
                                || !transition
                                    && name.starts_with("org.clockwork.")
                                    && name.ends_with(".plist")
                            {
                                return Err(Error::new(
                                    "Clockwork binding transitions must be recovered and bindings disabled before uninstall",
                                ));
                            }
                        }
                    }
                    Ok(_) => {
                        return Err(Error::new(
                            "Clockwork locks or LaunchAgents path is not a regular directory",
                        ));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
            transaction.detach(&before)?;
            json!({"detached":true})
        }
        _ => return Err(Error::new("unsupported installer operation")),
    };
    Ok(json!({"ok":true,"data":data}))
}

/// Run a product's program-file installation boundary.
#[must_use]
pub fn main(spec: &Spec, version: &str) -> ExitCode {
    main_inner(spec, version, None)
}

/// Run the explicit product recipe without a coordinator lifecycle protocol.
#[must_use]
pub fn main_with_deployment(spec: &Spec, version: &str, deployment: Deployment) -> ExitCode {
    main_inner(spec, version, Some(deployment))
}

fn main_inner(spec: &Spec, version: &str, deployment: Option<Deployment>) -> ExitCode {
    let arguments: Vec<_> = std::env::args().skip(1).collect();
    if arguments == ["deploy"] {
        let result = Context::read(
            spec.product,
            spec.source_directory,
            &format!("{}-install", spec.product),
            version,
        )
        .and_then(|context| {
            if let Some(deployment) = deployment {
                deployment(&context)
            } else {
                deploy_program(spec, version, &context).map(|_| ())
            }
        });
        return adapter::finish_deployment(result);
    }
    if arguments == ["--version"] || arguments == ["-V"] {
        println!("{}-install {version}", spec.product);
        return ExitCode::SUCCESS;
    }
    if arguments.is_empty() || arguments == ["--help"] || arguments == ["-h"] {
        println!(
            "{}-install {version}\n\ndeploy < REQUEST.json\ninstall --binary ABS --bundle ABS [--home ABS] [--expected-current absent|releases/ID]\ninspect [--home ABS]\nrecover --release ABS [--home ABS] [--expected-current absent|releases/ID]\nClockwork uninstall detaches only owned selectors.",
            spec.product
        );
        return ExitCode::SUCCESS;
    }
    let adapter = arguments.first().is_some_and(|value| value == "adapter");
    let result = execute(spec, version, &arguments);
    if adapter {
        return adapter::finish(result);
    }
    match result {
        Ok(value) => {
            println!("{value}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            println!(
                "{}",
                json!({"ok":false,"error":{"detail":error.message,"disposition":error.disposition}})
            );
            ExitCode::FAILURE
        }
    }
}
