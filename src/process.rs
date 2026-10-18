use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::PathBuf,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use crate::paths::{self, Paths};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CommandSpec {
    pub(crate) program: PathBuf,
    pub(crate) args: Vec<String>,
    pub(crate) cwd: PathBuf,
    pub(crate) env: BTreeMap<String, String>,
    pub(crate) timeout_seconds: Option<u64>,
    pub(crate) stdin: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) context: Option<CommandContext>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CommandContext {
    pub(crate) path: PathBuf,
    pub(crate) value: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProcessResult {
    pub(crate) exit_code: Option<i32>,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
    pub(crate) timed_out: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) output_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) signal: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) start_error: Option<String>,
}
impl ProcessResult {
    pub(crate) fn success(&self) -> bool {
        self.exited_with(&[0])
    }

    pub(crate) fn exited_with(&self, codes: &[i32]) -> bool {
        self.exit_code.is_some_and(|code| codes.contains(&code))
            && !self.timed_out
            && self.output_error.is_none()
            && self.signal.is_none()
            && self.start_error.is_none()
    }

    pub(crate) fn diagnostics(&self) -> String {
        let mut text = format!("{}{}", self.stdout, self.stderr);
        for error in [&self.start_error, &self.output_error]
            .into_iter()
            .flatten()
        {
            text.push('\n');
            text.push_str(error);
        }
        if let Some(signal) = self.signal {
            use std::fmt::Write as _;
            let _ = write!(text, "\nprocess terminated by signal {signal}");
        }
        text
    }
}

const OUTPUT_LIMIT: usize = 16 * 1024 * 1024;

fn read_stream(stream: impl Read) -> (String, Option<String>) {
    let mut bytes = Vec::new();
    let mut stream = stream;
    let mut chunk = [0u8; 8192];
    let mut overflow = false;
    let mut errors = Vec::new();
    loop {
        let count = match stream.read(&mut chunk) {
            Ok(count) => count,
            Err(error) => {
                errors.push(format!("read process output: {error}"));
                break;
            }
        };
        if count == 0 {
            break;
        }
        let remaining = OUTPUT_LIMIT.saturating_sub(bytes.len());
        bytes.extend_from_slice(&chunk[..count.min(remaining)]);
        overflow |= count > remaining;
    }
    if overflow {
        errors.push("process output exceeded 16 MiB".into());
    }
    (
        String::from_utf8_lossy(&bytes).into_owned(),
        (!errors.is_empty()).then(|| errors.join("; ")),
    )
}

fn prepare_context(paths: &Paths, spec: &CommandSpec) -> Result<()> {
    let Some(context) = &spec.context else {
        return Ok(());
    };
    ensure!(
        spec.program.is_absolute()
            && spec.cwd.is_absolute()
            && context.path.is_absolute()
            && context.path.starts_with(&paths.root)
            && context.path != paths.root,
        "context commands require absolute paths and context in Telete state"
    );
    let context_path = context
        .path
        .to_str()
        .context("context path must be UTF-8")?;
    ensure!(
        spec.env.get("TELETE_CONTEXT").map(String::as_str) == Some(context_path),
        "TELETE_CONTEXT must match the retained context path"
    );
    let parent = context.path.parent().context("context has no parent")?;
    paths::ensure_private(parent)?;
    let mut staged = tempfile::NamedTempFile::new_in(parent)?;
    staged
        .as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    serde_json::to_writer_pretty(&mut staged, &context.value)?;
    staged.write_all(b"\n")?;
    staged.as_file().sync_all()?;
    match staged.persist_noclobber(&context.path) {
        Ok(_) => File::open(parent)?.sync_all()?,
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.error.into()),
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&context.path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.uid() == rustix::process::getuid().as_raw()
            && metadata.mode() & 0o777 == 0o600,
        "context must be a private user-owned regular file"
    );
    let saved: serde_json::Value = serde_json::from_reader(file)?;
    ensure!(saved == context.value, "retained command context changed");
    Ok(())
}

pub(crate) fn run(paths: &Paths, spec: &CommandSpec) -> Result<ProcessResult> {
    run_without(paths, spec, &[])
}

#[allow(clippy::too_many_lines)]
pub(crate) fn run_without(
    paths: &Paths,
    spec: &CommandSpec,
    removed: &[std::ffi::OsString],
) -> Result<ProcessResult> {
    ensure!(
        spec.timeout_seconds.is_none_or(|seconds| seconds > 0) && spec.cwd.is_absolute(),
        "invalid process timeout or working directory"
    );
    prepare_context(paths, spec)?;
    let mut command = Command::new(&spec.program);
    for name in removed {
        command.env_remove(name);
    }
    command
        .args(&spec.args)
        .current_dir(&spec.cwd)
        .envs(&spec.env)
        .stdin(if spec.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return Ok(ProcessResult {
                exit_code: None,
                stdout: String::new(),
                stderr: String::new(),
                timed_out: false,
                output_error: None,
                signal: None,
                start_error: Some(format!("start {}: {error}", spec.program.display())),
            });
        }
    };
    let out = child.stdout.take().context("stdout missing")?;
    let err = child.stderr.take().context("stderr missing")?;
    let stdout = thread::spawn(move || read_stream(out));
    let stderr = thread::spawn(move || read_stream(err));
    let input = if let Some(value) = spec.stdin.clone() {
        let mut writer = child.stdin.take().context("stdin missing")?;
        Some(thread::spawn(move || writer.write_all(value.as_bytes())))
    } else {
        None
    };
    let started = Instant::now();
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if spec
            .timeout_seconds
            .is_some_and(|seconds| started.elapsed() >= Duration::from_secs(seconds))
        {
            timed_out = true;
            let pid = format!("-{}", child.id());
            let _ = Command::new("/bin/kill")
                .args(["-TERM", "--", &pid])
                .status();
            thread::sleep(Duration::from_millis(150));
            let _ = Command::new("/bin/kill")
                .args(["-KILL", "--", &pid])
                .status();
            let _ = child.kill();
            break child.wait()?;
        }
        thread::sleep(Duration::from_millis(25));
    };
    // Close descendants' pipes as well: a direct process exit is not proof that
    // a surviving child stopped. Gate commands may not leave background work.
    let pid = format!("-{}", child.id());
    let _ = Command::new("/bin/kill")
        .args(["-KILL", "--", &pid])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    if let Some(input) = input {
        let _ = input.join();
    }
    let (stdout, stdout_error) = stdout
        .join()
        .unwrap_or_else(|_| (String::new(), Some("reader panicked".into())));
    let (stderr, stderr_error) = stderr
        .join()
        .unwrap_or_else(|_| (String::new(), Some("reader panicked".into())));
    let errors: Vec<_> = [("stdout", stdout_error), ("stderr", stderr_error)]
        .into_iter()
        .filter_map(|(name, error)| error.map(|error| format!("{name}: {error}")))
        .collect();
    Ok(ProcessResult {
        exit_code: status.code(),
        stdout,
        stderr,
        timed_out,
        output_error: (!errors.is_empty()).then(|| errors.join("; ")),
        signal: status.signal(),
        start_error: None,
    })
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Completion {
    schema: u32,
    request: CommandSpec,
    result: ProcessResult,
}

pub(crate) fn observe(directory: &std::path::Path) -> Result<Option<ProcessResult>> {
    let result = directory.join("result.json");
    if !result.exists() {
        return Ok(None);
    }
    let request: CommandSpec =
        serde_json::from_slice(&std::fs::read(directory.join("request.json"))?)?;
    let completion: Completion = serde_json::from_slice(&std::fs::read(result)?)?;
    ensure!(
        completion.schema == 1
            && serde_json::to_value(request)? == serde_json::to_value(completion.request)?,
        "uncorrelated process completion"
    );
    Ok(Some(completion.result))
}

pub(crate) fn supervise(
    paths: &Paths,
    spec: &CommandSpec,
    directory: &std::path::Path,
) -> Result<ProcessResult> {
    ensure!(
        directory.starts_with(&paths.root),
        "process records must stay in Telete state"
    );
    paths::ensure_private(directory)?;
    let request_path = directory.join("request.json");
    let _admission = paths::lock(&directory.join("admission.lock"), true)?;
    if request_path.exists() {
        let saved: CommandSpec = serde_json::from_slice(&std::fs::read(&request_path)?)?;
        ensure!(
            serde_json::to_value(saved)? == serde_json::to_value(spec)?,
            "process identity reused with another request"
        );
    } else {
        paths::atomic_json(&request_path, spec)?;
    }
    if let Some(result) = observe(directory)? {
        return Ok(result);
    }
    ensure!(
        !directory.join("intent.json").exists(),
        "process has admitted intent without completion; do not repeat it"
    );
    paths::atomic_json(
        &directory.join("intent.json"),
        &serde_json::json!({"schema":1,"owner_pid":std::process::id()}),
    )?;
    #[cfg(test)]
    {
        // Unit tests use the test harness rather than an installed CLI process.
        let result = run(paths, spec)?;
        paths::atomic_json(
            &directory.join("result.json"),
            &Completion {
                schema: 1,
                request: spec.clone(),
                result: result.clone(),
            },
        )?;
        Ok(result)
    }
    #[cfg(not(test))]
    {
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(directory.join("supervisor.log"))?;
        log.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        let mut child = Command::new(std::env::current_exe()?)
            .args(["internal-execute", "--state"])
            .arg(&paths.root)
            .arg("--directory")
            .arg(directory)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log))
            .process_group(0)
            .spawn()?;
        paths::atomic_json(
            &directory.join("supervisor.json"),
            &serde_json::json!({"schema":1,"pid":child.id()}),
        )?;
        let status = child.wait()?;
        observe(directory)?
            .with_context(|| format!("supervisor stopped without terminal evidence: {status}"))
    }
}

pub(crate) fn execute_request(paths: &Paths, directory: &std::path::Path) -> Result<()> {
    ensure!(
        directory.starts_with(&paths.root),
        "foreign supervisor directory"
    );
    paths::ensure_private(directory)?;
    ensure!(
        directory.join("intent.json").is_file(),
        "supervisor request was not admitted"
    );
    let _owner = paths::lock(&directory.join("runner.lock"), false)?;
    ensure!(
        !directory.join("started.json").exists(),
        "supervisor execution cannot be repeated"
    );
    let request: CommandSpec =
        serde_json::from_slice(&std::fs::read(directory.join("request.json"))?)?;
    paths::atomic_json(
        &directory.join("started.json"),
        &serde_json::json!({"schema":1,"pid":std::process::id()}),
    )?;
    let result = run(paths, &request)?;
    paths::atomic_json(
        &directory.join("result.json"),
        &Completion {
            schema: 1,
            request,
            result,
        },
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, Paths, CommandSpec) {
        let directory = tempfile::tempdir().unwrap();
        let paths = Paths::for_test(directory.path().join("work")).unwrap();
        let request = CommandSpec {
            program: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                "printf output; printf diagnostic >&2; exit 7".into(),
            ],
            cwd: paths.root.clone(),
            env: BTreeMap::new(),
            timeout_seconds: Some(5),
            stdin: None,
            context: None,
        };
        (directory, paths, request)
    }

    #[test]
    fn retained_completion_requires_the_same_frozen_command() {
        let (_temporary, paths, request) = fixture();
        let directory = paths.root.join("execution");
        let result = supervise(&paths, &request, &directory).unwrap();
        assert_eq!(result.exit_code, Some(7));
        assert_eq!(result.stdout, "output");
        assert_eq!(result.stderr, "diagnostic");
        assert!(!result.success());
        assert_eq!(
            supervise(&paths, &request, &directory).unwrap().exit_code,
            Some(7)
        );
        let mut changed = request.clone();
        changed.args.clear();
        assert!(supervise(&paths, &changed, &directory).is_err());
        paths::atomic_json(&directory.join("request.json"), &changed).unwrap();
        assert!(observe(&directory).is_err());
    }

    #[test]
    fn admitted_execution_writes_a_receipt_and_cannot_run_again() {
        let (_temporary, paths, request) = fixture();
        let directory = paths.root.join("execution");
        paths::ensure_private(&directory).unwrap();
        paths::atomic_json(&directory.join("request.json"), &request).unwrap();
        assert!(execute_request(&paths, &directory).is_err());
        paths::atomic_json(
            &directory.join("intent.json"),
            &serde_json::json!({"schema":1}),
        )
        .unwrap();
        execute_request(&paths, &directory).unwrap();
        assert_eq!(observe(&directory).unwrap().unwrap().exit_code, Some(7));
        assert!(execute_request(&paths, &directory).is_err());
    }

    #[test]
    fn optional_deadline_runs_bounded_and_unbounded_commands() {
        let (_temporary, paths, mut request) = fixture();
        let encoded = serde_json::to_value(&request).unwrap();
        assert_eq!(encoded["timeout_seconds"], 5);
        assert!(encoded.get("context").is_none());
        assert_eq!(
            serde_json::from_value::<CommandSpec>(encoded)
                .unwrap()
                .timeout_seconds,
            Some(5)
        );
        request.args = vec!["-c".into(), "sleep 2; printf complete".into()];
        request.timeout_seconds = Some(1);
        assert!(run(&paths, &request).unwrap().timed_out);
        request.timeout_seconds = None;
        let result = run(&paths, &request).unwrap();
        assert!(result.success());
        assert_eq!(result.stdout, "complete");
        request.timeout_seconds = Some(0);
        assert!(run(&paths, &request).is_err());
    }

    #[test]
    fn arguments_are_literal_values() {
        let (_temporary, paths, mut request) = fixture();
        request.program = "/usr/bin/printf".into();
        let literal = "$HOME; $(printf expanded) `printf expanded` * ' \"";
        request.args = vec!["%s".into(), literal.into()];
        let result = run(&paths, &request).unwrap();
        assert!(result.success());
        assert_eq!(result.stdout, literal);
    }

    #[test]
    fn failure_to_start_is_retained_and_not_replayed() {
        let (_temporary, paths, mut request) = fixture();
        request.program = paths.root.join("missing-executable");
        request.args = vec!["%s".into(), "unexpected execution".into()];
        let directory = paths.root.join("execution");
        let result = supervise(&paths, &request, &directory).unwrap();
        assert_eq!(result.exit_code, None);
        assert!(result.stdout.is_empty());
        assert!(result.stderr.is_empty());
        assert!(!result.timed_out);
        assert!(result.signal.is_none());
        assert!(result.output_error.is_none());
        assert!(result.start_error.is_some());
        assert!(!result.success());

        std::os::unix::fs::symlink("/usr/bin/printf", &request.program).unwrap();
        let retained = supervise(&paths, &request, &directory).unwrap();
        assert_eq!(retained.start_error, result.start_error);
        assert!(retained.stdout.is_empty());
        assert!(!retained.success());
        assert_eq!(
            observe(&directory).unwrap().unwrap().start_error,
            result.start_error
        );
    }

    fn with_context(paths: &Paths, request: &mut CommandSpec) {
        let context = CommandContext {
            path: paths.root.join("context").join("invocation.json"),
            value: serde_json::json!({"candidate":"exact candidate", "tests":false}),
        };
        request.env.insert(
            "TELETE_CONTEXT".into(),
            context.path.to_str().unwrap().into(),
        );
        request.context = Some(context);
    }

    #[test]
    fn context_commands_inherit_environment_with_explicit_overrides() {
        let (_temporary, paths, mut request) = fixture();
        with_context(&paths, &mut request);
        request.args = vec![
            "-c".into(),
            "printf '%s\\n' \"$HOME\" \"$PATH\" \"$LITERAL_VALUE\" \"$TELETE_CONTEXT\"".into(),
        ];
        request
            .env
            .insert("LITERAL_VALUE".into(), "$HOME; $(printf changed)".into());
        request.env.insert("PATH".into(), "/explicit/path".into());
        let result = run(&paths, &request).unwrap();
        assert!(result.success());
        let observed: Vec<_> = result.stdout.lines().collect();
        assert_eq!(
            observed,
            [
                std::env::var("HOME").unwrap_or_default().as_str(),
                request.env["PATH"].as_str(),
                request.env["LITERAL_VALUE"].as_str(),
                request.env["TELETE_CONTEXT"].as_str(),
            ]
        );
        let context = request.context.as_ref().unwrap();
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&context.path).unwrap()).unwrap();
        assert_eq!(saved, context.value);
        let metadata = std::fs::metadata(&context.path).unwrap();
        assert_eq!(metadata.mode() & 0o777, 0o600);
    }

    #[test]
    fn selected_environment_removals_allow_explicit_overrides() {
        let (_temporary, paths, mut request) = fixture();
        request.args = vec!["-c".into(), "printf '%s' \"${HOME-unset}\"".into()];
        let removed = [std::ffi::OsString::from("HOME")];
        let result = run_without(&paths, &request, &removed).unwrap();
        assert!(result.success());
        assert_eq!(result.stdout, "unset");
        request.env.insert("HOME".into(), "/explicit/home".into());
        let result = run_without(&paths, &request, &removed).unwrap();
        assert!(result.success());
        assert_eq!(result.stdout, "/explicit/home");
    }

    #[test]
    fn context_binding_and_retained_value_cannot_change() {
        let (_temporary, paths, mut request) = fixture();
        with_context(&paths, &mut request);
        let mut changed = request.clone();
        changed.env.remove("TELETE_CONTEXT");
        assert!(run(&paths, &changed).is_err());
        changed = request.clone();
        changed.program = "sh".into();
        assert!(run(&paths, &changed).is_err());
        changed = request.clone();
        changed.context.as_mut().unwrap().path = "invocation.json".into();
        assert!(run(&paths, &changed).is_err());

        let directory = paths.root.join("execution");
        supervise(&paths, &request, &directory).unwrap();
        changed = request.clone();
        changed.context.as_mut().unwrap().value["tests"] = true.into();
        assert!(supervise(&paths, &changed, &directory).is_err());
        assert!(run(&paths, &changed).is_err());
        let context = request.context.as_ref().unwrap();
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&context.path).unwrap()).unwrap();
        assert_eq!(saved, context.value);
    }

    #[test]
    fn an_existing_fifo_cannot_block_context_validation() {
        let (_temporary, paths, mut request) = fixture();
        with_context(&paths, &mut request);
        let context = request.context.as_ref().unwrap();
        paths::ensure_private(context.path.parent().unwrap()).unwrap();
        assert!(
            Command::new("/usr/bin/mkfifo")
                .args(["-m", "600"])
                .arg(&context.path)
                .status()
                .unwrap()
                .success()
        );
        let started = Instant::now();
        assert!(run(&paths, &request).is_err());
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn output_overflow_retains_terminal_status_and_diagnostics() {
        let (_temporary, paths, mut request) = fixture();
        request.args = vec![
            "-c".into(),
            format!(
                "/usr/bin/head -c {} /dev/zero; printf diagnostic >&2; exit 0",
                OUTPUT_LIMIT + 1
            ),
        ];
        let directory = paths.root.join("execution");
        let result = supervise(&paths, &request, &directory).unwrap();
        assert_eq!(result.exit_code, Some(0));
        assert_eq!(result.stdout.len(), OUTPUT_LIMIT);
        assert_eq!(result.stderr, "diagnostic");
        assert!(
            result
                .output_error
                .as_deref()
                .unwrap()
                .contains("stdout: process output exceeded 16 MiB")
        );
        assert!(!result.success());
        assert!(!result.exited_with(&[0, 1]));
        let retained = observe(&directory).unwrap().unwrap();
        assert_eq!(retained.exit_code, Some(0));
        assert_eq!(retained.output_error, result.output_error);
    }

    #[test]
    fn output_read_failure_retains_partial_diagnostics() {
        struct BrokenStream(bool);
        impl Read for BrokenStream {
            fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
                if self.0 {
                    return Err(std::io::Error::other("stream failure"));
                }
                self.0 = true;
                bytes[..7].copy_from_slice(b"partial");
                Ok(7)
            }
        }
        let (output, error) = read_stream(BrokenStream(false));
        assert_eq!(output, "partial");
        assert!(error.unwrap().contains("stream failure"));
    }

    #[test]
    fn signal_completion_is_retained() {
        let (_temporary, paths, mut request) = fixture();
        request.args = vec!["-c".into(), "kill -TERM $$".into()];
        let result = supervise(&paths, &request, &paths.root.join("execution")).unwrap();
        assert_eq!(result.exit_code, None);
        assert_eq!(result.signal, Some(libc::SIGTERM));
        assert!(!result.success());
    }
}
