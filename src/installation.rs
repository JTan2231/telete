use crate::paths::{self, Paths};
use crate::signing;
use crate::store::Store;
use anyhow::{Context, Result, ensure};
use cell_install::{
    InstallLayout, LockKind, LockSpec, ProviderSpec, PublicEntry, PublicKind, ReleasePlan,
    SourceFile,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

const LABEL: &str = "org.cell.telete";

fn layout() -> InstallLayout {
    InstallLayout {
        product: "telete".into(),
        application: "Telete".into(),
        product_lock: LockSpec {
            path: "Library/Application Support/Telete/install.lock.d".into(),
            kind: LockKind::Directory,
        },
        catalog_lock: Some(LockSpec {
            path: "Library/Application Support/Chancery/.catalog-update-lock".into(),
            kind: LockKind::Shlock,
        }),
        public: vec![
            PublicEntry {
                path: ".local/bin/telete".into(),
                artifact: "bin/telete".into(),
                kind: PublicKind::Symlink,
                mode: 0o555,
            },
            PublicEntry {
                path: "Library/Application Support/Chancery/providers/telete".into(),
                artifact: "provider".into(),
                kind: PublicKind::Symlink,
                mode: 0o555,
            },
            PublicEntry {
                path: format!("Library/LaunchAgents/{LABEL}.plist").into(),
                artifact: "launchagent.plist".into(),
                kind: PublicKind::Copy,
                mode: 0o600,
            },
        ],
    }
}

fn unsupported_format(_: &Path) -> cell_install::Result<cell_install::ReleaseInfo> {
    Err(cell_install::Error::new(
        "unsupported Telete installation format",
    ))
}

fn uid() -> Result<String> {
    let output = Command::new("/usr/bin/id").arg("-u").output()?;
    ensure!(
        output.status.success(),
        "cannot establish launchd user identity"
    );
    let uid = String::from_utf8(output.stdout)?.trim().to_string();
    ensure!(
        !uid.is_empty() && uid.bytes().all(|b| b.is_ascii_digit()) && uid != "0",
        "Telete service requires a non-root user"
    );
    Ok(uid)
}

fn launchctl(arguments: &[&str]) -> Result<std::process::Output> {
    ensure!(
        cfg!(target_os = "macos"),
        "Telete service requires macOS launchd"
    );
    Command::new("/bin/launchctl")
        .args(arguments)
        .output()
        .context("launchctl unavailable")
}

fn installed(paths: &Paths) -> Result<cell_install::InstallSnapshot> {
    let receipt: Value = serde_json::from_slice(
        &fs::read(paths.root.join("installation.json"))
            .context("Telete is not installed for this state selection")?,
    )?;
    ensure!(
        receipt["state"].as_str() == paths.root.to_str(),
        "installed Telete service belongs to another state selection"
    );
    let snapshot =
        cell_install::inspect_installation(&layout(), &signing::home()?, &unsupported_format)?;
    ensure!(
        snapshot.current.is_some(),
        "Telete program selection is absent"
    );
    Ok(snapshot)
}

fn loaded() -> Result<bool> {
    Ok(launchctl(&["print", &format!("gui/{}/{LABEL}", uid()?)])?
        .status
        .success())
}

fn wait_stopped(paths: &Paths) -> Result<()> {
    let started = Instant::now();
    loop {
        if let Ok(_worker) = paths::lock(&paths.root.join("worker.lock"), false) {
            crate::manager::require_service_stoppable(paths)?;
            return Ok(());
        }
        ensure!(
            started.elapsed() < Duration::from_secs(10),
            "Telete worker ownership remains held after service stop"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn stop(paths: &Paths) -> Result<()> {
    let _guards = if crate::manager::require_quiescent(paths).is_ok() {
        Vec::new()
    } else {
        let mut guards = crate::broker::settled_guards(paths)?;
        guards.push(paths::lock(
            &paths.root.join("deployments/deployment.lock"),
            false,
        )?);
        crate::manager::require_service_stoppable(paths)?;
        guards
    };
    if loaded()? {
        installed(paths)?;
        let output = launchctl(&["bootout", &format!("gui/{}/{LABEL}", uid()?)])?;
        ensure!(
            output.status.success(),
            "cannot stop owned Telete service: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    wait_stopped(paths)
}

fn xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn plist(executable: &Path, state: &Path) -> Result<String> {
    ensure!(
        executable.is_absolute() && state.is_absolute(),
        "service executable and state must be absolute"
    );
    let executable = xml(executable
        .to_str()
        .context("service executable is not UTF-8")?);
    let state = xml(state.to_str().context("service state is not UTF-8")?);
    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict>\n<key>Label</key><string>{LABEL}</string>\n<key>ProgramArguments</key><array><string>{executable}</string><string>--state</string><string>{state}</string><string>worker</string></array>\n<key>RunAtLoad</key><true/><key>KeepAlive</key><true/>\n<key>StandardOutPath</key><string>/dev/null</string><key>StandardErrorPath</key><string>/dev/null</string>\n</dict></plist>\n"
    ))
}

#[allow(clippy::too_many_lines)]
pub(crate) fn install(paths: &Paths) -> Result<Value> {
    ensure!(
        cfg!(target_os = "macos"),
        "Telete installation requires macOS"
    );
    let _admission = paths::lock(&paths.root.join("admission.lock"), false)?;
    let store = Store::open(&paths.root, false)?;
    store.config()?;
    crate::manager::require_quiescent(paths)?;
    let _deployment = paths::lock(&paths.root.join("deployments/deployment.lock"), false)?;
    ensure!(
        !paths.root.join("deployments/active.json").try_exists()?,
        "settle interrupted Telete deployment before installation"
    );
    stop(paths)?;
    let _worker = paths::lock(&paths.root.join("worker.lock"), false)?;
    crate::manager::require_quiescent(paths)?;
    let policy = signing::selected(paths)?;
    signing::preflight(&policy)?;
    let source = fs::canonicalize(std::env::current_exe()?)?;
    ensure!(
        fs::symlink_metadata(&source)?.is_file(),
        "running Telete executable is not regular"
    );
    let stage = tempfile::Builder::new()
        .prefix(".install-")
        .tempdir_in(paths.root.join("tmp"))?;
    let binary = stage.path().join("telete");
    fs::copy(&source, &binary)?;
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o755))?;
    signing::sign(&binary, &policy, "telete", "telete")?;
    let mut files = BTreeMap::from([(
        "bin/telete".into(),
        SourceFile {
            source: binary,
            mode: 0o555,
        },
    )]);
    // The documentation is compiled into the running binary. Publication never
    // discovers provider bytes through the changing source checkout.
    let assets: [(&str, &[u8]); 6] = [
        ("provider.json", include_bytes!("../chancery/provider.json")),
        ("overview.md", include_bytes!("../chancery/overview.md")),
        (
            "entries/queue-operate.json",
            include_bytes!("../chancery/entries/queue-operate.json"),
        ),
        (
            "manuals/queue-operate.md",
            include_bytes!("../chancery/manuals/queue-operate.md"),
        ),
        (
            "entries/signing-operate.json",
            include_bytes!("../chancery/entries/signing-operate.json"),
        ),
        (
            "manuals/signing-operate.md",
            include_bytes!("../chancery/manuals/signing-operate.md"),
        ),
    ];
    for (name, bytes) in assets {
        let destination = stage.path().join("provider").join(name);
        fs::create_dir_all(
            destination
                .parent()
                .context("provider asset parent absent")?,
        )?;
        fs::write(&destination, bytes)?;
        files.insert(
            format!("provider/{name}"),
            SourceFile {
                source: destination,
                mode: 0o444,
            },
        );
    }
    let prompt_seed = stage.path().join("seed.json");
    fs::write(&prompt_seed, include_bytes!("../prompts/seed.json"))?;
    files.insert(
        "prompts/seed.json".into(),
        SourceFile {
            source: prompt_seed,
            mode: 0o444,
        },
    );
    let home = signing::home()?;
    let executable = home.join("Library/Application Support/Telete/install/runtime/bin/telete");
    let launchagent = stage.path().join("launchagent.plist");
    fs::write(&launchagent, plist(&executable, &paths.root)?)?;
    files.insert(
        "launchagent.plist".into(),
        SourceFile {
            source: launchagent,
            mode: 0o600,
        },
    );
    let version = env!("CARGO_PKG_VERSION").to_string();
    let plan = ReleasePlan {
        files,
        versions: BTreeMap::from([("telete".into(), format!("telete {version}"))]),
        providers: BTreeMap::from([(
            "telete".into(),
            ProviderSpec {
                path: "provider".into(),
                version,
            },
        )]),
    };
    let layout = layout();
    let before = cell_install::inspect_detached_installation(&layout, &home, &unsupported_format)?;
    let prepared = cell_install::prepare_release(&layout, &home, &plan)?;
    ensure!(
        signing::selected(paths)? == policy,
        "signing selection changed during Telete installation"
    );
    let mut transaction = cell_install::lock_installation(&layout, &home, &unsupported_format)?;
    transaction.recheck(&before)?;
    let receipt = transaction.publish(&prepared, &before, |_| Ok(()))?;
    let result = json!({"schema":1,"state":paths.root,"label":LABEL,"release_id":prepared.info.release_id,"selection":receipt,"service_started":false,"paused":true});
    paths::atomic_json(&paths.root.join("installation.json"), &result)?;
    store.set("paused", &true)?;
    Ok(result)
}

pub(crate) fn service_status(paths: &Paths) -> Result<Value> {
    let selection = installed(paths)?;
    Ok(
        json!({"schema":1,"label":LABEL,"state":paths.root,"installed":selection.current,"loaded":loaded()?}),
    )
}

pub(crate) fn service_start(paths: &Paths) -> Result<Value> {
    let _admission = paths::lock(&paths.root.join("admission.lock"), false)?;
    installed(paths)?;
    ensure!(!loaded()?, "Telete service is already loaded");
    let _worker = paths::lock(&paths.root.join("worker.lock"), false)?;
    let agent = signing::home()?.join(format!("Library/LaunchAgents/{LABEL}.plist"));
    let output = launchctl(&[
        "bootstrap",
        &format!("gui/{}", uid()?),
        agent.to_str().context("LaunchAgent path is not UTF-8")?,
    ])?;
    ensure!(
        output.status.success(),
        "cannot load owned Telete service: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(json!({"label":LABEL,"loaded":true,"paused":Store::open(&paths.root,false)?.paused()?}))
}

pub(crate) fn service_stop(paths: &Paths) -> Result<Value> {
    let _admission = paths::lock(&paths.root.join("admission.lock"), false)?;
    installed(paths)?;
    stop(paths)?;
    Ok(json!({"label":LABEL,"loaded":false,"paused":true}))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    #[test]
    fn launchagent_has_separate_identity_and_literal_state_arguments() {
        let text = plist(
            Path::new("/home/a&b/Telete/install/runtime/bin/telete"),
            Path::new("/Volumes/Work/telete-x"),
        )
        .unwrap();
        assert!(text.contains("<string>org.cell.telete</string>"));
        assert!(text.contains("/home/a&amp;b/Telete/install/runtime/bin/telete"));
        assert!(text.contains(
            "<string>--state</string><string>/Volumes/Work/telete-x</string><string>worker</string>"
        ));
        assert!(plist(Path::new("relative"), Path::new("/state")).is_err());
    }
    #[test]
    fn published_paths_have_only_telete_ownership() {
        let layout = layout();
        assert_eq!(layout.product, "telete");
        assert_eq!(layout.application, "Telete");
        assert!(
            layout
                .public
                .iter()
                .all(|entry| entry.path.to_string_lossy().contains("telete"))
        );
        assert_eq!(layout.public.len(), 3);
        assert_eq!(
            layout.public[1].path,
            Path::new("Library/Application Support/Chancery/providers/telete")
        );
        assert_eq!(
            layout.catalog_lock.unwrap().path,
            Path::new("Library/Application Support/Chancery/.catalog-update-lock")
        );
    }
}
