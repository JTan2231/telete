use crate::paths::{self, Paths};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SigningPolicy {
    pub schema: u32,
    pub macos: MacSigning,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MacSigning {
    pub profile: String,
    pub certificate_sha1: String,
    pub keychain: PathBuf,
    pub identifier_namespace: String,
}

pub(crate) fn home() -> Result<PathBuf> {
    crate::host_setup::home()
}

fn name(value: &str) -> bool {
    value.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

impl SigningPolicy {
    pub(crate) fn normalize(mut self) -> Result<Self> {
        ensure!(
            self.schema == 1 && self.macos.profile == "local",
            "unsupported signing policy"
        );
        ensure!(
            self.macos.certificate_sha1.len() == 40
                && self
                    .macos
                    .certificate_sha1
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit()),
            "certificate fingerprint must contain 40 hexadecimal digits"
        );
        self.macos.certificate_sha1.make_ascii_lowercase();
        let namespace = &self.macos.identifier_namespace;
        ensure!(
            namespace.contains('.') && namespace.split('.').all(name),
            "invalid permanent signing namespace"
        );
        ensure!(
            self.macos.keychain.is_absolute()
                && !self
                    .macos
                    .keychain
                    .to_string_lossy()
                    .contains(['\0', '\n', '\r']),
            "invalid Keychain path"
        );
        Ok(self)
    }
}

fn read(path: &Path) -> Result<SigningPolicy> {
    let info = private_file(path)?;
    ensure!(
        info.len() <= 65536,
        "signing selection exceeds its size limit"
    );
    serde_json::from_slice::<SigningPolicy>(&fs::read(path)?)?.normalize()
}

fn private_file(path: &Path) -> Result<fs::Metadata> {
    let info = fs::symlink_metadata(path)
        .with_context(|| format!("signing selection unavailable: {}", path.display()))?;
    ensure!(
        info.is_file()
            && info.nlink() == 1
            && info.uid() == crate::host_setup::uid()
            && info.mode() & 0o777 == 0o600,
        "signing selection must be an owned private regular file"
    );
    Ok(info)
}

pub(crate) fn current() -> Result<SigningPolicy> {
    read(&host_policy(&home()?))
}

pub(crate) fn selected(paths: &Paths) -> Result<SigningPolicy> {
    let path = paths.root.join("signing.json");
    match fs::symlink_metadata(&path) {
        Ok(_) => read(&path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => current(),
        Err(error) => Err(error.into()),
    }
}

// These commands contain only public certificate selection. Never print a key
// export, password, or an entire command line in signing diagnostics.
fn tool(program: &str, args: &[String]) -> Result<String> {
    checked_tool(&mut run_tool, program, args, Duration::from_secs(30))
}

struct ToolOutput {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

type ToolRunner<'a> = dyn FnMut(&str, &[String], Duration) -> Result<ToolOutput> + 'a;

fn run_tool(program: &str, args: &[String], timeout: Duration) -> Result<ToolOutput> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("signing tool unavailable")?;
    let mut stdout = child.stdout.take().context("signing stdout unavailable")?;
    let mut stderr = child.stderr.take().context("signing stderr unavailable")?;
    let output = std::thread::spawn(move || -> std::io::Result<String> {
        let mut text = String::new();
        stdout.read_to_string(&mut text)?;
        Ok(text)
    });
    let diagnostic = std::thread::spawn(move || -> std::io::Result<String> {
        let mut text = String::new();
        stderr.read_to_string(&mut text)?;
        Ok(text)
    });
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() >= timeout {
            child.kill()?;
            child.wait()?;
            bail!("signing key access timed out; unlock and authorize the selected Keychain");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let output = output
        .join()
        .map_err(|_| anyhow::anyhow!("signing stdout reader failed"))??;
    let diagnostic = diagnostic
        .join()
        .map_err(|_| anyhow::anyhow!("signing stderr reader failed"))??;
    Ok(ToolOutput {
        code: status.code(),
        stdout: output,
        stderr: diagnostic,
    })
}

fn checked_tool(
    runner: &mut ToolRunner<'_>,
    program: &str,
    args: &[String],
    timeout: Duration,
) -> Result<String> {
    let output = runner(program, args, timeout)?;
    ensure!(
        output.code == Some(0),
        "signing tool failed: {}",
        output
            .stderr
            .chars()
            .rev()
            .take(1500)
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>()
    );
    Ok(output.stdout)
}

pub(crate) fn preflight(policy: &SigningPolicy) -> Result<()> {
    ensure!(cfg!(target_os = "macos"), "native signing requires macOS");
    preflight_with(policy, &mut run_tool)
}

fn preflight_with(policy: &SigningPolicy, runner: &mut ToolRunner<'_>) -> Result<()> {
    let policy = policy.clone().normalize()?;
    let keychain = &policy.macos.keychain;
    let info = fs::symlink_metadata(keychain).context("selected Keychain is unavailable")?;
    ensure!(
        info.is_file() && info.uid() == crate::host_setup::uid(),
        "Keychain must be an owned regular file"
    );
    let output = checked_tool(
        runner,
        "/usr/bin/security",
        &[
            "find-identity".into(),
            "-v".into(),
            "-p".into(),
            "codesigning".into(),
            keychain.display().to_string(),
        ],
        Duration::from_secs(30),
    )?;
    ensure!(
        output
            .split(|c: char| !c.is_ascii_hexdigit())
            .any(|value| value.eq_ignore_ascii_case(&policy.macos.certificate_sha1)),
        "selected certificate or private key is missing, expired, or unavailable"
    );
    checked_tool(
        runner,
        "/usr/bin/codesign",
        &[
            "--dryrun".into(),
            "--detached".into(),
            "/dev/null".into(),
            "--force".into(),
            "--sign".into(),
            policy.macos.certificate_sha1.clone(),
            "--keychain".into(),
            keychain.display().to_string(),
            "--identifier".into(),
            format!("{}.preflight", policy.macos.identifier_namespace),
            "--timestamp=none".into(),
            "/usr/bin/true".into(),
        ],
        Duration::from_secs(30),
    )?;
    Ok(())
}

pub(crate) fn identifier(
    policy: &SigningPolicy,
    product: &str,
    executable: &str,
) -> Result<String> {
    ensure!(
        name(product) && name(executable),
        "invalid permanent executable identity"
    );
    Ok(format!(
        "{}.{}.{}",
        policy.macos.identifier_namespace, product, executable
    ))
}

pub(crate) fn sign(
    path: &Path,
    policy: &SigningPolicy,
    product: &str,
    executable: &str,
) -> Result<()> {
    let identity = identifier(policy, product, executable)?;
    let requirement = format!(
        "=designated => identifier \"{identity}\" and certificate leaf = H\"{}\"",
        policy.macos.certificate_sha1
    );
    tool(
        "/usr/bin/codesign",
        &[
            "--force".into(),
            "--sign".into(),
            policy.macos.certificate_sha1.clone(),
            "--keychain".into(),
            policy.macos.keychain.display().to_string(),
            "--identifier".into(),
            identity,
            "--requirements".into(),
            requirement,
            "--timestamp=none".into(),
            path.display().to_string(),
        ],
    )?;
    Ok(())
}

pub(crate) fn status(paths: &Paths) -> Result<Value> {
    let policy = selected(paths)?;
    preflight(&policy)?;
    Ok(
        json!({"policy":policy,"ready":true,"selection":if paths.root.join("signing.json").exists(){"telete"}else{"host"}}),
    )
}

pub(crate) fn configure(
    paths: &Paths,
    fingerprint: &str,
    keychain: &Path,
    namespace: &str,
) -> Result<SigningPolicy> {
    ensure!(cfg!(target_os = "macos"), "native signing requires macOS");
    let policy = policy(fingerprint, keychain, namespace)?;
    let _guards = configuration_guard(paths)?;
    let destination = paths.root.join("signing.json");
    if present(&destination)? {
        read(&destination)?;
    }
    configure_with(&destination, policy, &mut run_tool)
}

pub(crate) fn configure_host(
    paths: &Paths,
    fingerprint: &str,
    keychain: &Path,
    namespace: &str,
) -> Result<SigningPolicy> {
    ensure!(cfg!(target_os = "macos"), "native signing requires macOS");
    let policy = policy(fingerprint, keychain, namespace)?;
    let _setup = crate::host_setup::setup_lock()?;
    let _guards = configuration_guard(paths)?;
    let _manual_deployment = manual_deployment_guard(paths)?;
    configure_with(&host_policy(&home()?), policy, &mut run_tool)
}

fn policy(fingerprint: &str, keychain: &Path, namespace: &str) -> Result<SigningPolicy> {
    SigningPolicy {
        schema: 1,
        macos: MacSigning {
            profile: "local".into(),
            certificate_sha1: fingerprint.into(),
            keychain: keychain.into(),
            identifier_namespace: namespace.into(),
        },
    }
    .normalize()
}

fn configuration_guard(paths: &Paths) -> Result<Vec<paths::FileLock>> {
    let guards = vec![
        paths::lock(&paths.root.join("admission.lock"), false)?,
        paths::lock(&paths.root.join("worker.lock"), false)?,
        paths::lock(&paths.root.join("deployments/deployment.lock"), false)?,
    ];
    crate::manager::require_quiescent(paths)?;
    if present(&paths.root.join("queue.sqlite3"))? {
        let config = crate::store::Store::open(&paths.root, false)?.config()?;
        ensure!(
            crate::git::common(paths, &config.repository)? == config.common_git_dir,
            "configured Git repository identity changed"
        );
        for name in [
            "cell-release-publication.lock",
            "cell-release-publication.lock.d",
        ] {
            ensure!(
                !present(&config.common_git_dir.join(name))?,
                "settle release publication before changing signing configuration"
            );
        }
    }
    ensure!(
        !present(&paths.root.join("deployments/active.json"))?,
        "settle interrupted Telete deployment before changing signing selection"
    );
    Ok(guards)
}

fn configure_with(
    path: &Path,
    policy: SigningPolicy,
    runner: &mut ToolRunner<'_>,
) -> Result<SigningPolicy> {
    if present(path)? {
        private_file(path)?;
    }
    preflight_with(&policy, runner)?;
    paths::atomic_json(path, &policy)?;
    Ok(policy)
}

fn host_policy(home: &Path) -> PathBuf {
    home.join("Library/Application Support/Cell/signing.json")
}

fn present(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn manual_deployment_guard(paths: &Paths) -> Result<paths::FileLock> {
    let root = paths.workspace.join("deployments");
    let guard = paths::lock(&root.join("deployment.lock"), false)?;
    ensure!(
        !present(&root.join("active"))?,
        "settle retained manual deployment recovery before changing shared signing selection"
    );
    Ok(guard)
}

pub(crate) fn create_local(paths: &Paths) -> Result<Value> {
    ensure!(cfg!(target_os = "macos"), "native signing requires macOS");
    let _setup = crate::host_setup::setup_lock()?;
    let _guards = configuration_guard(paths)?;
    let _manual_deployment = manual_deployment_guard(paths)?;
    let policy = create_local_with(paths, &home()?, &mut run_tool)?;
    Ok(json!({"policy":policy}))
}

#[allow(clippy::too_many_lines)] // Keep the initial creation sequence in operation order.
fn create_local_with(
    paths: &Paths,
    home: &Path,
    runner: &mut ToolRunner<'_>,
) -> Result<SigningPolicy> {
    let destination = host_policy(home);
    ensure!(
        !present(&destination)? && !present(&paths.root.join("signing.json"))?,
        "signing is already configured; select an existing identity explicitly with signing configure"
    );
    let keychain = home.join("Library/Keychains/login.keychain-db");
    let info = fs::symlink_metadata(&keychain).context("login Keychain is unavailable")?;
    ensure!(
        info.is_file() && info.uid() == crate::host_setup::uid(),
        "login Keychain must be an owned regular file"
    );
    let probe = runner(
        "/usr/bin/security",
        &[
            "find-certificate".into(),
            "-c".into(),
            "Cell Local Signing".into(),
            keychain.display().to_string(),
        ],
        Duration::from_secs(30),
    )
    .context("cannot establish whether a Cell Local Signing certificate already exists")?;
    ensure!(
        probe.code != Some(0),
        "Cell Local Signing already exists; use signing configure --host after its key is usable"
    );
    ensure!(
        probe.code == Some(44),
        "cannot establish whether a Cell Local Signing certificate already exists"
    );
    let parent = destination
        .parent()
        .context("signing policy has no parent")?;
    paths::ensure_private(parent)?;
    let stage = tempfile::Builder::new()
        .prefix(".signing-setup-")
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir_in(paths.root.join("tmp"))?;
    let key = stage.path().join("key.pem");
    let certificate = stage.path().join("certificate.pem");
    for path in [&key, &certificate] {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
    }
    checked_tool(
        runner,
        "/usr/bin/openssl",
        &[
            "req".into(),
            "-new".into(),
            "-newkey".into(),
            "rsa:3072".into(),
            "-nodes".into(),
            "-x509".into(),
            "-sha256".into(),
            "-days".into(),
            "3650".into(),
            "-subj".into(),
            "/CN=Cell Local Signing".into(),
            "-keyout".into(),
            key.display().to_string(),
            "-out".into(),
            certificate.display().to_string(),
            "-addext".into(),
            "basicConstraints=critical,CA:FALSE".into(),
            "-addext".into(),
            "keyUsage=critical,digitalSignature".into(),
            "-addext".into(),
            "extendedKeyUsage=critical,codeSigning".into(),
        ],
        Duration::from_secs(30),
    )?;
    let fingerprint = checked_tool(
        runner,
        "/usr/bin/openssl",
        &[
            "x509".into(),
            "-in".into(),
            certificate.display().to_string(),
            "-noout".into(),
            "-fingerprint".into(),
            "-sha1".into(),
        ],
        Duration::from_secs(30),
    )?;
    let fingerprint = fingerprint
        .trim()
        .split_once('=')
        .context("invalid certificate fingerprint output")?
        .1
        .replace(':', "");
    let policy = policy(&fingerprint, &keychain, "local.cell")?;
    let result = import_and_select(&destination, &key, &certificate, &policy, runner);
    result.with_context(|| format!(
        "initial signing setup may have imported certificate {}; do not create or delete a replacement; make its key usable and run signing configure --host --certificate-sha1 {} --keychain {} --identifier-namespace local.cell",
        policy.macos.certificate_sha1,
        policy.macos.certificate_sha1,
        keychain.display()
    ))?;
    Ok(policy)
}

fn import_and_select(
    destination: &Path,
    key: &Path,
    certificate: &Path,
    policy: &SigningPolicy,
    runner: &mut ToolRunner<'_>,
) -> Result<()> {
    let keychain = policy.macos.keychain.display().to_string();
    checked_tool(
        runner,
        "/usr/bin/security",
        &[
            "import".into(),
            certificate.display().to_string(),
            "-k".into(),
            keychain.clone(),
        ],
        Duration::from_secs(30),
    )
    .context("certificate import did not complete; its Keychain effect may be incomplete")?;
    checked_tool(
        runner,
        "/usr/bin/security",
        &[
            "import".into(),
            key.display().to_string(),
            "-k".into(),
            keychain.clone(),
            "-T".into(),
            "/usr/bin/codesign".into(),
        ],
        Duration::from_secs(30),
    )
    .context("private-key import did not complete; Keychain may contain only the certificate, which configure alone cannot make usable")?;
    checked_tool(
        runner,
        "/usr/bin/security",
        &[
            "add-trusted-cert".into(),
            "-r".into(),
            "trustRoot".into(),
            "-p".into(),
            "codeSign".into(),
            "-k".into(),
            keychain,
            certificate.display().to_string(),
        ],
        Duration::from_secs(60),
    )
    .context("certificate code-signing trust did not complete")?;
    preflight_with(policy, runner).context("imported signing identity failed preflight")?;
    paths::atomic_json(destination, policy).context("shared signing policy publication failed")
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    struct SetupFixture {
        _directory: tempfile::TempDir,
        paths: Paths,
        home: PathBuf,
    }

    impl SetupFixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().canonicalize().unwrap();
            let paths = Paths::for_test(root.join("telete")).unwrap();
            let home = root.join("home");
            fs::create_dir_all(home.join("Library/Keychains")).unwrap();
            fs::write(home.join("Library/Keychains/login.keychain-db"), []).unwrap();
            Self {
                _directory: directory,
                paths,
                home,
            }
        }

        fn assert_no_staging(&self) {
            let parent = self.paths.root.join("tmp");
            if parent.exists() {
                assert!(fs::read_dir(parent).unwrap().all(|entry| {
                    !entry
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .starts_with(".signing-setup-")
                }));
            }
        }
    }

    struct FakeTools {
        destination: PathBuf,
        calls: Vec<(String, Vec<String>, Duration)>,
        fail_at: Option<usize>,
        probe_code: Option<i32>,
    }

    impl FakeTools {
        fn new(home: &Path) -> Self {
            Self {
                destination: host_policy(home),
                calls: Vec::new(),
                fail_at: None,
                probe_code: Some(44),
            }
        }

        fn run(&mut self, program: &str, args: &[String], timeout: Duration) -> Result<ToolOutput> {
            assert!(
                !self.destination.exists(),
                "policy published before preflight completed"
            );
            let index = self.calls.len();
            self.calls.push((program.into(), args.to_vec(), timeout));
            if self.fail_at == Some(index) {
                return Ok(ToolOutput {
                    code: Some(1),
                    stdout: String::new(),
                    stderr: "injected tool failure".into(),
                });
            }
            let mut output = ToolOutput {
                code: Some(0),
                stdout: String::new(),
                stderr: String::new(),
            };
            match args[0].as_str() {
                "find-certificate" => output.code = self.probe_code,
                "req" => {
                    for flag in ["-keyout", "-out"] {
                        let position = args.iter().position(|arg| arg == flag).unwrap();
                        let path = Path::new(&args[position + 1]);
                        assert_eq!(fs::metadata(path)?.mode() & 0o777, 0o600);
                        assert_eq!(fs::metadata(path.parent().unwrap())?.mode() & 0o777, 0o700);
                    }
                }
                "x509" => {
                    output.stdout = format!("sha1 Fingerprint={}\n", vec!["AA"; 20].join(":"));
                }
                "find-identity" => {
                    output.stdout = format!("1) {} \"Cell Local Signing\"", "AA".repeat(20));
                }
                _ => {}
            }
            Ok(output)
        }
    }
    fn policy() -> SigningPolicy {
        SigningPolicy {
            schema: 1,
            macos: MacSigning {
                profile: "local".into(),
                certificate_sha1: "AA".repeat(20),
                keychain: "/private/login.keychain-db".into(),
                identifier_namespace: "local.cell".into(),
            },
        }
    }
    #[test]
    fn policy_normalizes_exact_identity_without_changing_namespace() {
        let policy = policy().normalize().unwrap();
        assert_eq!(policy.macos.certificate_sha1, "aa".repeat(20));
        assert_eq!(
            identifier(&policy, "krisis", "krisis-install").unwrap(),
            "local.cell.krisis.krisis-install"
        );
    }
    #[test]
    fn configuration_rejects_unsupported_profiles_and_nonliteral_identities() {
        let mut invalid = policy();
        invalid.macos.profile = "ad-hoc".into();
        assert!(invalid.normalize().is_err());
        let mut invalid = policy();
        invalid.macos.identifier_namespace = "local.cell..x".into();
        assert!(invalid.normalize().is_err());
        let mut invalid = policy();
        invalid.macos.certificate_sha1 = "Cell Local Signing".into();
        assert!(invalid.normalize().is_err());
        let mut invalid = policy();
        invalid.macos.keychain = "relative".into();
        assert!(invalid.normalize().is_err());
    }

    #[test]
    fn initial_creation_publishes_shared_policy_after_restricted_import_and_preflight() {
        let fixture = SetupFixture::new();
        let mut tools = FakeTools::new(&fixture.home);
        let policy = create_local_with(
            &fixture.paths,
            &fixture.home,
            &mut |program, args, timeout| tools.run(program, args, timeout),
        )
        .unwrap();
        assert_eq!(read(&host_policy(&fixture.home)).unwrap(), policy);
        assert!(!fixture.paths.root.join("signing.json").exists());
        assert_eq!(policy.macos.certificate_sha1, "aa".repeat(20));
        assert_eq!(policy.macos.identifier_namespace, "local.cell");
        assert_eq!(tools.calls.len(), 8);
        let generation = &tools.calls[1].1;
        let keyout = generation
            .iter()
            .position(|argument| argument == "-keyout")
            .unwrap();
        assert!(Path::new(&generation[keyout + 1]).starts_with(fixture.paths.root.join("tmp")));
        for argument in [
            "rsa:3072",
            "-sha256",
            "3650",
            "/CN=Cell Local Signing",
            "basicConstraints=critical,CA:FALSE",
            "keyUsage=critical,digitalSignature",
            "extendedKeyUsage=critical,codeSigning",
        ] {
            assert!(generation.iter().any(|value| value == argument));
        }
        let import = &tools.calls[4].1;
        assert_eq!(&import[4..], &["-T", "/usr/bin/codesign"]);
        assert!(
            tools
                .calls
                .iter()
                .all(|(_, args, _)| !args.iter().any(|value| value == "-A"))
        );
        assert_eq!(tools.calls[5].2, Duration::from_secs(60));
        assert_eq!(
            &tools.calls[5].1[0..5],
            &["add-trusted-cert", "-r", "trustRoot", "-p", "codeSign"]
        );
        assert_eq!(tools.calls[7].0, "/usr/bin/codesign");
        assert_eq!(tools.calls[7].1[0], "--dryrun");
        fixture.assert_no_staging();
    }

    #[test]
    fn initial_creation_refuses_existing_or_symbolic_selections_before_keychain_calls() {
        for host in [false, true] {
            for symbolic in [false, true] {
                let fixture = SetupFixture::new();
                let destination = if host {
                    host_policy(&fixture.home)
                } else {
                    fixture.paths.root.join("signing.json")
                };
                paths::ensure_private(destination.parent().unwrap()).unwrap();
                if symbolic {
                    symlink("absent-policy", &destination).unwrap();
                } else {
                    fs::write(&destination, "invalid existing policy").unwrap();
                }
                let mut tools = FakeTools::new(&fixture.home);
                let error = create_local_with(
                    &fixture.paths,
                    &fixture.home,
                    &mut |program, args, timeout| tools.run(program, args, timeout),
                )
                .unwrap_err();
                assert!(error.to_string().contains("already configured"));
                assert!(tools.calls.is_empty());
            }
        }
    }

    #[test]
    fn only_item_not_found_allows_initial_generation() {
        for code in [Some(0), Some(1), Some(45), None] {
            let fixture = SetupFixture::new();
            let mut tools = FakeTools::new(&fixture.home);
            tools.probe_code = code;
            assert!(
                create_local_with(
                    &fixture.paths,
                    &fixture.home,
                    &mut |program, args, timeout| tools.run(program, args, timeout)
                )
                .is_err()
            );
            assert_eq!(tools.calls.len(), 1);
            assert!(!host_policy(&fixture.home).exists());
            fixture.assert_no_staging();
        }
    }

    #[test]
    fn creation_failures_preserve_absent_policy_and_never_delete_imported_identity() {
        for fail_at in 0..8 {
            let fixture = SetupFixture::new();
            let mut tools = FakeTools::new(&fixture.home);
            tools.fail_at = Some(fail_at);
            let error = create_local_with(
                &fixture.paths,
                &fixture.home,
                &mut |program, args, timeout| tools.run(program, args, timeout),
            )
            .unwrap_err();
            assert!(!host_policy(&fixture.home).exists());
            assert_eq!(tools.calls.len(), fail_at + 1);
            assert!(
                !tools
                    .calls
                    .iter()
                    .any(|(_, args, _)| args[0].starts_with("delete-"))
            );
            if fail_at >= 3 {
                assert!(error.to_string().contains(&"aa".repeat(20)));
                assert!(error.to_string().contains("configure --host"));
                let stage = match fail_at {
                    3 => "certificate import did not complete",
                    4 => "private-key import did not complete",
                    5 => "certificate code-signing trust did not complete",
                    _ => "imported signing identity failed preflight",
                };
                assert!(format!("{error:#}").contains(stage));
            }
            fixture.assert_no_staging();
        }
    }

    #[test]
    fn signing_maintenance_requires_exclusive_worker_and_settled_deployment() {
        let fixture = SetupFixture::new();
        let worker = paths::lock(&fixture.paths.root.join("worker.lock"), false).unwrap();
        assert!(configuration_guard(&fixture.paths).is_err());
        drop(worker);
        let active = fixture.paths.root.join("deployments/active.json");
        symlink("missing-record", &active).unwrap();
        assert!(configuration_guard(&fixture.paths).is_err());
        fs::remove_file(active).unwrap();
        assert!(configuration_guard(&fixture.paths).is_ok());
    }

    #[test]
    fn signing_maintenance_requires_existing_queue_to_be_paused_and_initialized() {
        let fixture = SetupFixture::new();
        let store = crate::store::Store::open(&fixture.paths.root, true).unwrap();
        store.set("paused", &false).unwrap();
        let error = configuration_guard(&fixture.paths).err().unwrap();
        assert!(error.to_string().contains("pause Telete"));
        store.set("paused", &true).unwrap();
        let error = configuration_guard(&fixture.paths).err().unwrap();
        assert!(error.to_string().contains("initialization is incomplete"));
    }

    #[test]
    fn interrupted_import_reports_exact_identity_without_selecting_or_deleting_it() {
        let fixture = SetupFixture::new();
        let mut tools = FakeTools::new(&fixture.home);
        let error = create_local_with(
            &fixture.paths,
            &fixture.home,
            &mut |program, args, timeout| {
                if args[0] == "import" {
                    bail!("key access timed out");
                }
                tools.run(program, args, timeout)
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains(&"aa".repeat(20)));
        assert!(error.to_string().contains("configure --host"));
        assert!(!host_policy(&fixture.home).exists());
        fixture.assert_no_staging();
    }

    #[test]
    fn explicit_configuration_preserves_previous_policy_when_preflight_fails() {
        let fixture = SetupFixture::new();
        let destination = host_policy(&fixture.home);
        let previous = policy().normalize().unwrap();
        paths::atomic_json(&destination, &previous).unwrap();
        let replacement = super::policy(
            &"bb".repeat(20),
            &fixture.home.join("Library/Keychains/login.keychain-db"),
            "local.cell",
        )
        .unwrap();
        let error = configure_with(&destination, replacement, &mut |_, _, _| {
            bail!("key inaccessible")
        });
        assert!(error.is_err());
        assert_eq!(read(&destination).unwrap(), previous);
    }

    #[test]
    fn explicit_host_configuration_can_replace_corrupted_private_policy_after_preflight() {
        let fixture = SetupFixture::new();
        let destination = host_policy(&fixture.home);
        paths::atomic_json(&destination, &policy()).unwrap();
        let previous = b"interrupted or corrupted JSON";
        fs::write(&destination, previous).unwrap();
        let replacement = super::policy(
            &"bb".repeat(20),
            &fixture.home.join("Library/Keychains/login.keychain-db"),
            "local.cell",
        )
        .unwrap();
        let mut calls = 0;
        let result = configure_with(&destination, replacement.clone(), &mut |_, args, _| {
            assert_eq!(fs::read(&destination).unwrap(), previous);
            calls += 1;
            Ok(ToolOutput {
                code: Some(0),
                stdout: if args[0] == "find-identity" {
                    "bb".repeat(20)
                } else {
                    String::new()
                },
                stderr: String::new(),
            })
        })
        .unwrap();
        assert_eq!(calls, 2);
        assert_eq!(result, replacement);
        assert_eq!(read(&destination).unwrap(), replacement);
    }

    #[test]
    fn failed_host_configuration_preserves_corrupted_private_policy_bytes() {
        let fixture = SetupFixture::new();
        let destination = host_policy(&fixture.home);
        paths::atomic_json(&destination, &policy()).unwrap();
        let previous = b"interrupted or corrupted JSON";
        fs::write(&destination, previous).unwrap();
        let replacement = super::policy(
            &"bb".repeat(20),
            &fixture.home.join("Library/Keychains/login.keychain-db"),
            "local.cell",
        )
        .unwrap();
        let mut calls = 0;
        let result = configure_with(&destination, replacement, &mut |_, _, _| {
            calls += 1;
            bail!("key inaccessible")
        });
        assert!(result.is_err());
        assert_eq!(calls, 1);
        assert_eq!(fs::read(destination).unwrap(), previous);
    }

    #[test]
    fn publication_failure_identifies_its_stage_and_preserves_imported_identity() {
        let fixture = SetupFixture::new();
        let destination = host_policy(&fixture.home);
        let mut tools = FakeTools::new(&fixture.home);
        let error = create_local_with(
            &fixture.paths,
            &fixture.home,
            &mut |program, args, timeout| {
                let result = tools.run(program, args, timeout)?;
                if program == "/usr/bin/codesign" {
                    fs::create_dir(&destination)?;
                }
                Ok(result)
            },
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("shared signing policy publication failed"));
        assert!(error.to_string().contains(&"aa".repeat(20)));
        assert!(destination.is_dir());
        assert_eq!(tools.calls.len(), 8);
        fixture.assert_no_staging();
    }
}
