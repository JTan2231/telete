use crate::broker::{self, ResourceClass};
use crate::inventory::{Inventory, Product};
use crate::model::{CommitId, ProductId};
use crate::paths::{self, Paths};
use crate::process::{self, CommandSpec, ProcessResult};
use crate::signing::{self, SigningPolicy};
use anyhow::{Context, Result, bail, ensure};
use cell_install::adapter::{DependencyCandidate, Request};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Component, Path, PathBuf};
use uuid::Uuid;

#[derive(Debug)]
pub(crate) enum PrepareError {
    Compilation(String),
}
impl fmt::Display for PrepareError {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Compilation(detail) => write!(out, "release compilation failed: {detail}"),
        }
    }
}
impl std::error::Error for PrepareError {}

fn require_compilation(result: &ProcessResult) -> Result<()> {
    ensure!(
        !result.timed_out,
        "production compilation timed out; execution requires reconciliation"
    );
    ensure!(
        result.start_error.is_none() && result.output_error.is_none() && result.signal.is_none(),
        "production compiler execution failed: start={:?}, output={:?}, signal={:?}",
        result.start_error,
        result.output_error,
        result.signal
    );
    if !result.success() {
        return Err(
            PrepareError::Compilation(format!("{}\n{}", result.stdout, result.stderr)).into(),
        );
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Preparation {
    pub schema: u32,
    pub source: CommitId,
    pub products: Vec<String>,
    pub signing_policy: SigningPolicy,
    pub candidates: BTreeMap<String, Candidate>,
    pub release_check: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
// These retained field names match the existing candidate package format.
#[allow(clippy::struct_field_names)]
pub(crate) struct Candidate {
    pub schema: u32,
    pub candidate_id: String,
    pub source_commit: CommitId,
    pub source_key: String,
    pub product: String,
    pub candidate_dir: PathBuf,
    pub binaries: BTreeMap<String, CandidateBinary>,
    pub signing_policy: SigningPolicy,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CandidateBinary {
    pub path: String,
    pub code_identifier: String,
    pub version: String,
}

fn spec(
    paths: &Paths,
    repo: &Path,
    program: &str,
    args: Vec<String>,
    confined: bool,
) -> CommandSpec {
    CommandSpec {
        program: program.into(),
        args,
        cwd: repo.into(),
        env: paths.environment(),
        timeout_seconds: Some(1800),
        stdin: None,
        confined,
        context: None,
    }
}

fn git(paths: &Paths, repo: &Path, args: &[&str]) -> Result<String> {
    let result = process::run(
        paths,
        &spec(
            paths,
            repo,
            "git",
            args.iter().map(|s| (*s).into()).collect(),
            false,
        ),
    )?;
    ensure!(result.success(), "Git operation failed: {}", result.stderr);
    Ok(result.stdout.trim().into())
}

fn require_source(paths: &Paths, repo: &Path, commit: &CommitId) -> Result<()> {
    ensure!(
        git(paths, repo, &["rev-parse", "HEAD"])? == commit.to_string(),
        "source HEAD does not match the frozen commit"
    );
    ensure!(
        git(
            paths,
            repo,
            &["status", "--porcelain", "--untracked-files=all"]
        )?
        .is_empty(),
        "production source must be clean"
    );
    Ok(())
}

fn regular(path: &Path) -> Result<()> {
    ensure!(
        fs::symlink_metadata(path)?.is_file(),
        "artifact must be a regular non-symbolic file: {}",
        path.display()
    );
    Ok(())
}

fn within_workspace(paths: &Paths, path: &Path) -> Result<()> {
    ensure!(
        path.is_absolute() && !path.components().any(|c| matches!(c, Component::ParentDir)),
        "work path must be absolute and literal"
    );
    ensure!(
        path.starts_with(&paths.workspace),
        "generated material must stay in the configured workspace"
    );
    let mut ancestor = Some(path);
    while let Some(value) = ancestor {
        match fs::symlink_metadata(value) {
            Ok(info) => ensure!(
                !info.file_type().is_symlink(),
                "generated material path is symbolic"
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        if value == paths.workspace {
            break;
        }
        ancestor = value.parent();
    }
    Ok(())
}

fn selected_products(repo: &Path, products: &[String]) -> Result<Vec<Product>> {
    ensure!(
        !products.is_empty(),
        "select at least one production product"
    );
    let mut selected = Inventory::load(repo)?.select(products)?;
    for product in &mut selected {
        if product.id.to_string() == "decisions" {
            product.id = ProductId::new("krisis")?;
        }
    }
    selected.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(selected)
}

fn binary_metadata(
    metadata: &Value,
    selected: &[Product],
) -> Result<BTreeMap<String, (String, String)>> {
    let packages = metadata["packages"]
        .as_array()
        .context("Cargo metadata package inventory is absent")?;
    let mut binaries = BTreeMap::new();
    for product in selected {
        ensure!(
            !product.binaries.is_empty(),
            "product has no production binaries: {}",
            product.id
        );
        for binary in &product.binaries {
            ensure!(
                binary.source.as_path() == Path::new(&format!("target/release/{}", binary.name)),
                "invalid release binary declaration"
            );
            let matches = packages
                .iter()
                .filter(|package| {
                    product
                        .packages
                        .iter()
                        .any(|name| package["name"].as_str() == Some(&name.to_string()))
                        && package["targets"].as_array().is_some_and(|targets| {
                            targets.iter().any(|target| {
                                target["name"].as_str() == Some(&binary.name)
                                    && target["kind"]
                                        .as_array()
                                        .is_some_and(|kinds| kinds.iter().any(|kind| kind == "bin"))
                            })
                        })
                })
                .collect::<Vec<_>>();
            ensure!(
                matches.len() == 1,
                "release executable must select exactly one Cargo package: {}",
                binary.name
            );
            let package = matches[0];
            ensure!(
                binaries
                    .insert(
                        binary.name.clone(),
                        (
                            package["name"]
                                .as_str()
                                .context("Cargo package name absent")?
                                .into(),
                            format!(
                                "{} {}",
                                binary.name,
                                package["version"]
                                    .as_str()
                                    .context("Cargo package version absent")?
                            )
                        )
                    )
                    .is_none(),
                "colliding production executable name"
            );
        }
    }
    Ok(binaries)
}

fn preparation_staging(parent: &Path) -> Result<tempfile::TempDir> {
    let temporary = tempfile::Builder::new()
        .prefix(".preparation-")
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir_in(parent)?;
    paths::ensure_private(temporary.path())?;
    Ok(temporary)
}

#[allow(clippy::too_many_lines)]
pub(crate) fn prepare(
    paths: &Paths,
    repo: &Path,
    commit: &CommitId,
    products: &[String],
    policy: &SigningPolicy,
    output: &Path,
) -> Result<Preparation> {
    paths.require_capacity()?;
    within_workspace(paths, output)?;
    ensure!(
        !output.starts_with(repo),
        "preparation output must be outside source"
    );
    require_source(paths, repo, commit)?;
    let selected = selected_products(repo, products)?;
    let names = selected
        .iter()
        .map(|p| p.id.to_string())
        .collect::<Vec<_>>();
    ensure!(
        signing::selected(paths)? == *policy,
        "signing selection changed after admission"
    );
    if output.try_exists()? {
        regular(&output.join("result.json"))?;
        let prepared: Preparation = serde_json::from_slice(&fs::read(output.join("result.json"))?)?;
        ensure!(
            prepared.schema == 1
                && prepared.source == *commit
                && prepared.products == names
                && prepared.signing_policy == *policy
                && prepared.release_check,
            "existing preparation does not match the frozen request"
        );
        validate_preparation(&prepared, &names)?;
        return Ok(prepared);
    }
    signing::preflight(policy)?;
    let target = paths.targets().join("production");
    paths::ensure_private(&target)?;
    let _build = paths::lock(&target.join("build.lock"), true)?;
    let mut environment = paths.environment();
    environment.insert("CARGO_TARGET_DIR".into(), target.display().to_string());
    environment.insert("CARGO_BUILD_BUILD_DIR".into(), target.display().to_string());
    environment.insert("CARGO_INCREMENTAL".into(), "0".into());
    environment.insert("CARGO_BUILD_WARNINGS".into(), "deny".into());
    let offline = selected.iter().any(|product| product.offline);
    if offline {
        environment.insert("CARGO_NET_OFFLINE".into(), "true".into());
    }
    let mut metadata_spec = spec(
        paths,
        repo,
        "cargo",
        vec![
            "metadata".into(),
            "--format-version".into(),
            "1".into(),
            "--no-deps".into(),
            "--locked".into(),
        ],
        true,
    );
    metadata_spec.timeout_seconds = None;
    metadata_spec.env = environment.clone();
    let preparation_id = Uuid::new_v4().to_string();
    let metadata = broker::run(
        paths,
        &format!("production-{preparation_id}-metadata"),
        ResourceClass::Heavy,
        &metadata_spec,
    )?;
    ensure!(
        metadata.success(),
        "Cargo metadata failed: {}",
        metadata.stderr
    );
    let metadata: Value = serde_json::from_str(&metadata.stdout)?;
    let binaries = binary_metadata(&metadata, &selected)?;
    let packages = selected
        .iter()
        .flat_map(|p| p.packages.iter().map(ToString::to_string))
        .collect::<BTreeSet<_>>();
    ensure!(
        !packages.is_empty(),
        "production selection has no Cargo packages"
    );
    let mut args = vec![
        "build".into(),
        "--release".into(),
        "--locked".into(),
        "--manifest-path".into(),
        repo.join("Cargo.toml").display().to_string(),
        "--target-dir".into(),
        target.display().to_string(),
    ];
    if offline {
        args.push("--offline".into());
    }
    for package in packages {
        args.extend(["--package".into(), package]);
    }
    let mut build = spec(paths, repo, "cargo", args, true);
    build.timeout_seconds = None;
    build.env = environment;
    let compiled = broker::run(
        paths,
        &format!("production-{preparation_id}-build"),
        ResourceClass::Heavy,
        &build,
    )?;
    let parent = output.parent().context("preparation parent absent")?;
    paths::ensure_private(parent)?;
    let diagnostic_prefix = format!(
        "{}.build",
        output
            .file_name()
            .context("preparation name absent")?
            .to_string_lossy()
    );
    crate::store::atomic_bytes(
        &parent.join(format!("{diagnostic_prefix}.stdout")),
        compiled.stdout.as_bytes(),
    )?;
    crate::store::atomic_bytes(
        &parent.join(format!("{diagnostic_prefix}.stderr")),
        compiled.stderr.as_bytes(),
    )?;
    require_compilation(&compiled)?;
    require_source(paths, repo, commit)?;
    let temporary = preparation_staging(parent)?;
    paths::ensure_private(&temporary.path().join("candidates"))?;
    let mut candidates = BTreeMap::new();
    for product in selected {
        let name = product.id.to_string();
        let directory = temporary.path().join("candidates").join(&name);
        paths::ensure_private(&directory)?;
        paths::ensure_private(&directory.join("bin"))?;
        let mut declarations = BTreeMap::new();
        for binary in &product.binaries {
            let source = target.join("release").join(&binary.name);
            regular(&source)?;
            let destination = directory.join("bin").join(&binary.name);
            fs::copy(source, &destination)?;
            fs::set_permissions(&destination, fs::Permissions::from_mode(0o755))?;
            signing::sign(&destination, policy, &name, &binary.name)?;
            fs::File::open(&destination)?.sync_all()?;
            fs::set_permissions(&destination, fs::Permissions::from_mode(0o555))?;
            declarations.insert(
                binary.name.clone(),
                CandidateBinary {
                    path: format!("bin/{}", binary.name),
                    code_identifier: signing::identifier(policy, &name, &binary.name)?,
                    version: binaries[&binary.name].1.clone(),
                },
            );
        }
        let candidate = Candidate {
            schema: 1,
            candidate_id: format!("uuid:{}", Uuid::new_v4().simple()),
            source_commit: commit.clone(),
            source_key: commit.to_string(),
            product: name.clone(),
            candidate_dir: output.join("candidates").join(&name),
            binaries: declarations,
            signing_policy: policy.clone(),
        };
        paths::atomic_json(&directory.join("candidate.json"), &candidate)?;
        fs::set_permissions(
            directory.join("candidate.json"),
            fs::Permissions::from_mode(0o444),
        )?;
        fs::set_permissions(directory.join("bin"), fs::Permissions::from_mode(0o555))?;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o555))?;
        candidates.insert(name, candidate);
    }
    ensure!(
        signing::selected(paths)? == *policy,
        "signing selection changed during preparation"
    );
    require_source(paths, repo, commit)?;
    let prepared = Preparation {
        schema: 1,
        source: commit.clone(),
        products: names,
        signing_policy: policy.clone(),
        candidates,
        release_check: true,
    };
    paths::atomic_json(&temporary.path().join("result.json"), &prepared)?;
    let staging = temporary.keep();
    fs::rename(staging, output)?;
    fs::File::open(parent)?.sync_all()?;
    Ok(prepared)
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Declaration {
    schema: u32,
    product: String,
    order: i64,
    steps: Vec<Instruction>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Instruction {
    Run {
        id: String,
        argv: Vec<String>,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
        #[serde(default)]
        stdin: Option<String>,
        #[serde(default)]
        timeout_seconds: Option<u64>,
    },
    Copy {
        id: String,
        source: String,
        destination: String,
        #[serde(default)]
        mode: Option<u32>,
    },
    Link {
        id: String,
        target: String,
        destination: String,
    },
}
impl Instruction {
    fn id(&self) -> &str {
        match self {
            Self::Run { id, .. } | Self::Copy { id, .. } | Self::Link { id, .. } => id,
        }
    }
    fn kind(&self) -> &str {
        match self {
            Self::Run { .. } => "run",
            Self::Copy { .. } => "copy",
            Self::Link { .. } => "link",
        }
    }
    fn validate(&self) -> Result<()> {
        ensure!(
            !self.id().is_empty() && !self.id().contains('\0'),
            "instruction ID is empty or invalid"
        );
        let text = |value: &str| -> Result<()> {
            ensure!(!value.contains('\0'), "instruction text contains NUL");
            Ok(())
        };
        match self {
            Self::Run {
                argv,
                cwd,
                env,
                stdin,
                timeout_seconds,
                ..
            } => {
                ensure!(
                    !argv.is_empty() && !argv[0].is_empty(),
                    "run instruction requires literal arguments"
                );
                for value in argv {
                    text(value)?;
                }
                if let Some(value) = cwd {
                    text(value)?;
                }
                for (key, value) in env {
                    ensure!(
                        !key.is_empty() && !key.contains(['=', '\0']),
                        "invalid instruction environment name"
                    );
                    text(value)?;
                }
                if let Some(value) = stdin {
                    text(value)?;
                }
                ensure!(
                    timeout_seconds.is_none_or(|value| value > 0),
                    "instruction timeout must be positive"
                );
            }
            Self::Copy {
                source,
                destination,
                mode,
                ..
            } => {
                text(source)?;
                text(destination)?;
                ensure!(
                    !source.is_empty() && !destination.is_empty(),
                    "copy paths absent"
                );
                ensure!(mode.is_none_or(|value| value <= 0o777), "invalid copy mode");
            }
            Self::Link {
                target,
                destination,
                ..
            } => {
                text(target)?;
                text(destination)?;
                ensure!(
                    !target.is_empty() && !destination.is_empty(),
                    "link paths absent"
                );
            }
        }
        Ok(())
    }
}
impl Declaration {
    fn validate(&self, product: &str) -> Result<()> {
        ensure!(
            self.schema == 1 && self.product == product && !self.steps.is_empty(),
            "invalid product deployment declaration"
        );
        let mut ids = BTreeSet::new();
        for instruction in &self.steps {
            instruction.validate()?;
            ensure!(
                ids.insert(instruction.id()),
                "duplicate deployment instruction ID"
            );
        }
        Ok(())
    }
}

fn expand(text: &str, context: &BTreeMap<String, String>) -> Result<String> {
    let mut output = String::new();
    let mut chars = text.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                output.push('{');
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                output.push('}');
            }
            '{' => {
                let mut key = String::new();
                let mut closed = false;
                for value in chars.by_ref() {
                    if value == '}' {
                        closed = true;
                        break;
                    }
                    key.push(value);
                }
                ensure!(closed, "unclosed instruction variable");
                output.push_str(
                    context
                        .get(&key)
                        .context("unknown instruction path variable")?,
                );
            }
            '}' => bail!("unmatched instruction variable terminator"),
            other => output.push(other),
        }
    }
    Ok(output)
}
fn absolute(text: &str, context: &BTreeMap<String, String>) -> Result<PathBuf> {
    let path = PathBuf::from(expand(text, context)?);
    ensure!(
        path.is_absolute(),
        "instruction filesystem path must be absolute"
    );
    Ok(path)
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DeploymentState {
    Running,
    Succeeded,
    Failed,
    Interrupted,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum InstructionState {
    Running,
    Succeeded,
    Failed,
    Interrupted,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct InstructionResult {
    pub id: String,
    pub product: String,
    pub kind: String,
    pub state: InstructionState,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct DeploymentResult {
    pub schema: u32,
    pub request_id: String,
    pub run_id: String,
    pub source: CommitId,
    pub products: Vec<String>,
    pub state: DeploymentState,
    pub exit_code: Option<i32>,
    pub instructions: Vec<InstructionResult>,
    pub diagnostic: Option<String>,
    #[serde(default)]
    pub acknowledged: bool,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct FrozenRequest {
    repository: PathBuf,
    prepared: Preparation,
    products: Vec<String>,
    declarations: Vec<Declaration>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    settings: BTreeMap<String, Value>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Operation {
    request: FrozenRequest,
    result: DeploymentResult,
}

fn request_id(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 256
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_.:-".contains(&b)),
        "invalid deployment request identity"
    );
    Ok(())
}
fn index(paths: &Paths) -> Result<BTreeMap<String, String>> {
    let path = paths.root.join("deployments/index.json");
    if !path.try_exists()? {
        return Ok(BTreeMap::new());
    }
    regular(&path)?;
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}
fn operation_path(paths: &Paths, run: &str) -> Result<PathBuf> {
    ensure!(
        Uuid::parse_str(run)?.to_string() == run,
        "invalid retained deployment identity"
    );
    Ok(paths
        .root
        .join("deployments/operations")
        .join(format!("{run}.json")))
}
pub(crate) fn observe(paths: &Paths, request: &str) -> Result<Option<DeploymentResult>> {
    request_id(request)?;
    let Some(run) = index(paths)?.get(request).cloned() else {
        return Ok(None);
    };
    let path = operation_path(paths, &run)?;
    let mut operation: Operation = serde_json::from_slice(&fs::read(&path)?)?;
    ensure!(
        operation.result.request_id == request && operation.result.run_id == run,
        "deployment receipt correlation failed"
    );
    correlate_record(&operation)?;
    if let Ok(_lock) = paths::lock(&paths.root.join("deployments/deployment.lock"), false) {
        reconcile_record(paths, &mut operation)?;
        paths::atomic_json(&path, &operation)?;
        clear_settled_marker(paths, &operation.result)?;
    } else if operation.result.state == DeploymentState::Running {
        operation.result.state = DeploymentState::Interrupted;
        operation.result.diagnostic =
            Some("deployment ownership remains active; effects are not settled".into());
    }
    Ok(Some(operation.result))
}

fn run_directory(paths: &Paths, run: &str) -> PathBuf {
    paths.root.join("deployments/runs").join(run)
}
fn process_directory(paths: &Paths, run: &str, index: usize) -> PathBuf {
    run_directory(paths, run)
        .join("steps")
        .join(format!("{index:04}"))
        .join("process")
}

fn command(
    paths: &Paths,
    operation: &Operation,
    product: &str,
    instruction: &Instruction,
) -> Result<CommandSpec> {
    let Instruction::Run {
        argv,
        cwd,
        env,
        stdin,
        timeout_seconds,
        ..
    } = instruction
    else {
        bail!("expected run instruction")
    };
    let candidate = &operation.request.prepared.candidates[product];
    let run_dir = run_directory(paths, &operation.result.run_id);
    let worktree = run_dir.join("worktree");
    let context = BTreeMap::from([
        ("product".into(), product.into()),
        (
            "candidate_dir".into(),
            candidate.candidate_dir.display().to_string(),
        ),
        ("source_root".into(), worktree.display().to_string()),
        ("run_dir".into(), run_dir.display().to_string()),
        ("home".into(), signing::home()?.display().to_string()),
    ]);
    let dependencies = operation
        .request
        .prepared
        .candidates
        .iter()
        .filter(|(name, _)| name.as_str() != product && operation.result.products.contains(name))
        .map(|(name, candidate)| {
            Ok((
                name.clone(),
                DependencyCandidate {
                    candidate_dir: candidate.candidate_dir.clone(),
                    candidate: serde_json::to_value(candidate)?,
                },
            ))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    let input = Request {
        schema: 2,
        product: product.into(),
        run_id: operation.result.run_id.clone(),
        run_dir: run_dir.clone(),
        source_root: worktree.clone(),
        candidate_dir: Some(candidate.candidate_dir.clone()),
        candidate: Some(serde_json::to_value(candidate)?),
        selected_products: operation.result.products.clone(),
        settings: operation.request.settings.get(product).cloned(),
        dependency_settings: operation.request.settings.clone(),
        dependency_candidates: dependencies,
    };
    let args = argv
        .iter()
        .map(|text| expand(text, &context))
        .collect::<Result<Vec<_>>>()?;
    let mut environment = paths.environment();
    for (key, value) in env {
        environment.insert(key.clone(), expand(value, &context)?);
    }
    Ok(CommandSpec {
        program: args[0].clone().into(),
        args: args[1..].into(),
        cwd: cwd
            .as_ref()
            .map(|text| absolute(text, &context))
            .transpose()?
            .unwrap_or(worktree),
        env: environment,
        timeout_seconds: *timeout_seconds,
        stdin: stdin
            .as_ref()
            .map(|text| -> Result<String> {
                if text == "deployment_request" {
                    Ok(serde_json::to_string(&input)?)
                } else {
                    Ok(text.clone())
                }
            })
            .transpose()?,
        confined: false,
        context: None,
    })
}

fn correlate_record(operation: &Operation) -> Result<()> {
    ensure!(
        operation.result.schema == 2
            && operation.result.source == operation.request.prepared.source
            && operation.result.products == operation.request.products,
        "deployment receipt source or scope differs from its frozen request"
    );
    validate_preparation(&operation.request.prepared, &operation.request.products)?;
    let declared = operation
        .request
        .declarations
        .iter()
        .flat_map(|d| d.steps.iter().map(move |step| (&d.product, step)))
        .collect::<Vec<_>>();
    ensure!(
        operation.result.instructions.len() <= declared.len(),
        "deployment contains undeclared instruction results"
    );
    for (index, result) in operation.result.instructions.iter().enumerate() {
        let (product, instruction) = declared[index];
        ensure!(
            result.product == *product
                && result.id == format!("{product}:{}", instruction.id())
                && result.kind == instruction.kind(),
            "deployment instruction result correlation failed"
        );
        if index + 1 < operation.result.instructions.len() {
            ensure!(
                result.state == InstructionState::Succeeded,
                "deployment continued after an unsettled instruction"
            );
        }
        if result.state == InstructionState::Succeeded && result.kind == "run" {
            ensure!(
                result.exit_code == Some(0),
                "run success has no successful process completion"
            );
        }
    }
    if operation.result.state == DeploymentState::Succeeded {
        ensure!(
            operation.result.exit_code == Some(0)
                && operation.result.instructions.len() == declared.len()
                && operation
                    .result
                    .instructions
                    .iter()
                    .all(|step| step.state == InstructionState::Succeeded),
            "deployment success has incomplete instruction evidence"
        );
    }
    Ok(())
}

fn reconciled_state(instructions: &[InstructionResult], expected: usize) -> DeploymentState {
    if instructions.len() == expected
        && instructions
            .iter()
            .all(|i| i.state == InstructionState::Succeeded)
    {
        DeploymentState::Succeeded
    } else if instructions
        .last()
        .is_some_and(|i| i.state == InstructionState::Failed)
    {
        DeploymentState::Failed
    } else {
        DeploymentState::Interrupted
    }
}

fn reconcile_record(paths: &Paths, operation: &mut Operation) -> Result<()> {
    correlate_record(operation)?;
    if !matches!(
        operation.result.state,
        DeploymentState::Running | DeploymentState::Interrupted
    ) || operation.result.acknowledged
    {
        return Ok(());
    }
    let declared = operation
        .request
        .declarations
        .iter()
        .flat_map(|d| d.steps.iter().map(move |step| (&d.product, step)))
        .collect::<Vec<_>>();
    for (index, &(product, instruction)) in declared
        .iter()
        .take(operation.result.instructions.len())
        .enumerate()
    {
        let saved = &operation.result.instructions[index];
        if saved.kind != "run"
            || !matches!(
                saved.state,
                InstructionState::Running | InstructionState::Interrupted
            )
        {
            continue;
        }
        let directory = process_directory(paths, &operation.result.run_id, index);
        if let Some(result) = process::observe(&directory)? {
            let expected = command(paths, operation, product, instruction)?;
            let request: CommandSpec =
                serde_json::from_slice(&fs::read(directory.join("request.json"))?)?;
            ensure!(
                serde_json::to_value(request)? == serde_json::to_value(expected)?,
                "deployment process does not match its frozen instruction"
            );
            let saved = &mut operation.result.instructions[index];
            saved.state = if result.timed_out {
                InstructionState::Interrupted
            } else if result.success() {
                InstructionState::Succeeded
            } else {
                InstructionState::Failed
            };
            saved.exit_code = result.exit_code;
            saved.stdout = result.stdout;
            saved.stderr = result.stderr;
        }
    }
    operation.result.state = reconciled_state(&operation.result.instructions, declared.len());
    operation.result.exit_code = match operation.result.state {
        DeploymentState::Succeeded => Some(0),
        DeploymentState::Failed => Some(1),
        _ => None,
    };
    operation.result.diagnostic=match operation.result.state {DeploymentState::Succeeded=>None,DeploymentState::Failed=>Some("retained process evidence reports a failed instruction".into()),
        _=>Some("executor stopped before every instruction settled; remaining instructions were not resumed".into())};
    correlate_record(operation)
}

fn clear_settled_marker(paths: &Paths, result: &DeploymentResult) -> Result<()> {
    if !matches!(
        result.state,
        DeploymentState::Succeeded | DeploymentState::Failed
    ) && !result.acknowledged
    {
        return Ok(());
    }
    let path = paths.root.join("deployments/active.json");
    if !path.try_exists()? {
        return Ok(());
    }
    let marker: Value = serde_json::from_slice(&fs::read(&path)?)?;
    if marker["request_id"] == result.request_id && marker["run_id"] == result.run_id {
        fs::remove_file(path)?;
        fs::File::open(paths.root.join("deployments"))?.sync_all()?;
    }
    Ok(())
}

fn validate_preparation(prepared: &Preparation, products: &[String]) -> Result<()> {
    ensure!(
        prepared.schema == 1 && prepared.release_check,
        "unsupported production preparation"
    );
    let scope = prepared.products.iter().cloned().collect::<BTreeSet<_>>();
    ensure!(
        scope.len() == prepared.products.len()
            && scope == prepared.candidates.keys().cloned().collect(),
        "preparation candidate scope mismatch"
    );
    for product in products {
        let candidate = prepared
            .candidates
            .get(product)
            .context("deployment product was not prepared")?;
        ensure!(
            candidate.schema == 1
                && candidate.product == *product
                && candidate.source_commit == prepared.source
                && candidate.source_key == prepared.source.to_string()
                && candidate.signing_policy == prepared.signing_policy
                && !candidate.binaries.is_empty(),
            "prepared candidate correlation failed"
        );
        let identity = candidate
            .candidate_id
            .strip_prefix("uuid:")
            .context("invalid prepared candidate identity")?;
        ensure!(
            Uuid::parse_str(identity).is_ok(),
            "invalid prepared candidate identity"
        );
        ensure!(
            candidate.candidate_dir.is_absolute(),
            "prepared candidate path must be absolute"
        );
        for (name, binary) in &candidate.binaries {
            ensure!(
                binary.path == format!("bin/{name}") && !name.contains('/'),
                "invalid candidate executable path"
            );
        }
    }
    Ok(())
}

fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    if metadata.file_type().is_symlink() {
        symlink(fs::read_link(source)?, destination)?;
    } else if metadata.is_dir() {
        fs::create_dir(destination)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            copy_tree(&entry.path(), &destination.join(entry.file_name()))?;
        }
        fs::set_permissions(destination, metadata.permissions())?;
    } else {
        ensure!(metadata.is_file(), "copy source is a special file");
        fs::copy(source, destination)?;
        fs::File::open(destination)?.sync_all()?;
    }
    Ok(())
}

fn file_instruction(instruction: &Instruction, context: &BTreeMap<String, String>) -> Result<()> {
    let destination = match instruction {
        Instruction::Copy { destination, .. } | Instruction::Link { destination, .. } => {
            absolute(destination, context)?
        }
        Instruction::Run { .. } => bail!("expected file instruction"),
    };
    let parent = destination
        .parent()
        .context("instruction destination has no parent")?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".telete-{}", Uuid::new_v4()));
    let result = (|| -> Result<()> {
        match instruction {
            Instruction::Copy { source, mode, .. } => {
                copy_tree(&absolute(source, context)?, &temporary)?;
                if let Some(mode) = mode {
                    fs::set_permissions(&temporary, fs::Permissions::from_mode(*mode))?;
                }
            }
            Instruction::Link { target, .. } => {
                symlink(expand(target, context)?, &temporary)?;
            }
            Instruction::Run { .. } => bail!("expected file instruction"),
        }
        fs::rename(&temporary, &destination)?;
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if temporary.is_symlink() || temporary.is_file() {
        let _ = fs::remove_file(&temporary);
    } else if temporary.is_dir() {
        let _ = fs::remove_dir_all(&temporary);
    }
    result
}

#[allow(clippy::too_many_lines)]
pub(crate) fn execute(
    paths: &Paths,
    repo: &Path,
    request: &str,
    prepared: &Preparation,
    products: &[String],
    settings: &BTreeMap<String, Value>,
) -> Result<DeploymentResult> {
    request_id(request)?;
    paths.require_capacity()?;
    let storage = paths.root.join("deployments");
    paths::ensure_private(&storage.join("operations"))?;
    let _lock = paths::lock(&storage.join("deployment.lock"), false)?;
    require_source(paths, repo, &prepared.source)?;
    let selected = selected_products(repo, products)?;
    let mut declarations = Vec::new();
    for product in &selected {
        let text = git(
            paths,
            repo,
            &[
                "show",
                &format!(
                    "{}:{}/deployment/manifest.json",
                    prepared.source,
                    product.root.display()
                ),
            ],
        )?;
        let declaration: Declaration = serde_json::from_str(&text)?;
        declaration.validate(&product.id.to_string())?;
        declarations.push(declaration);
    }
    declarations.sort_by(|a, b| (a.order, &a.product).cmp(&(b.order, &b.product)));
    let names = declarations
        .iter()
        .map(|d| d.product.clone())
        .collect::<Vec<_>>();
    validate_preparation(prepared, &names)?;
    ensure!(
        settings
            .iter()
            .all(|(product, value)| names.contains(product) && value.is_object()),
        "settings must contain only selected canonical products and JSON objects"
    );
    for candidate in prepared.candidates.values() {
        within_workspace(paths, &candidate.candidate_dir)?;
    }
    let frozen = FrozenRequest {
        repository: fs::canonicalize(repo)?,
        prepared: prepared.clone(),
        products: names.clone(),
        declarations,
        settings: settings.clone(),
    };
    let mut index = index(paths)?;
    if let Some(run) = index.get(request) {
        let path = operation_path(paths, run)?;
        let mut old: Operation = serde_json::from_slice(&fs::read(&path)?)?;
        ensure!(
            old.request == frozen,
            "deployment request identity already belongs to different frozen inputs"
        );
        reconcile_record(paths, &mut old)?;
        paths::atomic_json(&path, &old)?;
        clear_settled_marker(paths, &old.result)?;
        return Ok(old.result);
    }
    if storage.join("active.json").try_exists()? {
        let marker: Value = serde_json::from_slice(&fs::read(storage.join("active.json"))?)?;
        let active_request = marker["request_id"]
            .as_str()
            .context("active deployment request missing")?;
        let active_run = marker["run_id"]
            .as_str()
            .context("active deployment run missing")?;
        ensure!(
            index
                .get(active_request)
                .is_some_and(|run| run == active_run),
            "active deployment index differs"
        );
        let active_path = operation_path(paths, active_run)?;
        let mut active: Operation = serde_json::from_slice(&fs::read(&active_path)?)?;
        ensure!(
            active.result.request_id == active_request && active.result.run_id == active_run,
            "active deployment owner differs"
        );
        reconcile_record(paths, &mut active)?;
        paths::atomic_json(&active_path, &active)?;
        clear_settled_marker(paths, &active.result)?;
    }
    ensure!(
        !storage.join("active.json").try_exists()?,
        "another deployment has retained uncertain effects; acknowledge it explicitly before a new request"
    );
    ensure!(
        signing::selected(paths)? == prepared.signing_policy,
        "signing selection changed before deployment"
    );
    paths::ensure_private(&storage.join("runs"))?;
    let run = Uuid::new_v4().to_string();
    let run_dir = storage.join("runs").join(&run);
    paths::ensure_private(&run_dir)?;
    let path = operation_path(paths, &run)?;
    let mut operation = Operation {
        request: frozen,
        result: DeploymentResult {
            schema: 2,
            request_id: request.into(),
            run_id: run.clone(),
            source: prepared.source.clone(),
            products: names,
            state: DeploymentState::Running,
            exit_code: None,
            instructions: Vec::new(),
            diagnostic: None,
            acknowledged: false,
        },
    };
    paths::atomic_json(&path, &operation)?;
    index.insert(request.into(), run.clone());
    paths::atomic_json(&storage.join("index.json"), &index)?;
    paths::atomic_json(
        &storage.join("active.json"),
        &json!({"request_id":request,"run_id":run}),
    )?;
    let worktree = run_dir.join("worktree");
    let execution = (|| -> Result<()> {
        git(
            paths,
            repo,
            &[
                "worktree",
                "add",
                "--detach",
                &worktree.display().to_string(),
                &prepared.source.to_string(),
            ],
        )?;
        for declaration in operation.request.declarations.clone() {
            let candidate = &prepared.candidates[&declaration.product];
            let context = BTreeMap::from([
                ("product".into(), declaration.product.clone()),
                (
                    "candidate_dir".into(),
                    candidate.candidate_dir.display().to_string(),
                ),
                ("source_root".into(), worktree.display().to_string()),
                ("run_dir".into(), run_dir.display().to_string()),
                ("home".into(), signing::home()?.display().to_string()),
            ]);
            for instruction in declaration.steps {
                let index = operation.result.instructions.len();
                operation.result.instructions.push(InstructionResult {
                    id: format!("{}:{}", declaration.product, instruction.id()),
                    product: declaration.product.clone(),
                    kind: instruction.kind().into(),
                    state: InstructionState::Running,
                    exit_code: None,
                    stdout: String::new(),
                    stderr: String::new(),
                });
                paths::atomic_json(&path, &operation)?;
                match &instruction {
                    Instruction::Run { .. } => {
                        let command =
                            command(paths, &operation, &declaration.product, &instruction)?;
                        let directory = process_directory(paths, &run, index);
                        paths::ensure_private(&run_dir.join("steps"))?;
                        paths::ensure_private(
                            directory.parent().context("process parent absent")?,
                        )?;
                        let result = match process::supervise(paths, &command, &directory) {
                            Ok(result) => result,
                            Err(error) => {
                                operation.result.instructions[index].state =
                                    InstructionState::Interrupted;
                                return Err(error);
                            }
                        };
                        crate::store::atomic_bytes(
                            &run_dir.join(format!("{index:04}.stdout")),
                            result.stdout.as_bytes(),
                        )?;
                        crate::store::atomic_bytes(
                            &run_dir.join(format!("{index:04}.stderr")),
                            result.stderr.as_bytes(),
                        )?;
                        let succeeded = result.success();
                        let saved = &mut operation.result.instructions[index];
                        saved.exit_code = result.exit_code;
                        saved.stdout = result.stdout;
                        saved.stderr = result.stderr;
                        saved.state = if result.timed_out {
                            InstructionState::Interrupted
                        } else if succeeded {
                            InstructionState::Succeeded
                        } else {
                            InstructionState::Failed
                        };
                        paths::atomic_json(&path, &operation)?;
                        ensure!(
                            !result.timed_out,
                            "deployment command timed out; effects remain uncertain"
                        );
                        ensure!(succeeded, "deployment instruction failed");
                    }
                    _ => match file_instruction(&instruction, &context) {
                        Ok(()) => {
                            operation.result.instructions[index].state =
                                InstructionState::Succeeded;
                        }
                        Err(error) => {
                            operation.result.instructions[index].state = InstructionState::Failed;
                            paths::atomic_json(&path, &operation)?;
                            return Err(error);
                        }
                    },
                }
                paths::atomic_json(&path, &operation)?;
            }
        }
        Ok(())
    })();
    match execution {
        Ok(()) => {
            operation.result.state = DeploymentState::Succeeded;
            operation.result.exit_code = Some(0);
        }
        Err(error) => {
            let uncertain = operation.result.instructions.iter().any(|i| {
                matches!(
                    i.state,
                    InstructionState::Running | InstructionState::Interrupted
                )
            });
            operation.result.state = if uncertain {
                DeploymentState::Interrupted
            } else {
                DeploymentState::Failed
            };
            operation.result.exit_code = if uncertain { None } else { Some(1) };
            operation.result.diagnostic = Some(error.to_string());
        }
    }
    paths::atomic_json(&path, &operation)?;
    if operation.result.state != DeploymentState::Interrupted {
        fs::remove_file(storage.join("active.json"))?;
        fs::File::open(&storage)?.sync_all()?;
        // Retain diagnostics and receipts. Program state and lifecycle remain
        // owned by the product installers; the executor performs no recovery.
        let _ = git(
            paths,
            repo,
            &[
                "worktree",
                "remove",
                "--force",
                &worktree.display().to_string(),
            ],
        );
    }
    Ok(operation.result)
}

pub(crate) fn acknowledge(paths: &Paths, request: &str) -> Result<DeploymentResult> {
    request_id(request)?;
    let storage = paths.root.join("deployments");
    // Every instruction supervisor inherits this lock. Acquiring it excludes
    // surviving children before an operator releases uncertain ownership.
    let _lock = paths::lock(&storage.join("deployment.lock"), false)?;
    let run = index(paths)?
        .get(request)
        .cloned()
        .context("deployment request not found")?;
    let path = operation_path(paths, &run)?;
    let mut operation: Operation = serde_json::from_slice(&fs::read(&path)?)?;
    ensure!(
        operation.result.request_id == request && operation.result.run_id == run,
        "deployment acknowledgement owner differs"
    );
    reconcile_record(paths, &mut operation)?;
    if matches!(
        operation.result.state,
        DeploymentState::Running | DeploymentState::Interrupted
    ) && !operation.result.acknowledged
    {
        let marker: Value = serde_json::from_slice(&fs::read(storage.join("active.json"))?)?;
        ensure!(
            marker["request_id"] == request && marker["run_id"] == run,
            "active deployment owner differs"
        );
        operation.result.state = DeploymentState::Interrupted;
        operation.result.acknowledged = true;
        operation.result.diagnostic=Some("uncertain effects explicitly acknowledged; remaining instructions were not repeated and product recovery was not inferred".into());
    }
    paths::atomic_json(&path, &operation)?;
    clear_settled_marker(paths, &operation.result)?;
    Ok(operation.result)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn only_normal_compiler_failures_request_source_repair() -> Result<()> {
        let mut result = ProcessResult {
            exit_code: Some(1),
            stdout: String::new(),
            stderr: "compiler diagnostic".into(),
            timed_out: false,
            output_error: None,
            signal: None,
            start_error: None,
        };
        assert!(
            require_compilation(&result)
                .err()
                .context("expected compiler failure")?
                .downcast_ref::<PrepareError>()
                .is_some()
        );
        for observation in ["start", "output", "signal", "timeout"] {
            let mut infrastructure = result.clone();
            match observation {
                "start" => infrastructure.start_error = Some("not started".into()),
                "output" => infrastructure.output_error = Some("capture failed".into()),
                "signal" => infrastructure.signal = Some(9),
                "timeout" => infrastructure.timed_out = true,
                _ => unreachable!(),
            }
            assert!(
                require_compilation(&infrastructure)
                    .err()
                    .context("expected infrastructure failure")?
                    .downcast_ref::<PrepareError>()
                    .is_none()
            );
        }
        result.exit_code = Some(0);
        require_compilation(&result)
    }

    #[test]
    fn preparation_staging_is_private_under_ordinary_umask() -> Result<()> {
        const CHILD: &str = "TELETE_TEST_PREPARATION_UMASK_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new("/bin/sh")
                .args([
                    "-c",
                    "umask 022; exec \"$1\" --exact deployment::tests::preparation_staging_is_private_under_ordinary_umask --nocapture",
                    "telete-staging-test",
                ])
                .arg(std::env::current_exe()?)
                .env(CHILD, "1")
                .output()?;
            ensure!(
                output.status.success(),
                "staging child failed: {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return Ok(());
        }

        let owner = tempfile::tempdir()?;
        let ordinary = owner.path().join("ordinary");
        fs::create_dir(&ordinary)?;
        assert_eq!(fs::metadata(&ordinary)?.permissions().mode() & 0o777, 0o755);
        let parent = owner.path().join("production");
        paths::ensure_private(&parent)?;
        let temporary = preparation_staging(&parent)?;
        assert_eq!(
            fs::metadata(temporary.path())?.permissions().mode() & 0o777,
            0o700
        );
        paths::ensure_private(&temporary.path().join("candidates"))?;
        let receipt = json!({"schema":1,"release_check":true});
        paths::atomic_json(&temporary.path().join("result.json"), &receipt)?;
        let output = parent.join("production-0");
        fs::rename(temporary.keep(), &output)?;
        paths::ensure_private(&output)?;
        assert_eq!(
            fs::metadata(output.join("result.json"))?
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(output.join("result.json"))?)?,
            receipt
        );
        Ok(())
    }

    #[test]
    fn manifest_requires_known_fields_unique_ids_and_literal_commands() {
        let valid = r#"{"schema":1,"product":"email","order":4,"steps":[{"id":"install","kind":"run","argv":["{candidate_dir}/bin/email-install","deploy"],"stdin":"deployment_request"}]}"#;
        let value: Declaration = serde_json::from_str(valid).unwrap();
        assert!(value.validate("email").is_ok());
        assert!(value.validate("nucleus").is_err());
        let mut duplicate = value.clone();
        duplicate.steps.push(duplicate.steps[0].clone());
        assert!(duplicate.validate("email").is_err());
        assert!(
            serde_json::from_str::<Declaration>(
                &valid.replace("\"order\":4", "\"unexpected\":4,\"order\":4")
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<Instruction>(r#"{"kind":"run","id":"x","argv":[]}"#)
                .unwrap()
                .validate()
                .is_err()
        );
    }
    #[test]
    fn expansion_preserves_literals_and_refuses_unknown_or_unbalanced_variables() {
        let context = BTreeMap::from([("source_root".into(), "/source with spaces".into())]);
        assert_eq!(
            expand("{source_root}/a {{literal}}", &context).unwrap(),
            "/source with spaces/a {literal}"
        );
        assert!(expand("{unknown}", &context).is_err());
        assert!(expand("{source_root", &context).is_err());
        assert!(expand("x}", &context).is_err());
        assert!(absolute("relative", &context).is_err());
    }
    #[test]
    fn metadata_matches_executable_target_instead_of_release_unit_name() {
        let metadata = json!({"packages":[{"name":"nucleus-cli","version":"1.0.0","targets":[{"name":"nucleus","kind":["bin"]}]}]});
        let product = Product {
            id: ProductId::new("nucleus").unwrap(),
            name: "Nucleus".into(),
            root: "infrastructure/nucleus".into(),
            aliases: vec![],
            packages: vec![crate::model::PackageId::new("nucleus-cli").unwrap()],
            offline: false,
            providers: vec![],
            binaries: vec![crate::inventory::ReleaseBinary {
                unit: "nucleus".into(),
                source: "target/release/nucleus".into(),
                name: "nucleus".into(),
            }],
            fields: BTreeMap::new(),
        };
        assert_eq!(
            binary_metadata(&metadata, std::slice::from_ref(&product)).unwrap()["nucleus"],
            ("nucleus-cli".into(), "nucleus 1.0.0".into())
        );
        let ambiguous =
            json!({"packages":[metadata["packages"][0].clone(),metadata["packages"][0].clone()]});
        assert!(binary_metadata(&ambiguous, std::slice::from_ref(&product)).is_err());
        assert!(binary_metadata(&metadata, &[product.clone(), product]).is_err());
    }
    fn email_preparation() -> Preparation {
        let source = CommitId::new("a".repeat(40)).unwrap();
        let policy = SigningPolicy {
            schema: 1,
            macos: signing::MacSigning {
                profile: "local".into(),
                certificate_sha1: "a".repeat(40),
                keychain: "/keychain".into(),
                identifier_namespace: "local.cell".into(),
            },
        };
        let candidate = Candidate {
            schema: 1,
            candidate_id: "uuid:00000000000000000000000000000001".into(),
            source_commit: source.clone(),
            source_key: source.to_string(),
            product: "email".into(),
            candidate_dir: "/workspace/candidate/email".into(),
            binaries: BTreeMap::from([(
                "email".into(),
                CandidateBinary {
                    path: "bin/email".into(),
                    code_identifier: "local.cell.email.email".into(),
                    version: "email 1.0.0".into(),
                },
            )]),
            signing_policy: policy.clone(),
        };
        Preparation {
            schema: 1,
            source,
            products: vec!["email".into()],
            signing_policy: policy,
            candidates: BTreeMap::from([("email".into(), candidate)]),
            release_check: true,
        }
    }

    #[test]
    fn preparation_correlation_rejects_scope_source_policy_and_path_substitution() {
        let prepared = email_preparation();
        assert!(validate_preparation(&prepared, &["email".into()]).is_ok());
        assert!(validate_preparation(&prepared, &["nucleus".into()]).is_err());
        let mut changed = prepared.clone();
        changed.candidates.get_mut("email").unwrap().source_key = "b".repeat(40);
        assert!(validate_preparation(&changed, &["email".into()]).is_err());
        let mut changed = prepared.clone();
        changed
            .candidates
            .get_mut("email")
            .unwrap()
            .signing_policy
            .macos
            .identifier_namespace = "local.other".into();
        assert!(validate_preparation(&changed, &["email".into()]).is_err());
        let mut changed = prepared.clone();
        changed
            .candidates
            .get_mut("email")
            .unwrap()
            .binaries
            .get_mut("email")
            .unwrap()
            .path = "../outside".into();
        assert!(validate_preparation(&changed, &["email".into()]).is_err());
        let mut changed = prepared;
        changed.products.push("email".into());
        assert!(validate_preparation(&changed, &["email".into()]).is_err());
    }

    #[test]
    fn installer_input_uses_frozen_product_and_dependency_settings() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700))?;
        let paths = Paths::for_test(temporary.path().canonicalize()?)?;
        let prepared = email_preparation();
        let instruction: Instruction = serde_json::from_value(json!({
            "id":"install", "kind":"run", "argv":["{candidate_dir}/bin/email"],
            "stdin":"deployment_request"
        }))?;
        let operation = Operation {
            request: FrozenRequest {
                repository: "/source".into(),
                products: vec!["email".into(), "nucleus".into()],
                declarations: Vec::new(),
                settings: BTreeMap::from([
                    (
                        "email".into(),
                        json!({"credential_file":"/private/email-key"}),
                    ),
                    ("nucleus".into(), json!({"codex_home":"/private/codex"})),
                ]),
                prepared: prepared.clone(),
            },
            result: DeploymentResult {
                schema: 2,
                request_id: "settings-request".into(),
                run_id: "settings-run".into(),
                source: prepared.source,
                products: vec!["email".into(), "nucleus".into()],
                state: DeploymentState::Running,
                exit_code: None,
                instructions: Vec::new(),
                diagnostic: None,
                acknowledged: false,
            },
        };
        let spec = command(&paths, &operation, "email", &instruction)?;
        let input: Request = serde_json::from_str(spec.stdin.as_deref().unwrap())?;
        assert_eq!(
            input.settings,
            operation.request.settings.get("email").cloned()
        );
        assert_eq!(input.dependency_settings, operation.request.settings);
        let retained = serde_json::to_value(&operation.request)?;
        let mut changed: FrozenRequest = serde_json::from_value(retained.clone())?;
        changed
            .settings
            .insert("email".into(), json!({"credential_file":"/different"}));
        assert_ne!(changed, operation.request);
        let mut legacy = retained;
        legacy.as_object_mut().unwrap().remove("settings");
        assert!(
            serde_json::from_value::<FrozenRequest>(legacy)?
                .settings
                .is_empty()
        );
        Ok(())
    }
    #[test]
    fn reconciliation_requires_all_instructions_and_does_not_resume_a_prefix() {
        let mut step = InstructionResult {
            id: "email:install".into(),
            product: "email".into(),
            kind: "run".into(),
            state: InstructionState::Succeeded,
            exit_code: Some(0),
            stdout: String::new(),
            stderr: String::new(),
        };
        assert_eq!(
            reconciled_state(std::slice::from_ref(&step), 1),
            DeploymentState::Succeeded
        );
        assert_eq!(
            reconciled_state(std::slice::from_ref(&step), 2),
            DeploymentState::Interrupted
        );
        step.state = InstructionState::Running;
        assert_eq!(
            reconciled_state(std::slice::from_ref(&step), 1),
            DeploymentState::Interrupted
        );
        step.state = InstructionState::Failed;
        assert_eq!(
            reconciled_state(std::slice::from_ref(&step), 2),
            DeploymentState::Failed
        );
    }
}
