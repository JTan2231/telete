//! On-demand background execution. Locks own processes; PIDs are diagnostics.

use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use serde_json::json;

use crate::paths::{self, Paths};
use crate::{manager, store};

fn log(paths: &Paths, name: &str) -> Result<File> {
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(paths.root.join(name))?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.uid() == rustix::process::getuid().as_raw()
            && metadata.mode() & 0o777 == 0o600,
        "background log must be a private user-owned regular file"
    );
    file.set_len(0)?;
    Ok(file)
}

fn spawn(paths: &Paths, command: &str, log_name: &str) -> Result<Child> {
    let log = log(paths, log_name)?;
    let mut child = Command::new(std::env::current_exe()?);
    child
        .args([command, "--state"])
        .arg(&paths.root)
        .current_dir(&paths.root)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log));
    if command == "internal-supervise" {
        detach(&mut child);
    }
    child.spawn().with_context(|| format!("start {command}"))
}

#[allow(unsafe_code)]
fn detach(command: &mut Command) {
    // SAFETY: setsid is async-signal-safe, and this hook performs no allocation
    // or locking. Detach before exec so no terminal signal can reach an
    // acknowledged background process during its Rust startup.
    unsafe {
        command.pre_exec(|| rustix::process::setsid().map(|_| ()).map_err(Into::into));
    }
}

/// Serializes startup against the supervisor's final empty-queue decision.
pub(crate) fn ensure(paths: &Paths) -> Result<()> {
    let lifecycle = paths::lock(&paths.root.join("lifecycle.lock"), true)?;
    lifecycle.close_on_exec()?;
    if !manager::needs_worker(paths)? {
        return Ok(());
    }
    let Some(owner) = paths::try_lock(&paths.root.join("supervisor.lock"))? else {
        return Ok(());
    };
    match fs::remove_file(paths.root.join("worker.json")) {
        Ok(()) => (),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
        Err(error) => return Err(error.into()),
    }
    drop(owner);
    // lifecycle.lock excludes other starters until acknowledgement. Release
    // supervisor.lock before spawn so a fast child can acquire it immediately.
    let mut child = spawn(paths, "internal-supervise", "supervisor.log")?;
    let started = Instant::now();
    loop {
        if let Ok(bytes) = fs::read(paths.root.join("worker.json"))
            && let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes)
            && value["supervisor_pid"].as_u64() == Some(u64::from(child.id()))
        {
            if let Some(error) = value["error"].as_str() {
                bail!("background execution could not start: {error}");
            }
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            bail!("background supervisor exited during startup: {status}; inspect supervisor.log");
        }
        if started.elapsed() >= Duration::from_secs(5) {
            bail!("background supervisor did not acknowledge startup; inspect supervisor.log");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn diagnostic(paths: &Paths, pid: Option<u32>, error: Option<&str>) -> Result<()> {
    paths::atomic_json(
        &paths.root.join("worker.json"),
        &json!({"supervisor_pid":std::process::id(),"worker_pid":pid,
            "error":error,"updated":store::now()}),
    )
}

pub(crate) async fn supervise(paths: &Paths) -> Result<()> {
    let Some(owner) = paths::try_lock(&paths.root.join("supervisor.lock"))? else {
        return Ok(());
    };
    owner.close_on_exec()?;
    loop {
        let error = match spawn(paths, "internal-worker", "worker.log") {
            Ok(mut child) => {
                diagnostic(paths, Some(child.id()), None)?;
                let status = child.wait()?;
                (!status.success()).then(|| format!("worker exited: {status}"))
            }
            Err(error) => Some(format!("{error:#}")),
        };
        diagnostic(paths, None, error.as_deref())?;
        let lifecycle = paths::lock(&paths.root.join("lifecycle.lock"), true)?;
        lifecycle.close_on_exec()?;
        if !manager::needs_worker(paths)? {
            fs::remove_file(paths.root.join("worker.json"))?;
            // Release ownership while startup is still excluded. A concurrent
            // submitter will either find us alive or arrange our replacement.
            drop(owner);
            return Ok(());
        }
        drop(lifecycle);
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}
