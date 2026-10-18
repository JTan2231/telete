//! Usher's installation boundary, separate from its read-only recognition CLI.

use std::collections::BTreeMap;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use cell_install::{
    Disposition, InstallLayout, InstallSnapshot, InstallSpec, Installation, LockKind, LockSpec,
    PreparedRelease, ProviderSpec, PublicEntry, PublicKind, ReleaseInfo, ReleasePlan, SourceFile,
};
use clap::{Args, Parser, Subcommand};
use serde::Serialize;
use serde_json::{Value, json};

const SPEC: InstallSpec = InstallSpec {
    product: "usher",
    application: "Usher",
    commands: &["usher", "usher-install"],
    provider: "usher",
};

#[derive(Parser)]
#[command(
    name = "usher-install",
    version,
    about = "Install and recover owned Usher program releases"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Install exact binaries and the matching provider through one owned selector.
    Install(CandidateArgs),
    /// Inspect the owned installation without changing it.
    Inspect(HomeArgs),
    /// Restore an owned retained release using the current installer.
    Recover {
        #[arg(long)]
        release: PathBuf,
        #[command(flatten)]
        home: HomeArgs,
        #[arg(long)]
        expected_current: Option<String>,
    },
    /// Install the supplied candidate files; accept one recipe request on stdin.
    Deploy,
}

#[derive(Args)]
struct HomeArgs {
    /// Intentional isolated or alternate current-user installation boundary.
    #[arg(long)]
    home: Option<PathBuf>,
}

#[derive(Args)]
struct CandidateArgs {
    /// Exact tested Usher recognition executable.
    #[arg(long)]
    binary: PathBuf,
    /// Version-matched product-owned Chancery bundle.
    #[arg(long)]
    bundle: PathBuf,
    #[command(flatten)]
    home: HomeArgs,
    /// Require absent or `releases/<ID>`; omission snapshots before locking.
    #[arg(long)]
    expected_current: Option<String>,
}

#[derive(Debug, Serialize)]
struct Failure {
    code: &'static str,
    detail: String,
    disposition: Disposition,
}

impl Failure {
    fn input(detail: &str) -> Self {
        Self {
            code: "invalid_input",
            detail: detail.to_owned(),
            disposition: Disposition::Unchanged,
        }
    }
}

impl From<cell_install::Error> for Failure {
    fn from(error: cell_install::Error) -> Self {
        Self {
            code: "installation_failed",
            detail: error.message,
            disposition: error.disposition,
        }
    }
}

impl From<io::Error> for Failure {
    fn from(error: io::Error) -> Self {
        Self {
            code: "io_failed",
            detail: format!("installation I/O failed: {}", error.kind()),
            disposition: Disposition::Unchanged,
        }
    }
}

impl From<serde_json::Error> for Failure {
    fn from(_: serde_json::Error) -> Self {
        Self::input("invalid installation JSON")
    }
}

type Result<T> = std::result::Result<T, Failure>;

fn home_path(home: Option<PathBuf>) -> Result<PathBuf> {
    let path = home
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .ok_or_else(|| Failure::input("HOME or --home is required"))?;
    if !path.is_absolute() {
        return Err(Failure::input("installation home must be absolute"));
    }
    Ok(path)
}

fn layout() -> InstallLayout {
    InstallLayout {
        product: SPEC.product.to_owned(),
        application: SPEC.application.to_owned(),
        product_lock: LockSpec {
            path: "Library/Application Support/Usher/install/.update-lock".into(),
            kind: LockKind::Shlock,
        },
        catalog_lock: Some(LockSpec {
            path: "Library/Application Support/Chancery/.catalog-update-lock".into(),
            kind: LockKind::Shlock,
        }),
        public: vec![
            public_entry(".local/bin/usher", "bin/usher"),
            public_entry(".local/bin/usher-install", "bin/usher-install"),
            public_entry(
                "Library/Application Support/Chancery/providers/usher",
                "share/chancery/usher",
            ),
        ],
    }
}

fn public_entry(path: &str, artifact: &str) -> PublicEntry {
    PublicEntry {
        path: path.into(),
        artifact: artifact.into(),
        kind: PublicKind::Symlink,
        mode: 0o555,
    }
}

// Retain Usher's predecessor readers without using their publication path.
fn legacy(root: &Path) -> cell_install::Result<ReleaseInfo> {
    let prior = cell_install::read_release(&SPEC, root)?;
    let mut public = layout().public;
    if root.join("manifest.json").try_exists()? {
        let manifest: cell_install::Manifest =
            serde_json::from_slice(&std::fs::read(root.join("manifest.json"))?)?;
        Ok(ReleaseInfo {
            release_id: prior.release_id,
            format: prior.format,
            versions: manifest.versions,
            files: manifest.files,
            public,
        })
    } else {
        public.retain(|entry| entry.path != Path::new(".local/bin/usher-install"));
        let mut info = cell_install::legacy::read(
            root,
            &cell_install::legacy::LegacySpec {
                format: "legacy-usher-v1",
                manifest: "manifest.txt",
                metadata: &[],
                proofs: &[],
                providers: &[],
                hash_path_lines: false,
            },
            public,
        )?;
        info.format = prior.format;
        info.versions.insert("usher".into(), prior.version);
        Ok(info)
    }
}

fn selected(info: &ReleaseInfo) -> Installation {
    Installation {
        current: format!("releases/{}", info.release_id),
        release_id: info.release_id.clone(),
        version: info.versions.get("usher").cloned().unwrap_or_default(),
        format: info.format.clone(),
    }
}

fn expected(before: &InstallSnapshot, value: Option<&str>) -> cell_install::Result<()> {
    let current = before.current.as_ref().map_or_else(
        || "absent".to_owned(),
        |info| format!("releases/{}", info.release_id),
    );
    if value.is_some_and(|value| value != current) {
        return Err(cell_install::Error::new(
            "stale deployment: installed selection changed",
        ));
    }
    Ok(())
}

fn install(
    home: &Path,
    binary: PathBuf,
    provider_dir: &Path,
    expected_current: Option<&str>,
) -> Result<Installation> {
    if !binary.is_absolute() || !provider_dir.is_absolute() {
        return Err(Failure::input("candidate paths must be absolute"));
    }
    let layout = layout();
    let before = cell_install::inspect_installation(&layout, home, &legacy)?;
    expected(&before, expected_current)?;
    let installer = std::env::current_exe()?;
    for source in [&binary, &installer] {
        let metadata = std::fs::symlink_metadata(source).map_err(cell_install::Error::from)?;
        if !metadata.is_file() || metadata.nlink() != 1 || metadata.mode() & 0o111 == 0 {
            return Err(cell_install::Error::new(
                "candidate must be an absolute executable regular file",
            )
            .into());
        }
    }
    let mut files = BTreeMap::from([
        (
            "bin/usher".into(),
            SourceFile {
                source: binary,
                mode: 0o555,
            },
        ),
        (
            "bin/usher-install".into(),
            SourceFile {
                source: installer.clone(),
                mode: 0o555,
            },
        ),
        (
            "package/install".into(),
            SourceFile {
                source: installer,
                mode: 0o555,
            },
        ),
    ]);
    for relative in cell_install::provider_inventory(provider_dir, &SPEC)?.keys() {
        files.insert(
            format!("share/chancery/usher/{relative}"),
            SourceFile {
                source: provider_dir.join(relative),
                mode: 0o444,
            },
        );
    }
    let version = env!("CARGO_PKG_VERSION").to_owned();
    let plan = ReleasePlan {
        files,
        versions: BTreeMap::from([
            ("usher".into(), version.clone()),
            ("usher-install".into(), version.clone()),
        ]),
        providers: BTreeMap::from([(
            "usher".into(),
            ProviderSpec {
                path: "share/chancery/usher".into(),
                version,
            },
        )]),
    };
    let prepared = cell_install::prepare_release(&layout, home, &plan)?;
    let mut transaction = cell_install::lock_installation(&layout, home, &legacy)?;
    transaction.recheck(&before)?;
    transaction.publish(&prepared, &before, |_| Ok(()))?;
    Ok(selected(&prepared.info))
}

fn recover(home: &Path, release: PathBuf, expected_current: Option<&str>) -> Result<Installation> {
    if !release.is_absolute()
        || release.parent()
            != Some(
                home.join("Library/Application Support/Usher/install/releases")
                    .as_path(),
            )
    {
        return Err(Failure::input(
            "recovery release must belong to this Usher installation",
        ));
    }
    let layout = layout();
    let info = cell_install::read_release_at(&layout, &release, &legacy)?;
    let before = cell_install::inspect_detached_installation(&layout, home, &legacy)?;
    expected(&before, expected_current)?;
    let mut transaction = cell_install::lock_installation(&layout, home, &legacy)?;
    transaction.recheck(&before)?;
    transaction.publish(
        &PreparedRelease {
            root: release,
            info: info.clone(),
        },
        &before,
        |_| Ok(()),
    )?;
    Ok(selected(&info))
}

fn run(command: Command) -> Result<Value> {
    let data = match command {
        Command::Install(args) => {
            let home = home_path(args.home.home)?;
            json!(install(
                &home,
                args.binary,
                &args.bundle,
                args.expected_current.as_deref()
            )?)
        }
        Command::Inspect(args) => {
            let snapshot =
                cell_install::inspect_installation(&layout(), &home_path(args.home)?, &legacy)?;
            json!(snapshot.current.as_ref().map(selected))
        }
        Command::Recover {
            release,
            home,
            expected_current,
        } => {
            let home = home_path(home.home)?;
            json!(recover(&home, release, expected_current.as_deref())?)
        }
        Command::Deploy => {
            let context = cell_install::adapter::Context::read(
                "usher",
                "infrastructure/usher",
                "usher-install",
                env!("CARGO_PKG_VERSION"),
            )?;
            json!(install(
                &context.home,
                context.binary("usher")?,
                &context
                    .request
                    .source_root
                    .join("infrastructure/usher/chancery"),
                None,
            )?)
        }
    };
    Ok(json!({"ok": true, "data": data}))
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli.command) {
        Ok(reply) => {
            println!("{reply}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            let reply = json!({"ok": false, "error": error});
            println!("{reply}");
            ExitCode::from(1)
        }
    }
}
