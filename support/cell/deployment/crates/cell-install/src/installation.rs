use crate::artifact::{
    copy_file, current_uid, inventory, provider_files, regular, simple_format, valid_release_id,
    validate_spec, write_manifest,
};
use crate::{Disposition, Error, InstallSpec, Installation, ReleaseInput, Result, read_release};
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

struct Paths {
    product: &'static str,
    home: PathBuf,
    install: PathBuf,
    releases: PathBuf,
    catalog: PathBuf,
    public: BTreeMap<PathBuf, PathBuf>,
    directories: Vec<(PathBuf, u32)>,
    uid: u32,
}

impl Paths {
    fn new(spec: &InstallSpec, home: &Path) -> Result<Self> {
        validate_spec(spec)?;
        if !home.is_absolute() || fs::canonicalize(home)? != home {
            return Err(Error::new(
                "operator home must be an absolute non-symbolic directory",
            ));
        }
        let uid = current_uid()?;
        owned_directory(home, uid)?;
        let support = home.join("Library/Application Support");
        let state = support.join(spec.application);
        let install = state.join("install");
        let releases = install.join("releases");
        let catalog = support.join("Chancery");
        let cli = home.join(".local/bin");
        let mut public = BTreeMap::new();
        for name in spec.commands {
            public.insert(cli.join(name), install.join("current/bin").join(name));
        }
        public.insert(
            catalog.join("providers").join(spec.provider),
            install.join(format!("current/share/chancery/{}", spec.provider)),
        );
        let directories = vec![
            (home.join("Library"), 0o700),
            (support, 0o700),
            (state, 0o700),
            (install.clone(), 0o700),
            (releases.clone(), 0o700),
            (home.join(".local"), 0o755),
            (cli, 0o755),
            (catalog.clone(), 0o700),
            (catalog.join("providers"), 0o700),
        ];
        Ok(Self {
            product: spec.product,
            home: home.to_owned(),
            install,
            releases,
            catalog,
            public,
            directories,
            uid,
        })
    }

    fn directories(&self, create: bool) -> Result<()> {
        for (path, mode) in &self.directories {
            match fs::symlink_metadata(path) {
                Ok(_) => owned_directory(path, self.uid)?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound && create => {
                    match fs::create_dir(path) {
                        Ok(()) => fs::set_permissions(path, fs::Permissions::from_mode(*mode))?,
                        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                        Err(e) => return Err(e.into()),
                    }
                    owned_directory(path, self.uid)?;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    fn selector_paths(&self) -> Vec<PathBuf> {
        [self.install.join("current"), self.install.join("previous")]
            .into_iter()
            .chain(self.public.keys().cloned())
            .collect()
    }
}

fn owned_directory(path: &Path, uid: u32) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o022 != 0 {
        return Err(Error::new(
            "installation directory ownership or permissions are unsafe",
        ));
    }
    Ok(())
}

type View = BTreeMap<PathBuf, Option<PathBuf>>;

fn selector(path: &Path) -> Result<Option<PathBuf>> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => Ok(Some(fs::read_link(path)?)),
        Ok(_) => Err(Error::new(
            "installation selector is occupied by a non-symbolic path",
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn view(paths: &Paths) -> Result<View> {
    paths
        .selector_paths()
        .into_iter()
        .map(|path| Ok((path.clone(), selector(&path)?)))
        .collect()
}

fn release_selector(value: &Path) -> Result<&str> {
    let value = value
        .to_str()
        .ok_or_else(|| Error::new("invalid installed selection"))?;
    match value.strip_prefix("releases/") {
        Some(id) if valid_release_id(id) => Ok(value),
        _ => Err(Error::new("invalid installed selection")),
    }
}

fn desired_public(spec: &InstallSpec, paths: &Paths, release: &Installation) -> View {
    paths
        .public
        .iter()
        .map(|(path, target)| {
            let legacy_extra = !simple_format(&release.format)
                && spec
                    .commands
                    .iter()
                    .skip(1)
                    .any(|name| path == &paths.home.join(".local/bin").join(name));
            (
                path.clone(),
                if legacy_extra {
                    None
                } else {
                    Some(target.clone())
                },
            )
        })
        .collect()
}

fn inspect_with(spec: &InstallSpec, paths: &Paths) -> Result<Option<Installation>> {
    paths.directories(false)?;
    let current = view(paths)?;
    let Some(selected) = &current[&paths.install.join("current")] else {
        if current.values().any(Option::is_some) {
            return Err(Error::new("installed selectors have no current release"));
        }
        return Ok(None);
    };
    let selected = release_selector(selected)?;
    let release = read_release(spec, &paths.install.join(selected))?;
    owned_directory(&paths.install.join(selected), paths.uid)?;
    for (path, target) in desired_public(spec, paths, &release) {
        if current[&path] != target {
            return Err(Error::new("public selector is foreign or incoherent"));
        }
    }
    if let Some(previous) = &current[&paths.install.join("previous")] {
        let previous = paths.install.join(release_selector(previous)?);
        read_release(spec, &previous)?;
        owned_directory(&previous, paths.uid)?;
    }
    Ok(Some(release))
}

// Explicit recovery may encounter only a subset of an owned publication.
// It still refuses every foreign selector and every invalid selected release.
fn recovery_view(spec: &InstallSpec, paths: &Paths) -> Result<(View, Option<Installation>)> {
    paths.directories(false)?;
    let current = view(paths)?;
    for (path, expected) in &paths.public {
        if current[path]
            .as_ref()
            .is_some_and(|value| value != expected)
        {
            return Err(Error::new("recovery encountered a foreign public selector"));
        }
    }
    let mut selected = None;
    for name in ["current", "previous"] {
        if let Some(value) = &current[&paths.install.join(name)] {
            let release = read_release(spec, &paths.install.join(release_selector(value)?))?;
            if name == "current" {
                selected = Some(release);
            }
        }
    }
    Ok((current, selected))
}

/// Read installed selectors and release metadata without running code.
///
/// # Errors
/// Returns an error for foreign or incoherent selectors, unsafe ownership,
/// invalid retained releases, or inaccessible installation paths.
pub fn inspect(spec: &InstallSpec, home: &Path) -> Result<Option<Installation>> {
    inspect_with(spec, &Paths::new(spec, home)?)
}

pub(crate) struct Lock {
    path: PathBuf,
    pid: String,
}

impl Lock {
    pub(crate) fn acquire(path: PathBuf, uid: u32) -> Result<Self> {
        let wait: u64 = std::env::var("CELL_DEPLOY_LOCK_WAIT_SECONDS")
            .unwrap_or_else(|_| "300".to_owned())
            .parse()
            .map_err(|_| Error::new("invalid deployment lock timeout"))?;
        let pid = std::process::id().to_string();
        let deadline = Instant::now() + Duration::from_secs(wait);
        loop {
            match fs::symlink_metadata(&path) {
                Ok(meta) if !meta.is_file() || meta.nlink() != 1 || meta.uid() != uid => {
                    return Err(Error::new("deployment lock is foreign or unsafe"));
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
            if Command::new("/usr/bin/shlock")
                .arg("-p")
                .arg(&pid)
                .arg("-f")
                .arg(&path)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()?
                .success()
            {
                return Ok(Self { path, pid });
            }
            if Instant::now() >= deadline {
                return Err(Error::new("deployment or catalog lock unavailable"));
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        if regular(&self.path).is_ok()
            && fs::read_to_string(&self.path)
                .is_ok_and(|value| value.lines().next() == Some(self.pid.as_str()))
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn versions(
    spec: &InstallSpec,
    input: &ReleaseInput,
    _home: &Path,
) -> Result<BTreeMap<String, String>> {
    if input
        .binaries
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>()
        != spec
            .commands
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
    {
        return Err(Error::new(
            "candidate commands do not match the installation specification",
        ));
    }
    let mut versions = BTreeMap::new();
    for name in spec.commands {
        let path = &input.binaries[*name];
        if !path.is_absolute() || regular(path)?.mode() & 0o111 == 0 {
            return Err(Error::new(
                "candidate must be an absolute executable regular file",
            ));
        }
        versions.insert((*name).to_owned(), String::new());
    }
    if !input.installer.is_absolute() || regular(&input.installer)?.mode() & 0o111 == 0 {
        return Err(Error::new(
            "recovery installer must be an absolute executable regular file",
        ));
    }
    Ok(versions)
}

fn stage(
    spec: &InstallSpec,
    paths: &Paths,
    input: &ReleaseInput,
) -> Result<(tempfile::TempDir, String)> {
    let versions = versions(spec, input, &paths.home)?;
    let stage = tempfile::Builder::new()
        .prefix(&format!(
            ".cell-install-stage-{}-{}-",
            spec.product,
            std::process::id()
        ))
        .tempdir_in(&paths.releases)?;
    let root = stage.path();
    fs::create_dir(root.join("bin"))?;
    fs::create_dir(root.join("package"))?;
    for (name, source) in &input.binaries {
        copy_file(source, &root.join("bin").join(name), 0o755)?;
    }
    copy_file(&input.installer, &root.join("package/install"), 0o755)?;
    let bundle = root.join(format!("share/chancery/{}", spec.provider));
    fs::create_dir_all(bundle.join("entries"))?;
    fs::create_dir(bundle.join("manuals"))?;
    let source_files = provider_files(&input.provider_dir, spec)?;
    for relative in source_files.keys() {
        copy_file(
            &input.provider_dir.join(relative),
            &bundle.join(relative),
            0o444,
        )?;
    }
    for dir in inventory(root)?.1 {
        fs::set_permissions(root.join(dir), fs::Permissions::from_mode(0o755))?;
    }
    let manifest = write_manifest(root, spec, versions)?;
    Ok((stage, manifest.release_id))
}

fn selected_name(observed: Option<&Installation>) -> &str {
    observed.map_or("absent", |r| r.current.as_str())
}

fn expected_check(expected: &str, observed: Option<&Installation>) -> Result<()> {
    if expected != "absent" {
        release_selector(Path::new(expected))?;
    }
    if expected != selected_name(observed) {
        return Err(Error::new(
            "stale deployment: installation changed since inspection",
        ));
    }
    Ok(())
}

fn atomic_selector(paths: &Paths, path: &Path, target: Option<&Path>) -> Result<()> {
    if let Some(target) = target {
        let parent = path
            .parent()
            .ok_or_else(|| Error::new("invalid selector parent"))?;
        let temporary = tempfile::Builder::new()
            .prefix(&format!(
                ".cell-install-selector-{}-{}-",
                paths.product,
                std::process::id()
            ))
            .tempdir_in(parent)?;
        let link = temporary.path().join("link");
        symlink(target, &link)?;
        fs::rename(link, path)?;
    } else if selector(path)?.is_some() {
        fs::remove_file(path)?;
    }
    Ok(())
}

fn clean_scratch(paths: &Paths, selectors: bool) -> Result<()> {
    let parents: std::collections::BTreeSet<_> = if selectors {
        paths
            .selector_paths()
            .into_iter()
            .filter_map(|p| p.parent().map(Path::to_owned))
            .collect()
    } else {
        [paths.releases.clone()].into_iter().collect()
    };
    let targets = paths.public.values().cloned().collect();
    clean_scratch_at(
        paths.product,
        paths.uid,
        &parents,
        selectors.then_some(&targets),
    )
}

pub(crate) fn clean_scratch_at(
    product: &str,
    uid: u32,
    parents: &std::collections::BTreeSet<PathBuf>,
    selectors: Option<&std::collections::BTreeSet<PathBuf>>,
) -> Result<()> {
    let kind = if selectors.is_some() {
        "selector"
    } else {
        "stage"
    };
    let prefix = format!(".cell-install-{kind}-{product}-");
    for parent in parents {
        let entries = match fs::read_dir(parent) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let Some(rest) = name.to_str().and_then(|n| n.strip_prefix(&prefix)) else {
                continue;
            };
            let Some((pid, random)) = rest.split_once('-') else {
                return Err(Error::new(
                    "unrecognized installation scratch name retained",
                ));
            };
            let pid: u32 = pid
                .parse()
                .ok()
                .filter(|p| *p > 0 && i32::try_from(*p).is_ok())
                .ok_or_else(|| Error::new("invalid scratch owner retained"))?;
            if random.len() < 6 || !random.bytes().all(|c| c.is_ascii_alphanumeric()) {
                return Err(Error::new(
                    "unrecognized installation scratch name retained",
                ));
            }
            let alive = Command::new("/bin/kill")
                .arg("-0")
                .arg(pid.to_string())
                .env("LC_ALL", "C")
                .output()?;
            if alive.status.success()
                || !String::from_utf8_lossy(&alive.stderr).contains("No such process")
            {
                continue;
            }
            let root = entry.path();
            let meta = fs::symlink_metadata(&root)?;
            if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o777 != 0o700 {
                return Err(Error::new("unsafe installation scratch retained"));
            }
            validate_scratch_tree(uid, &root, selectors)?;
            fs::remove_dir_all(root)?;
        }
    }
    Ok(())
}

fn validate_scratch_tree(
    uid: u32,
    root: &Path,
    selectors: Option<&std::collections::BTreeSet<PathBuf>>,
) -> Result<()> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let meta = fs::symlink_metadata(&path)?;
        if meta.uid() != uid || (!meta.file_type().is_symlink() && meta.mode() & 0o022 != 0) {
            return Err(Error::new("unsafe scratch artifact retained"));
        }
        if let Some(targets) = selectors {
            if entry.file_name() == "copy" && meta.is_file() {
                regular(&path)?;
                continue;
            }
            if entry.file_name() != "link" || !meta.file_type().is_symlink() {
                return Err(Error::new("unrecognized selector scratch retained"));
            }
            let target = fs::read_link(&path)?;
            if !targets.contains(&target) && release_selector(&target).is_err() {
                return Err(Error::new("foreign selector scratch retained"));
            }
        } else if meta.is_dir() {
            validate_scratch_tree(uid, &path, None)?;
        } else {
            regular(&path)?;
        }
    }
    Ok(())
}

fn publish(
    spec: &InstallSpec,
    paths: &Paths,
    target: &Installation,
    before: &View,
    _recorded_recovery: bool,
    check: impl FnOnce() -> Result<()>,
) -> Result<Installation> {
    let _catalog = Lock::acquire(paths.catalog.join(".catalog-update-lock"), paths.uid)?;
    clean_scratch(paths, true)?;
    if view(paths)? != *before {
        return Err(Error::new(
            "stale deployment: selectors changed before publication",
        ));
    }
    let mut after = before.clone();
    if before[&paths.install.join("current")] != Some(PathBuf::from(&target.current)) {
        after.insert(
            paths.install.join("previous"),
            before[&paths.install.join("current")].clone(),
        );
    }
    after.insert(
        paths.install.join("current"),
        Some(PathBuf::from(&target.current)),
    );
    after.extend(desired_public(spec, paths, target));
    let result = (|| {
        // Public links and previous are established before making current visible.
        for (path, value) in &after {
            if path != &paths.install.join("current") && before[path] != *value {
                atomic_selector(paths, path, value.as_deref())?;
            }
        }
        if before[&paths.install.join("current")] != after[&paths.install.join("current")] {
            atomic_selector(
                paths,
                &paths.install.join("current"),
                after[&paths.install.join("current")].as_deref(),
            )?;
        }
        if view(paths)? != after {
            return Err(Error::new("installed selector verification failed"));
        }
        let installed = inspect_with(spec, paths)?
            .ok_or_else(|| Error::new("installed selection is absent"))?;
        if installed != *target {
            return Err(Error::new(
                "installed release differs from the prepared candidate",
            ));
        }
        check()?;
        Ok(installed)
    })();
    result.map_err(|mut error| {
        error.disposition = Disposition::Uncertain;
        error
    })
}

fn detach(paths: &Paths, before: &View, after: &View) -> Disposition {
    let Ok(actual) = view(paths) else {
        return Disposition::Uncertain;
    };
    if actual
        .iter()
        .any(|(path, target)| target != &before[path] && target != &after[path])
    {
        return Disposition::Uncertain;
    }
    for path in paths.selector_paths() {
        if atomic_selector(paths, &path, None).is_err() {
            return Disposition::Uncertain;
        }
    }
    if view(paths).is_ok_and(|actual| actual.values().all(Option::is_none)) {
        Disposition::Detached
    } else {
        Disposition::Uncertain
    }
}

/// Install explicit artifacts, preserving the pre-wait expected selection.
/// No product runtime state is initialized, migrated, held, or released.
///
/// # Errors
/// Returns an error for unsafe or stale installation state, invalid candidates,
/// unavailable locks, or failed publication. A publication failure retains completed
/// selector changes and reports uncertainty.
#[deprecated(
    note = "use prepare_release and lock_installation publication for fixed runtime paths"
)]
pub fn install(
    spec: &InstallSpec,
    home: &Path,
    input: &ReleaseInput,
    expected_current: Option<&str>,
) -> Result<Installation> {
    let paths = Paths::new(spec, home)?;
    let observed = inspect_with(spec, &paths)?;
    let expected = expected_current.unwrap_or_else(|| selected_name(observed.as_ref()));
    expected_check(expected, observed.as_ref())?;
    let before = view(&paths)?;
    paths.directories(true)?;
    let (staged, id) = stage(spec, &paths, input)?;
    let _product = Lock::acquire(paths.install.join(".update-lock"), paths.uid)?;
    clean_scratch(&paths, false)?;
    let locked = inspect_with(spec, &paths)?;
    expected_check(expected, locked.as_ref())?;
    if locked != observed || view(&paths)? != before {
        return Err(Error::new(
            "stale deployment: inspected installation changed",
        ));
    }
    let destination = paths.releases.join(&id);
    match fs::symlink_metadata(&destination) {
        Ok(_) => {
            read_release(spec, &destination)?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            fs::rename(staged.path(), &destination)?;
        }
        Err(e) => return Err(e.into()),
    }
    let prepared = read_release(spec, &destination)?;
    publish(spec, &paths, &prepared, &before, false, || Ok(()))
}

/// Restore one retained program release. The caller owns any
/// necessary runtime/state compatibility decision. Legacy extra commands detach.
///
/// # Errors
/// Returns an error for foreign or stale selections, invalid retained content,
/// unavailable locks, or failed publication checks. Mutation failures carry an
/// explicit restoration/detachment/uncertainty disposition.
#[deprecated(note = "use InstallTransaction::restore for fixed runtime paths")]
pub fn restore(
    spec: &InstallSpec,
    home: &Path,
    target_selector: &str,
    expected_current: Option<&str>,
) -> Result<Installation> {
    release_selector(Path::new(target_selector))?;
    let paths = Paths::new(spec, home)?;
    let (before, observed) = recovery_view(spec, &paths)?;
    let expected = expected_current.unwrap_or_else(|| selected_name(observed.as_ref()));
    expected_check(expected, observed.as_ref())?;
    paths.directories(true)?;
    let _product = Lock::acquire(paths.install.join(".update-lock"), paths.uid)?;
    clean_scratch(&paths, false)?;
    if recovery_view(spec, &paths)? != (before.clone(), observed) {
        return Err(Error::new("stale recovery: inspected installation changed"));
    }
    let target = read_release(spec, &paths.install.join(target_selector))?;
    publish(spec, &paths, &target, &before, false, || Ok(()))
}

/// Repair an interrupted selector-only publication using the captured prior
/// installation and exact candidate artifacts. Never repeat candidate apply.
/// Only an absent, exact prior, or exact candidate selection can be repaired.
///
/// # Errors
/// Returns an error when the selection cannot be attributed to the supplied
/// prior/candidate, when paths or releases are unsafe, or when locks, repair,
/// or publication checks fail. It never replaces an unknown selection.
#[deprecated(note = "use InstallTransaction::recover for fixed runtime paths")]
pub fn recover_installation(
    spec: &InstallSpec,
    home: &Path,
    _input: &ReleaseInput,
    prior: Option<&Installation>,
) -> Result<Option<Installation>> {
    let paths = Paths::new(spec, home)?;
    let (before, observed) = recovery_view(spec, &paths)?;
    paths.directories(true)?;
    let _product = Lock::acquire(paths.install.join(".update-lock"), paths.uid)?;
    clean_scratch(&paths, false)?;
    if recovery_view(spec, &paths)? != (before.clone(), observed.clone()) {
        return Err(Error::new(
            "stale recovery: installation changed since inspection",
        ));
    }
    if let Some(selected) = observed {
        let result = publish(spec, &paths, &selected, &before, true, || Ok(()))?;
        return Ok(Some(result));
    }
    if prior.is_some() || before[&paths.install.join("previous")].is_some() {
        return Err(Error::new(
            "recovery cannot prove the captured prior selection",
        ));
    }
    let _catalog = Lock::acquire(paths.catalog.join(".catalog-update-lock"), paths.uid)?;
    clean_scratch(&paths, true)?;
    if view(&paths)? != before {
        return Err(Error::new("stale recovery: partial selectors changed"));
    }
    let absent = paths
        .selector_paths()
        .into_iter()
        .map(|path| (path, None))
        .collect();
    let disposition = detach(&paths, &before, &absent);
    if disposition != Disposition::Detached {
        return Err(Error {
            message: "recovery could not detach the partial first installation".to_owned(),
            disposition,
        });
    }
    Ok(None)
}

#[cfg(all(test, target_os = "macos"))]
mod tests;
