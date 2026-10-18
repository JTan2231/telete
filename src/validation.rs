use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::autofix;
use crate::broker::{self, ResourceClass};
use crate::inventory::{Inventory, Product};
use crate::model::CommitId;
use crate::paths::Paths;
use crate::process::{self, CommandSpec, ProcessResult};

const NEXTEST_VERSION: &str = "0.9.146";
const PRODUCT_INPUTS: &[&str] = &[
    "*/src/bin/*-install.rs",
    "*/src/bin/*-install/*",
    "*/src/bin/installation/*",
    "*/src/installation.rs",
    "*/src/maintenance.rs",
    "*/src/migration*.rs",
    "*/migrations/*",
    "*/schema.sql",
    "*/migration*.sql",
    "*/tests/install.rs",
    "*/tests/maintenance.rs",
    "*/tests/fixtures/schema*",
    "*/packaging/*",
    "*/release.sh",
    "*/ci.sh",
    "*/Cargo.toml",
];
const PROMPT_INPUTS: &[&str] = &[
    "infrastructure/bazaar/src/prompts.rs",
    "infrastructure/bazaar/src/prompt_import.rs",
    "infrastructure/bazaar/seed.json",
];
const PROMPT_CONSUMERS: &[&str] = &[
    "annals",
    "decisions",
    "semantics",
    "platter",
    "weaver",
    "emt",
    "conatus",
];
const COMMON_EXECUTORS: &[&str] = &[
    "pipeline/cell/check.sh",
    "infrastructure/telete/src/validation.rs",
    "infrastructure/telete/src/autofix.rs",
    "infrastructure/telete/src/inventory.rs",
    "infrastructure/telete/src/broker.rs",
    "infrastructure/telete/src/process.rs",
    "infrastructure/telete/src/paths.rs",
];

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ValidationState {
    Passed,
    Failed,
    Stale,
    Autofix,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct GateReport {
    pub(crate) name: String,
    pub(crate) class: ResourceClass,
    pub(crate) command: Vec<String>,
    pub(crate) exit_code: Option<i32>,
    pub(crate) timed_out: bool,
    pub(crate) diagnostics: String,
}

impl GateReport {
    pub(crate) fn success(&self) -> bool {
        self.exit_code == Some(0) && !self.timed_out
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ValidationReport {
    pub(crate) schema: u32,
    pub(crate) base: CommitId,
    pub(crate) candidate: CommitId,
    pub(crate) state: ValidationState,
    pub(crate) products: Vec<String>,
    pub(crate) platform_products: Vec<String>,
    pub(crate) shared_suites: Vec<String>,
    pub(crate) tests_run: bool,
    pub(crate) release_builds_deferred: bool,
    pub(crate) required_gates: Vec<String>,
    pub(crate) gates: Vec<GateReport>,
    pub(crate) diagnostics: String,
    pub(crate) autofix_patch: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
struct CargoMetadata {
    packages: Vec<CargoPackage>,
}

#[derive(Debug, Deserialize)]
struct CargoPackage {
    name: String,
    manifest_path: PathBuf,
    #[serde(default)]
    dependencies: Vec<CargoDependency>,
    #[serde(default)]
    targets: Vec<CargoTarget>,
}

#[derive(Debug, Deserialize)]
struct CargoDependency {
    name: String,
    path: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
struct CargoTarget {
    name: String,
    kind: Vec<String>,
    #[serde(default = "yes")]
    test: bool,
}
fn yes() -> bool {
    true
}

#[derive(Debug, Default, Eq, PartialEq)]
struct Selection {
    products: BTreeSet<String>,
    platform: BTreeSet<String>,
    shared: BTreeSet<String>,
}

#[derive(Debug, Default, Eq, PartialEq)]
struct TestPlan {
    targets: BTreeSet<(String, String, String)>,
}

fn matches_pattern(pattern: &str, value: &str) -> bool {
    let mut prior = vec![false; value.len() + 1];
    prior[0] = true;
    for pattern_byte in pattern.bytes() {
        let mut current = vec![false; value.len() + 1];
        if pattern_byte == b'*' {
            current[0] = prior[0];
        }
        for (index, byte) in value.bytes().enumerate() {
            current[index + 1] = if pattern_byte == b'*' {
                prior[index + 1] || current[index]
            } else {
                prior[index] && (pattern_byte == b'?' || pattern_byte == byte)
            };
        }
        prior = current;
    }
    prior[value.len()]
}

fn runtime_inputs(product: &str) -> &'static [&'static str] {
    match product {
        "bazaar" => &["infrastructure/bazaar/src/api.rs"],
        "mantic" => &["products/mantic/src/store.rs", "products/mantic/src/lib.rs"],
        "conatus" => &[
            "products/conatus/src/main.rs",
            "products/conatus/src/store.rs",
        ],
        "clew" => &[
            "products/clew/src/store.rs",
            "products/clew/src/main.rs",
            "products/clew/src/lib.rs",
            "products/clew/src/delivery.rs",
        ],
        "annals" => &[
            "products/annals/crates/annals/src/db.rs",
            "products/annals/crates/annals/src/cli.rs",
            "products/annals/crates/annals/src/main.rs",
            "products/annals/crates/annals/src/sqlite.rs",
        ],
        "nucleus" => &[
            "infrastructure/nucleus/crates/nucleus-cli/src/service.rs",
            "infrastructure/nucleus/crates/nucleus-cli/src/main.rs",
            "infrastructure/nucleus/crates/nucleus-store/src/lib.rs",
            "infrastructure/nucleus/crates/nucleus-daemon/src/lib.rs",
        ],
        "decisions" => &[
            "products/decisions/crates/decisions/src/store.rs",
            "products/decisions/crates/decisions/src/cli.rs",
            "products/decisions/crates/decisions/src/main.rs",
        ],
        "semantics" => &[
            "infrastructure/semantics/src/store.rs",
            "infrastructure/semantics/src/cli.rs",
            "infrastructure/semantics/src/main.rs",
        ],
        "platter" => &[
            "products/platter/src/main.rs",
            "products/platter/src/cli.rs",
            "products/platter/src/store.rs",
            "products/platter/src/readiness.rs",
        ],
        "paperboy" => &[
            "products/paperboy/src/main.rs",
            "products/paperboy/src/manifest.rs",
            "products/paperboy/src/schedule.rs",
            "products/paperboy/src/lib.rs",
        ],
        "weaver" => &[
            "products/weaver-narrative/src/main.rs",
            "products/weaver-narrative/src/operations.rs",
            "products/weaver-narrative/src/store.rs",
            "products/weaver-narrative/src/lib.rs",
            "products/weaver-narrative/src/schema.sql",
            "products/weaver-narrative/src/agent.rs",
        ],
        _ => &[],
    }
}

fn is_platform_target(package: &str, target: &CargoTarget) -> bool {
    matches!(package, "cell-install" | "cell-maintenance")
        || (target.kind.iter().any(|kind| kind == "bin") && target.name.ends_with("-install"))
        || (target.kind.iter().any(|kind| kind == "test")
            && matches!(target.name.as_str(), "install" | "maintenance"))
}

fn binary_kind(target: &CargoTarget) -> Option<&str> {
    if target.kind.iter().any(|kind| kind == "proc-macro") {
        return Some("proc-macro");
    }
    if target.kind.iter().any(|kind| {
        matches!(
            kind.as_str(),
            "lib" | "rlib" | "dylib" | "cdylib" | "staticlib"
        )
    }) {
        return Some("lib");
    }
    ["bin", "test", "example", "bench"]
        .into_iter()
        .find(|kind| target.kind.iter().any(|value| value == kind))
}

fn uses_local_package(product: &Product, metadata: &CargoMetadata, package: &str) -> bool {
    product.packages.iter().any(|name| {
        metadata.packages.iter().any(|consumer| {
            consumer.name == name.to_string()
                && consumer
                    .dependencies
                    .iter()
                    .any(|dependency| dependency.name == package && dependency.path.is_some())
        })
    })
}

#[allow(clippy::too_many_lines)] // Keep product ownership and shared platform rules together.
fn select(
    repo: &Path,
    inventory: &Inventory,
    metadata: &CargoMetadata,
    changes: &BTreeSet<String>,
    operational: &BTreeSet<String>,
    introduced: &BTreeSet<String>,
) -> Selection {
    let mut selection = Selection::default();
    for product in &inventory.products {
        let id = product.id.to_string();
        let descriptor = format!("pipeline/products/{id}.sh");
        for path in changes {
            if Path::new(path).starts_with(&product.root) || path == &descriptor {
                selection.products.insert(id.clone());
            }
            let owned = Path::new(path).starts_with(&product.root);
            let catalog = (path.contains("/chancery/")
                || path.contains("/chancery-")
                || path.starts_with("infrastructure/chancery/provider/"))
                && Path::new(path)
                    .extension()
                    .is_some_and(|extension| extension == "json");
            if operational.contains(path)
                && (path == &descriptor
                    || (owned
                        && (catalog
                            || PRODUCT_INPUTS
                                .iter()
                                .any(|pattern| matches_pattern(pattern, path))
                            || runtime_inputs(&id).contains(&path.as_str()))))
            {
                selection.platform.insert(id.clone());
            }
        }
        if introduced.contains(&id) {
            selection.products.insert(id.clone());
            selection.platform.insert(id);
        }
    }
    if changes
        .iter()
        .any(|path| PROMPT_INPUTS.contains(&path.as_str()))
    {
        for product in &inventory.products {
            if PROMPT_CONSUMERS.contains(&product.id.to_string().as_str()) {
                selection.products.insert(product.id.to_string());
            }
        }
    }
    // Only shared installation and maintenance inputs expand to their consumers.
    let mut affected = BTreeSet::new();
    for path in operational {
        let absolute = repo.join(path);
        let deepest = metadata
            .packages
            .iter()
            .filter(|package| {
                package
                    .manifest_path
                    .parent()
                    .is_some_and(|root| absolute.starts_with(root))
            })
            .max_by_key(|package| package.manifest_path.components().count());
        if let Some(package) = deepest {
            affected.insert(package.name.clone());
        }
    }
    for (suite, package) in [
        ("install", "cell-install"),
        ("maintenance", "cell-maintenance"),
    ] {
        if affected.contains(package) {
            selection.shared.insert(suite.into());
            for product in &inventory.products {
                if uses_local_package(product, metadata, package) {
                    selection.platform.insert(product.id.to_string());
                }
            }
        }
    }
    if changes
        .iter()
        .any(|path| COMMON_EXECUTORS.contains(&path.as_str()))
    {
        for product in &inventory.products {
            selection.products.insert(product.id.to_string());
            selection.platform.insert(product.id.to_string());
        }
        selection
            .shared
            .extend(["install".into(), "maintenance".into(), "telete".into()]);
    }
    for path in operational {
        if path.starts_with("pipeline/")
            || path.starts_with("infrastructure/telete/")
            || path == "ci.sh"
        {
            selection.shared.insert("telete".into());
        }
        if (path.contains("/chancery/")
            || path.contains("/chancery-")
            || path.starts_with("infrastructure/chancery/provider/")
            || path.starts_with("pipeline/products/"))
            && Path::new(path)
                .extension()
                .is_some_and(|extension| extension == "json")
            || (path.starts_with("pipeline/products/")
                && Path::new(path)
                    .extension()
                    .is_some_and(|extension| extension == "sh"))
            || path == "infrastructure/telete/product.sh"
            || !introduced.is_empty()
        {
            selection.shared.insert("catalog".into());
        }
    }
    selection
        .products
        .extend(selection.platform.iter().cloned());
    // Backfill shared tests after product selection so one installer edit does
    // not select every other consumer for validation and default deployment.
    for product in &inventory.products {
        if selection.platform.contains(&product.id.to_string()) {
            selection.shared.insert("install".into());
            if uses_local_package(product, metadata, "cell-maintenance") {
                selection.shared.insert("maintenance".into());
            }
        }
    }
    selection
}

fn test_plan(
    inventory: &Inventory,
    metadata: &CargoMetadata,
    selection: &Selection,
) -> Result<TestPlan> {
    let mut plan = TestPlan::default();
    for product in &inventory.products {
        let id = product.id.to_string();
        if !selection.products.contains(&id) {
            continue;
        }
        for name in &product.packages {
            let package = metadata
                .packages
                .iter()
                .find(|package| package.name == name.to_string())
                .with_context(|| format!("selected Cargo package is absent: {name}"))?;
            for target in &package.targets {
                if !target.test
                    || (is_platform_target(&package.name, target)
                        && !selection.platform.contains(&id))
                {
                    continue;
                }
                if let Some(kind) = binary_kind(target) {
                    plan.targets
                        .insert((package.name.clone(), kind.into(), target.name.clone()));
                }
            }
        }
    }
    for (suite, name) in [
        ("install", "cell-install"),
        ("maintenance", "cell-maintenance"),
        ("telete", "telete"),
    ] {
        if !selection.shared.contains(suite) {
            continue;
        }
        let package = metadata
            .packages
            .iter()
            .find(|package| package.name == name)
            .with_context(|| format!("shared Cargo package is absent: {name}"))?;
        for target in &package.targets {
            if target.test
                && let Some(kind) = binary_kind(target)
            {
                plan.targets
                    .insert((name.into(), kind.into(), target.name.clone()));
            }
        }
    }
    Ok(plan)
}

fn exact_match(value: &str) -> String {
    let mut output = String::from("=");
    for character in value.chars() {
        match character {
            '\\' | ')' | ',' => {
                output.push('\\');
                output.push(character);
            }
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            _ => output.push(character),
        }
    }
    output
}

impl TestPlan {
    fn filterset(&self) -> String {
        let expressions: Vec<_> = self
            .targets
            .iter()
            .map(|(package, kind, name)| {
                format!(
                    "(package({}) & kind({}) & binary({}))",
                    exact_match(package),
                    exact_match(kind),
                    exact_match(name)
                )
            })
            .collect();
        if expressions.is_empty() {
            "none()".into()
        } else {
            expressions.join(" | ")
        }
    }
    fn cargo_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        if self
            .targets
            .iter()
            .any(|(_, kind, _)| matches!(kind.as_str(), "lib" | "proc-macro"))
        {
            args.push("--lib".into());
        }
        for kind in ["bin", "test", "example", "bench"] {
            for name in self
                .targets
                .iter()
                .filter(|(_, target_kind, _)| target_kind == kind)
                .map(|(_, _, name)| name)
                .collect::<BTreeSet<_>>()
            {
                args.extend([format!("--{kind}"), name.clone()]);
            }
        }
        for package in self
            .targets
            .iter()
            .map(|(package, _, _)| package)
            .collect::<BTreeSet<_>>()
        {
            args.extend(["--package".into(), package.clone()]);
        }
        args
    }
}

fn spec(
    paths: &Paths,
    repo: &Path,
    program: &str,
    args: Vec<String>,
    confined: bool,
) -> CommandSpec {
    let mut env = paths.environment();
    env.insert("CARGO_BUILD_JOBS".into(), "2".into());
    CommandSpec {
        program: PathBuf::from(program),
        args,
        cwd: repo.to_owned(),
        env,
        timeout_seconds: Some(1800),
        stdin: None,
        confined,
        context: None,
    }
}

fn git(paths: &Paths, repo: &Path, args: &[&str]) -> Result<ProcessResult> {
    let mut command = spec(
        paths,
        repo,
        "git",
        args.iter().map(|arg| (*arg).to_owned()).collect(),
        false,
    );
    command.env.insert("GIT_OPTIONAL_LOCKS".into(), "0".into());
    command.timeout_seconds = Some(120);
    process::run(paths, &command)
}

fn fresh(paths: &Paths, repo: &Path, candidate: &CommitId) -> Result<bool> {
    let head = git(paths, repo, &["rev-parse", "--verify", "HEAD"])?;
    let status = git(
        paths,
        repo,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--ignore-submodules=none",
        ],
    )?;
    ensure!(
        head.success() && status.success(),
        "cannot inspect candidate source: {} {}",
        head.stderr,
        status.stderr
    );
    Ok(head.stdout.trim() == candidate.to_string() && status.stdout.is_empty())
}

fn run_gate(
    paths: &Paths,
    repo: &Path,
    report: &mut ValidationReport,
    name: &str,
    class: ResourceClass,
    command: CommandSpec,
) -> Result<ProcessResult> {
    ensure!(
        fresh(paths, repo, &report.candidate)?,
        "candidate source changed before gate {name}"
    );
    let key = format!("validation/{}/{}/{name}", report.base, report.candidate);
    let result = broker::run(paths, &key, class, &command)?;
    report.gates.push(GateReport {
        name: name.into(),
        class,
        command: std::iter::once(command.program.display().to_string())
            .chain(command.args)
            .collect(),
        exit_code: result.exit_code,
        timed_out: result.timed_out,
        diagnostics: result.diagnostics(),
    });
    ensure!(
        fresh(paths, repo, &report.candidate)?,
        "candidate source changed during gate {name}"
    );
    if !result.success() {
        report.diagnostics = format!("{name}: {}", result.diagnostics());
    }
    Ok(result)
}

fn native_gate(report: &mut ValidationReport, name: &str, result: Result<()>) -> Result<()> {
    let diagnostics = result
        .as_ref()
        .err()
        .map_or_else(String::new, |error| format!("{error:#}"));
    report.gates.push(GateReport {
        name: name.into(),
        class: ResourceClass::Light,
        command: vec![format!("telete::{name}")],
        exit_code: Some(i32::from(result.is_err())),
        timed_out: false,
        diagnostics: diagnostics.clone(),
    });
    if !diagnostics.is_empty() {
        report.diagnostics = diagnostics;
    }
    result
}

fn wrapper(product: &Product, kind: &str) -> String {
    let id = product.id.to_string();
    let depth = product.root.components().count();
    let relative = std::iter::repeat_n("..", depth)
        .collect::<Vec<_>>()
        .join("/");
    let (comment, execution) = if kind == "ci" {
        (
            "# Edit the descriptor or shared runtime, then regenerate.",
            "exec \"$CELL_ROOT/ci.sh\" \"$@\"".to_owned(),
        )
    } else {
        (
            "# This publishes a release; it is not a build command.",
            format!("exec \"$CELL_ROOT/pipeline/release.sh\" \"{id}\" \"$@\""),
        )
    };
    format!(
        "#!/bin/sh\n# Generated by pipeline/generate.sh from pipeline/products/{id}.sh.\n{comment}\nset -eu\nPRODUCT_DIR=$(CDPATH='' cd \"$(dirname \"$0\")\" && pwd)\nCELL_ROOT=$(CDPATH='' cd \"$PRODUCT_DIR/{relative}\" && pwd)\n{execution}\n"
    )
}

fn source_path(repo: &Path, relative: &str) -> Result<PathBuf> {
    crate::inventory::checked_path(repo, Path::new(relative))
}

fn packaging_condition(
    condition: &str,
    macos: bool,
    available: impl Fn(&str) -> bool,
) -> Result<bool> {
    match condition {
        "always" => Ok(true),
        "darwin" => Ok(macos),
        value if value.starts_with("darwin-if-tool:") => {
            let tool = value.trim_start_matches("darwin-if-tool:");
            ensure!(
                !tool.is_empty()
                    && tool
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte)),
                "invalid packaging tool name"
            );
            Ok(macos && available(tool))
        }
        _ => bail!("unsupported packaging condition: {condition}"),
    }
}

fn tool_available(tool: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|value| {
        std::env::split_paths(&value).any(|directory| directory.join(tool).is_file())
    })
}

fn structure(repo: &Path, inventory: &Inventory) -> Result<()> {
    ensure!(
        repo.join("Cargo.toml").is_file() && repo.join("Cargo.lock").is_file(),
        "workspace manifest and lockfile are required"
    );
    for product in &inventory.products {
        if product.id.to_string() != "telete" {
            for kind in ["ci", "release"] {
                let actual =
                    fs::read_to_string(repo.join(&product.root).join(format!("{kind}.sh")))?;
                ensure!(
                    actual == wrapper(product, kind),
                    "generated {kind} wrapper drift: {}",
                    product.id
                );
            }
        }
        for row in product
            .fields
            .get("PROVIDERS")
            .into_iter()
            .flat_map(|rows| rows.lines())
        {
            let fields: Vec<_> = row.split('|').collect();
            ensure!(fields.len() == 4, "invalid provider inventory row");
            let manifest: chancery::api::ProviderManifest =
                chancery::api::ProviderManifest::decode(&fs::read_to_string(
                    source_path(repo, fields[2])?.join("provider.json"),
                )?)?;
            let expected: usize = fields[3].parse()?;
            ensure!(
                manifest.entries.len() == expected,
                "{} entry inventory mismatch",
                fields[1]
            );
        }
    }
    Ok(())
}

fn release_version(repo: &Path, fields: &[&str]) -> Result<String> {
    ensure!(fields.len() == 6, "invalid release unit");
    let value: toml::Value = toml::from_str(&fs::read_to_string(source_path(repo, fields[3])?)?)?;
    let table = if fields[2] == "workspace-package" {
        value
            .get("workspace")
            .and_then(|workspace| workspace.get("package"))
    } else {
        value.get("package")
    }
    .context("release unit has no package declaration")?;
    let version = table
        .get("version")
        .context("release unit has no version")?;
    if let Some(version) = version.as_str() {
        return Ok(version.into());
    }
    if version.get("workspace").and_then(toml::Value::as_bool) == Some(true) {
        let root: toml::Value = toml::from_str(&fs::read_to_string(repo.join("Cargo.toml"))?)?;
        return root
            .get("workspace")
            .and_then(|workspace| workspace.get("package"))
            .and_then(|package| package.get("version"))
            .and_then(toml::Value::as_str)
            .map(str::to_owned)
            .context("workspace has no version");
    }
    bail!("unsupported release version declaration")
}

fn validate_providers(
    repo: &Path,
    products: &[Product],
    client: &chancery::api::Client,
) -> Result<()> {
    for product in products {
        let mut units = BTreeMap::new();
        for row in product
            .fields
            .get("RELEASE_UNITS")
            .into_iter()
            .flat_map(|rows| rows.lines())
        {
            let fields: Vec<_> = row.split('|').collect();
            units.insert(
                fields.first().context("empty release unit")?.to_string(),
                release_version(repo, &fields)?,
            );
        }
        for row in product
            .fields
            .get("PROVIDERS")
            .into_iter()
            .flat_map(|rows| rows.lines())
        {
            let fields: Vec<_> = row.split('|').collect();
            ensure!(fields.len() == 4, "invalid provider declaration");
            let result = client.validate(&source_path(repo, fields[2])?)?;
            ensure!(
                result.ok && result.data.valid,
                "provider {}: {:?}",
                fields[1],
                result.data.issues
            );
            let identity = result
                .data
                .provider
                .context("valid provider has no identity")?;
            ensure!(identity.id == fields[1], "provider identity mismatch");
            if let Some(version) = units.get(fields[0]) {
                ensure!(
                    &identity.release == version,
                    "provider release differs from owning unit: {}",
                    identity.id
                );
            }
            ensure!(
                result.data.entries == Some(fields[3].parse()?),
                "provider entry count mismatch: {}",
                identity.id
            );
        }
        for row in product
            .fields
            .get("RELEASE_COMPANION_MANIFESTS")
            .into_iter()
            .flat_map(|rows| rows.lines())
        {
            let fields: Vec<_> = row.split('|').collect();
            ensure!(fields.len() == 2, "invalid companion manifest");
            let version =
                release_version(repo, &[fields[0], fields[0], "package", fields[1], "", "1"])?;
            ensure!(
                units.get(fields[0]) == Some(&version),
                "companion release version mismatch: {}",
                fields[1]
            );
        }
    }
    Ok(())
}

fn catalog(paths: &Paths, repo: &Path, inventory: &Inventory, executable: &Path) -> Result<()> {
    let registry = tempfile::Builder::new()
        .prefix("catalog-")
        .tempdir_in(paths.root.join("scratch"))?;
    let mut entries = Vec::new();
    let mut ids = BTreeSet::new();
    let bundles: Vec<_> = inventory
        .products
        .iter()
        .flat_map(|product| &product.providers)
        .map(|provider| (provider.id.to_string(), repo.join(&provider.path)))
        .collect();
    for (id, bundle) in bundles {
        ensure!(ids.insert(id.clone()), "duplicate catalog provider: {id}");
        std::os::unix::fs::symlink(&bundle, registry.path().join(&id))?;
        let manifest = chancery::api::ProviderManifest::decode(&fs::read_to_string(
            bundle.join("provider.json"),
        )?)?;
        ensure!(
            manifest.schema_version >= 3 && manifest.promise_scope.is_some(),
            "provider needs normalized scope: {id}"
        );
        for entry in manifest.entries {
            let entry = chancery::api::EntryDocument::decode(
                &fs::read_to_string(bundle.join(entry))?,
                manifest.schema_version,
            )?;
            ensure!(
                entry.promise.is_some(),
                "entry needs normalized promise: {}",
                entry.id
            );
            entries.push(entry.id);
        }
    }
    let client = chancery::api::Client::new(executable).with_registry(registry.path());
    let result = client.doctor()?;
    ensure!(
        result.ok && result.data.valid,
        "integrated source catalog: {:?}",
        result.data.issues
    );
    for id in entries {
        let result = client.resolve(&id, None, None, &[])?;
        ensure!(
            result.data.dependency_closure_status == "complete" && result.data.issues.is_empty(),
            "incomplete source catalog entry: {id}"
        );
        ensure!(
            !result.data.gaps.iter().any(|gap| matches!(
                gap.code.as_str(),
                "provider_scope_undeclared" | "provider_inventory_partial" | "facet_undeclared"
            )),
            "undeclared source promise: {id}"
        );
    }
    Ok(())
}

fn operational_change(paths: &Paths, repo: &Path, base: &CommitId, path: &str) -> Result<bool> {
    if Path::new(path)
        .extension()
        .is_some_and(|extension| extension == "md" || extension == "txt")
    {
        return Ok(false);
    }
    let previous = git(paths, repo, &["show", &format!("{base}:{path}")])?;
    if !previous.success() {
        return Ok(true);
    }
    let Ok(current) = fs::read_to_string(repo.join(path)) else {
        return Ok(true);
    };
    if path.ends_with("Cargo.toml") {
        let before = toml::from_str::<toml::Value>(&previous.stdout);
        let after = toml::from_str::<toml::Value>(&current);
        if let (Ok(mut before), Ok(mut after)) = (before, after) {
            for value in [&mut before, &mut after] {
                if let Some(package) = value.get_mut("package").and_then(toml::Value::as_table_mut)
                {
                    package.remove("version");
                }
                if let Some(package) = value
                    .get_mut("workspace")
                    .and_then(|v| v.get_mut("package"))
                    .and_then(toml::Value::as_table_mut)
                {
                    package.remove("version");
                }
            }
            return Ok(before != after);
        }
    }
    if path.ends_with("/provider.json")
        && let (Ok(mut before), Ok(mut after)) = (
            serde_json::from_str::<serde_json::Value>(&previous.stdout),
            serde_json::from_str::<serde_json::Value>(&current),
        )
    {
        for value in [&mut before, &mut after] {
            if let Some(provider) = value
                .get_mut("provider")
                .and_then(serde_json::Value::as_object_mut)
            {
                provider.remove("release");
            }
            if let Some(object) = value.as_object_mut() {
                object.remove("release");
            }
        }
        return Ok(before != after);
    }
    if path.starts_with("pipeline/products/") {
        let assignments = |text: &str| {
            text.lines()
                .map(str::trim)
                .filter(|line| !line.is_empty() && !line.starts_with('#'))
                .map(str::to_owned)
                .collect::<Vec<_>>()
        };
        return Ok(assignments(&previous.stdout) != assignments(&current));
    }
    Ok(true)
}

fn packages(inventory: &Inventory, selection: &Selection) -> Vec<String> {
    let mut result: BTreeSet<_> = inventory
        .products
        .iter()
        .filter(|product| selection.products.contains(&product.id.to_string()))
        .flat_map(|product| product.packages.iter().map(ToString::to_string))
        .collect();
    for (suite, package) in [
        ("install", "cell-install"),
        ("maintenance", "cell-maintenance"),
        ("telete", "telete"),
    ] {
        if selection.shared.contains(suite) {
            result.insert(package.into());
        }
    }
    result.into_iter().collect()
}

fn cargo_command(
    paths: &Paths,
    repo: &Path,
    operation: &str,
    packages: &[String],
    offline: bool,
) -> CommandSpec {
    let mut args = vec![
        operation.into(),
        "--manifest-path".into(),
        repo.join("Cargo.toml").display().to_string(),
    ];
    for package in packages {
        args.extend(["--package".into(), package.clone()]);
    }
    args.push("--locked".into());
    if offline {
        args.push("--offline".into());
    }
    let mut command = spec(paths, repo, "cargo", args, true);
    command.timeout_seconds = None;
    command
        .env
        .insert("CARGO_BUILD_WARNINGS".into(), "deny".into());
    if offline {
        command
            .env
            .insert("CARGO_NET_OFFLINE".into(), "true".into());
    }
    command
}

#[allow(clippy::too_many_lines)] // One ordered validation workflow has explicit early stop boundaries.
fn validate_inner(
    paths: &Paths,
    repo: &Path,
    report: &mut ValidationReport,
    run_tests: bool,
) -> Result<()> {
    let inventory = Inventory::load(repo)?;
    report.required_gates.extend([
        "structure".into(),
        "recognition".into(),
        "cargo.metadata".into(),
    ]);
    native_gate(report, "structure", structure(repo, &inventory))?;
    native_gate(
        report,
        "recognition",
        (|| {
            let recognized = usher::api::inspect(repo, None).map_err(anyhow::Error::msg)?;
            ensure!(
                recognized.incomplete == 0,
                "incomplete repository introductions: {}",
                serde_json::to_string(&recognized)?
            );
            ensure!(
                !recognized.products.iter().any(|product| product
                    .semantics
                    .identities
                    .iter()
                    .any(|id| id == "telete")),
                "another product claims the Telete semantic project"
            );
            let marker = fs::read_to_string(repo.join("infrastructure/telete/AGENTS.md"))?;
            ensure!(
                marker
                    .lines()
                    .filter(|line| *line == "Semantics-Project: telete")
                    .count()
                    == 1,
                "invalid Telete Semantics marker"
            );
            Ok(())
        })(),
    )?;
    let mut metadata_command = spec(
        paths,
        repo,
        "cargo",
        vec![
            "metadata".into(),
            "--manifest-path".into(),
            repo.join("Cargo.toml").display().to_string(),
            "--locked".into(),
            "--no-deps".into(),
            "--format-version".into(),
            "1".into(),
        ],
        true,
    );
    metadata_command.timeout_seconds = None;
    let metadata_result = run_gate(
        paths,
        repo,
        report,
        "cargo.metadata",
        ResourceClass::Heavy,
        metadata_command,
    )?;
    ensure!(metadata_result.success(), "Cargo metadata failed");
    let metadata: CargoMetadata = serde_json::from_str(&metadata_result.stdout)?;
    let diff_result = git(
        paths,
        repo,
        &[
            "diff",
            "--name-only",
            "-z",
            "--no-renames",
            &report.base.to_string(),
            &report.candidate.to_string(),
            "--",
        ],
    )?;
    ensure!(
        diff_result.success(),
        "cannot compare committed trees: {}",
        diff_result.stderr
    );
    let changes: BTreeSet<_> = diff_result
        .stdout
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
        .collect();
    let mut operational = BTreeSet::new();
    for path in &changes {
        if operational_change(paths, repo, &report.base, path)? {
            operational.insert(path.clone());
        }
    }
    let mut introduced = BTreeSet::new();
    for product in &inventory.products {
        let descriptor = if product.id.to_string() == "telete" {
            "infrastructure/telete/product.sh".to_owned()
        } else {
            format!("pipeline/products/{}.sh", product.id)
        };
        if !git(
            paths,
            repo,
            &["cat-file", "-e", &format!("{}:{descriptor}", report.base)],
        )?
        .success()
        {
            introduced.insert(product.id.to_string());
        }
    }
    let selection = select(
        repo,
        &inventory,
        &metadata,
        &changes,
        &operational,
        &introduced,
    );
    report.products = selection.products.iter().cloned().collect();
    report.platform_products = selection.platform.iter().cloned().collect();
    report.shared_suites = selection.shared.iter().cloned().collect();
    let selected: Vec<_> = inventory
        .products
        .iter()
        .filter(|product| selection.products.contains(&product.id.to_string()))
        .cloned()
        .collect();
    let package_scope = packages(&inventory, &selection);
    let offline = selected.iter().any(|product| product.offline);
    let tests = test_plan(&inventory, &metadata, &selection)?;
    let mut packaging = Vec::new();
    for product in &selected {
        for (index, row) in product
            .fields
            .get("CI_SHELL_CHECKS")
            .into_iter()
            .flat_map(|rows| rows.lines())
            .enumerate()
        {
            let fields: Vec<_> = row.split('|').collect();
            ensure!(
                fields.len() == 2 && matches!(fields[0], "sh" | "zsh"),
                "invalid shell check"
            );
            let name = format!("{}.shell.{index}", product.id);
            packaging.push((
                name.clone(),
                spec(
                    paths,
                    repo,
                    fields[0],
                    vec![
                        "-n".into(),
                        source_path(repo, fields[1])?.display().to_string(),
                    ],
                    true,
                ),
            ));
        }
        for (index, row) in product
            .fields
            .get("CI_PLIST_CHECKS")
            .into_iter()
            .flat_map(|rows| rows.lines())
            .enumerate()
        {
            let fields: Vec<_> = row.split('|').collect();
            ensure!(fields.len() == 3, "invalid packaging check");
            if !packaging_condition(fields[0], cfg!(target_os = "macos"), tool_available)? {
                continue;
            }
            let packaging_file = source_path(repo, fields[2])?;
            let args = match fields[1] {
                "lint" => vec!["-lint".into(), packaging_file.display().to_string()],
                "convert" => vec![
                    "-convert".into(),
                    "binary1".into(),
                    "-o".into(),
                    "/dev/null".into(),
                    "--".into(),
                    packaging_file.display().to_string(),
                ],
                _ => bail!("unsupported packaging mode"),
            };
            packaging.push((
                format!("{}.packaging.{index}", product.id),
                spec(paths, repo, "plutil", args, true),
            ));
        }
    }
    report
        .required_gates
        .extend(packaging.iter().map(|(name, _)| name.clone()));
    let providers_needed = selected.iter().any(|product| !product.providers.is_empty())
        || selection.shared.contains("catalog");
    if providers_needed {
        report
            .required_gates
            .extend(["chancery.build".into(), "providers".into()]);
    }
    if selection.shared.contains("catalog") {
        report.required_gates.push("catalog".into());
    }
    if !package_scope.is_empty() {
        report.required_gates.extend([
            "rust.toolchain".into(),
            "cargo.toolchain".into(),
            "rust.clippy".into(),
            "rust.format".into(),
        ]);
        if run_tests {
            report.required_gates.push("rust.tests".into());
        }
        if !report.release_builds_deferred {
            report.required_gates.push("rust.release".into());
        }
    }
    for (name, command) in packaging {
        ensure!(
            run_gate(paths, repo, report, &name, ResourceClass::Light, command)?.success(),
            "shell or packaging check failed: {name}"
        );
    }
    if providers_needed {
        let tool_target = paths
            .targets()
            .join("candidate-tools")
            .join(report.candidate.to_string());
        let mut command = cargo_command(paths, repo, "build", &["chancery".into()], offline);
        command.args.extend(["--bin".into(), "chancery".into()]);
        command
            .env
            .insert("CARGO_TARGET_DIR".into(), tool_target.display().to_string());
        ensure!(
            run_gate(
                paths,
                repo,
                report,
                "chancery.build",
                ResourceClass::Heavy,
                command
            )?
            .success(),
            "candidate Chancery build failed"
        );
        let executable = tool_target.join("debug/chancery");
        native_gate(
            report,
            "providers",
            validate_providers(repo, &selected, &chancery::api::Client::new(&executable)),
        )?;
        if selection.shared.contains("catalog") {
            native_gate(
                report,
                "catalog",
                catalog(paths, repo, &inventory, &executable),
            )?;
        }
    }
    if package_scope.is_empty() {
        report.tests_run = run_tests;
        return Ok(());
    }
    for (gate, tool) in [("rust.toolchain", "rustc"), ("cargo.toolchain", "cargo")] {
        let result = run_gate(
            paths,
            repo,
            report,
            gate,
            ResourceClass::Light,
            spec(paths, repo, tool, vec!["--version".into()], true),
        )?;
        ensure!(
            result.success() && result.stdout.starts_with(&format!("{tool} 1.97.1 ")),
            "pinned {tool} 1.97.1 is required"
        );
    }
    let mut clippy = cargo_command(paths, repo, "clippy", &package_scope, offline);
    clippy
        .args
        .extend(["--all-targets".into(), "--message-format=json".into()]);
    if selected.iter().any(|product| {
        product
            .fields
            .get("CLIPPY_KEEP_GOING")
            .is_some_and(|value| value == "1")
    }) {
        clippy.args.push("--keep-going".into());
    }
    clippy.args.extend(
        [
            "--",
            "-F",
            "unsafe_code",
            "-D",
            "clippy::all",
            "-D",
            "clippy::pedantic",
            "-D",
            "clippy::dbg_macro",
            "-D",
            "clippy::todo",
            "-D",
            "clippy::unimplemented",
            "-D",
            "clippy::unwrap_used",
            "-D",
            "clippy::expect_used",
        ]
        .map(str::to_owned),
    );
    let result = run_gate(
        paths,
        repo,
        report,
        "rust.clippy",
        ResourceClass::Heavy,
        clippy,
    )?;
    let output = receipt_directory(paths, &report.base, &report.candidate)?.join("autofix.patch");
    let fix = autofix::prepare(paths, repo, &package_scope, &result.stdout, &output)?;
    report.gates.push(GateReport {
        name: "rust.format".into(),
        class: ResourceClass::Heavy,
        command: std::iter::once(fix.format_command.program.display().to_string())
            .chain(fix.format_command.args)
            .collect(),
        exit_code: fix.format_result.exit_code,
        timed_out: fix.format_result.timed_out,
        diagnostics: fix.format_result.diagnostics(),
    });
    if let Some(patch) = fix.patch {
        ensure!(
            fresh(paths, repo, &report.candidate)?,
            "candidate changed during Rust fix preparation"
        );
        report.autofix_patch = Some(patch);
        report.state = ValidationState::Autofix;
        report.diagnostics = "deterministic Rust patch retained; validation is incomplete".into();
        return Ok(());
    }
    ensure!(result.success(), "strict Clippy failed");
    if run_tests && tests.targets.is_empty() {
        native_gate(report, "rust.tests", Ok(()))?;
        report.tests_run = true;
    } else if run_tests {
        let nextest = paths
            .workspace
            .join(format!("tools/nextest/{NEXTEST_VERSION}/cargo-nextest"));
        ensure!(
            fs::symlink_metadata(&nextest)
                .is_ok_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink()),
            "pinned nextest unavailable; run telete prepare-tools"
        );
        let threads = std::env::var("TELETE_TEST_THREADS")
            .unwrap_or_else(|_| "4".into())
            .parse::<usize>()?;
        ensure!(threads > 0, "TELETE_TEST_THREADS must be positive");
        let test_directory =
            receipt_directory(paths, &report.base, &report.candidate)?.join("tests");
        crate::paths::ensure_private(&test_directory)?;
        let config = test_directory.join("nextest.toml");
        let user = test_directory.join("user.toml");
        fs::write(
            &config,
            format!(
                "[store]\ndir = {}\n[profile.default.junit]\npath = \"report.xml\"\n",
                serde_json::to_string(&test_directory.join("nextest").display().to_string())?
            ),
        )?;
        fs::write(&user, "")?;
        let mut args = vec![
            "nextest".into(),
            "run".into(),
            "--manifest-path".into(),
            repo.join("Cargo.toml").display().to_string(),
            "--locked".into(),
        ];
        args.extend(tests.cargo_args());
        args.extend([
            "--config-file".into(),
            config.display().to_string(),
            "--user-config-file".into(),
            user.display().to_string(),
            "--profile".into(),
            "default".into(),
            "--ignore-default-filter".into(),
            "--filterset".into(),
            tests.filterset(),
            "--no-fail-fast".into(),
            "--retries".into(),
            "0".into(),
            "--test-threads".into(),
            threads.to_string(),
            "--no-tests".into(),
            "pass".into(),
            "--failure-output".into(),
            "final".into(),
            "--status-level".into(),
            "pass".into(),
            "--final-status-level".into(),
            "fail".into(),
            "--color".into(),
            "never".into(),
            "--no-input-handler".into(),
        ]);
        if offline {
            args.push("--offline".into());
        }
        let mut command = spec(paths, repo, &nextest.display().to_string(), args, true);
        command.timeout_seconds = None;
        command.env.retain(|key, _| !key.starts_with("NEXTEST_"));
        ensure!(
            run_gate(
                paths,
                repo,
                report,
                "rust.tests",
                ResourceClass::Heavy,
                command
            )?
            .success(),
            "selected Rust tests failed"
        );
        report.tests_run = true;
    }
    if !report.release_builds_deferred {
        let mut command = cargo_command(paths, repo, "build", &package_scope, offline);
        command.args.push("--release".into());
        ensure!(
            run_gate(
                paths,
                repo,
                report,
                "rust.release",
                ResourceClass::Heavy,
                command
            )?
            .success(),
            "selected release build failed"
        );
    }
    Ok(())
}

pub(crate) fn validate(
    paths: &Paths,
    repo: &Path,
    base: &CommitId,
    candidate: &CommitId,
    run_tests: bool,
    defer_release: bool,
) -> Result<ValidationReport> {
    let repo = repo.canonicalize()?;
    let mut report = ValidationReport {
        schema: 1,
        base: base.clone(),
        candidate: candidate.clone(),
        state: ValidationState::Failed,
        products: Vec::new(),
        platform_products: Vec::new(),
        shared_suites: Vec::new(),
        tests_run: false,
        release_builds_deferred: defer_release,
        required_gates: Vec::new(),
        gates: Vec::new(),
        diagnostics: String::new(),
        autofix_patch: None,
    };
    if !fresh(paths, &repo, candidate)? {
        report.state = ValidationState::Stale;
        report.diagnostics = "candidate HEAD, index or worktree is stale".into();
        return Ok(report);
    }
    crate::paths::ensure_private(&paths.root.join("scratch"))?;
    match validate_inner(paths, &repo, &mut report, run_tests) {
        Ok(()) if report.state != ValidationState::Autofix => {
            ensure!(
                report.required_gates.iter().all(|name| report
                    .gates
                    .iter()
                    .any(|gate| &gate.name == name && gate.success())),
                "validation omitted a required gate"
            );
            report.state = ValidationState::Passed;
            report.diagnostics.clear();
        }
        Ok(()) => {}
        Err(error) => {
            report.state = ValidationState::Failed;
            report.diagnostics = format!("{}\n{error:#}", report.diagnostics);
        }
    }
    if !fresh(paths, &repo, candidate)? {
        report.state = ValidationState::Stale;
        report.diagnostics = "candidate HEAD, index or worktree changed during validation".into();
        report.autofix_patch = None;
    }
    Ok(report)
}

fn receipt_directory(paths: &Paths, base: &CommitId, candidate: &CommitId) -> Result<PathBuf> {
    let directory = paths
        .root
        .join("validation")
        .join(base.to_string())
        .join(candidate.to_string());
    crate::paths::ensure_private(&paths.root.join("validation"))?;
    crate::paths::ensure_private(
        directory
            .parent()
            .context("validation has no base directory")?,
    )?;
    crate::paths::ensure_private(&directory)?;
    Ok(directory)
}

pub(crate) fn receipt_path(
    paths: &Paths,
    base: &CommitId,
    candidate: &CommitId,
    run_tests: bool,
    defer_release: bool,
) -> PathBuf {
    paths
        .root
        .join("validation")
        .join(base.to_string())
        .join(candidate.to_string())
        .join(format!(
            "report-tests-{}-defer-{}.json",
            u8::from(run_tests),
            u8::from(defer_release)
        ))
}

pub(crate) fn verify_report(
    report: &ValidationReport,
    base: &CommitId,
    candidate: &CommitId,
    run_tests: bool,
    defer_release: bool,
) -> Result<()> {
    ensure!(
        report.schema == 1
            && report.base == *base
            && report.candidate == *candidate
            && report.release_builds_deferred == defer_release,
        "validation receipt correlation mismatch"
    );
    ensure!(
        report
            .platform_products
            .iter()
            .all(|product| report.products.contains(product)),
        "platform coverage lies outside selected products"
    );
    for values in [
        &report.products,
        &report.platform_products,
        &report.shared_suites,
        &report.required_gates,
    ] {
        ensure!(
            values.iter().all(|value| !value.is_empty())
                && values.iter().collect::<BTreeSet<_>>().len() == values.len(),
            "validation receipt has duplicate or empty identities"
        );
    }
    let actual: BTreeSet<_> = report.gates.iter().map(|gate| &gate.name).collect();
    ensure!(
        actual.len() == report.gates.len()
            && report
                .gates
                .iter()
                .all(|gate| !gate.name.is_empty() && !gate.command.is_empty()),
        "validation has duplicate or incomplete gate receipts"
    );
    if report.state == ValidationState::Autofix {
        ensure!(
            report.autofix_patch.is_some(),
            "autofix receipt has no retained patch"
        );
    }
    if report.state != ValidationState::Passed {
        return Ok(());
    }
    ensure!(
        report.autofix_patch.is_none() && report.tests_run == run_tests,
        "validation pass contradicts frozen tests or contains an unapplied fix"
    );
    let required: BTreeSet<_> = report.required_gates.iter().collect();
    ensure!(
        actual == required && report.gates.iter().all(GateReport::success),
        "validation pass has incomplete required gate evidence"
    );
    let require = |name: &str| -> Result<()> {
        ensure!(
            report.required_gates.iter().any(|gate| gate == name),
            "validation pass omitted {name}"
        );
        Ok(())
    };
    for name in ["structure", "recognition", "cargo.metadata"] {
        require(name)?;
    }
    let rust_scope = !report.products.is_empty()
        || report
            .shared_suites
            .iter()
            .any(|suite| matches!(suite.as_str(), "install" | "maintenance" | "telete"));
    if rust_scope {
        for name in [
            "rust.toolchain",
            "cargo.toolchain",
            "rust.clippy",
            "rust.format",
        ] {
            require(name)?;
        }
        if run_tests {
            require("rust.tests")?;
        }
        if !defer_release {
            require("rust.release")?;
        }
    }
    if !report.products.is_empty() || report.shared_suites.iter().any(|suite| suite == "catalog") {
        require("chancery.build")?;
        require("providers")?;
    }
    if report.shared_suites.iter().any(|suite| suite == "catalog") {
        require("catalog")?;
    }
    Ok(())
}

fn dispatcher_spec(
    paths: &Paths,
    repo: &Path,
    base: &CommitId,
    candidate: &CommitId,
    run_tests: bool,
    defer_release: bool,
    timeout_seconds: Option<u64>,
) -> CommandSpec {
    let output = receipt_path(paths, base, candidate, run_tests, defer_release);
    let mut args = vec![
        "--state".into(),
        paths.root.display().to_string(),
        "internal-validate".into(),
        "--repo".into(),
        repo.display().to_string(),
        "--base".into(),
        base.to_string(),
        "--candidate".into(),
        candidate.to_string(),
        "--receipt".into(),
        output.display().to_string(),
    ];
    if run_tests {
        args.push("--run-tests".into());
    }
    if defer_release {
        args.push("--defer-release-builds".into());
    }
    let program = paths
        .targets()
        .join("candidate-validator")
        .join(candidate.to_string())
        .join("debug/telete");
    let mut command = spec(paths, repo, &program.display().to_string(), args, false);
    command.timeout_seconds = timeout_seconds;
    command
}

pub(crate) fn check_dispatcher_key(
    base: &CommitId,
    candidate: &CommitId,
    run_tests: bool,
    defer_release: bool,
) -> String {
    format!(
        "validation/{base}/{candidate}/dispatcher-cell-check-v1-tests-{}-defer-{}",
        u8::from(run_tests),
        u8::from(defer_release)
    )
}

pub(crate) fn check_dispatcher_spec(
    paths: &Paths,
    repo: &Path,
    base: &CommitId,
    candidate: &CommitId,
    run_tests: bool,
    defer_release: bool,
) -> CommandSpec {
    let mut command = dispatcher_spec(paths, repo, base, candidate, run_tests, defer_release, None);
    let mut args = vec![
        repo.join("pipeline/cell/check.sh").display().to_string(),
        command.program.display().to_string(),
    ];
    args.extend(command.args);
    command.program = "/bin/sh".into();
    command.args = args;
    command
}

pub(crate) fn require_completed_dispatcher(
    paths: &Paths,
    repo: &Path,
    base: &CommitId,
    candidate: &CommitId,
    run_tests: bool,
    defer_release: bool,
) -> Result<()> {
    let key = check_dispatcher_key(base, candidate, run_tests, defer_release);
    if broker::recorded(paths, &key)? {
        let result = broker::completed(
            paths,
            &key,
            ResourceClass::Supervisor,
            &check_dispatcher_spec(paths, repo, base, candidate, run_tests, defer_release),
        )?;
        ensure!(result.success(), "Cell check dispatcher did not succeed");
        return Ok(());
    }
    // Retained jobs keep the exact pre-extraction command and gate identity.
    let unbounded = format!(
        "validation/{base}/{candidate}/dispatcher-unbounded-tests-{}-defer-{}",
        u8::from(run_tests),
        u8::from(defer_release)
    );
    let (key, timeout) = if broker::recorded(paths, &unbounded)? {
        (unbounded, None)
    } else {
        (
            format!(
                "validation/{base}/{candidate}/dispatcher-tests-{}-defer-{}",
                u8::from(run_tests),
                u8::from(defer_release)
            ),
            Some(1800),
        )
    };
    let result = broker::completed(
        paths,
        &key,
        ResourceClass::Supervisor,
        &dispatcher_spec(
            paths,
            repo,
            base,
            candidate,
            run_tests,
            defer_release,
            timeout,
        ),
    )?;
    ensure!(result.success(), "validation dispatcher did not succeed");
    Ok(())
}

pub(crate) fn job_dispatcher_spec(
    paths: &Paths,
    repo: &Path,
    job: &crate::store::Job,
) -> Result<CommandSpec> {
    let base = job.base.as_ref().context("fixed base absent")?;
    let candidate = job.candidate.as_ref().context("candidate absent")?;
    let policy = job
        .policy
        .commands
        .as_ref()
        .context("command policy absent")?;
    ensure!(
        !policy.check_script.is_absolute()
            && policy
                .check_script
                .components()
                .all(|part| matches!(part, std::path::Component::Normal(_))),
        "check script must be a normalized repository-relative path"
    );
    let mut command =
        check_dispatcher_spec(paths, repo, base, candidate, job.options.run_tests, true);
    command.args[0] = repo.join(&policy.check_script).display().to_string();
    crate::invocation::bind(
        paths,
        job,
        &format!("check-{candidate}"),
        "check",
        &serde_json::json!({"validator": command.args[1]}),
        &serde_json::json!({"report": receipt_path(paths, base, candidate, job.options.run_tests, true)}),
        command,
    )
}

pub(crate) fn job_dispatcher_key(job: &crate::store::Job) -> Result<String> {
    Ok(format!(
        "command/{}/check/{}",
        job.id,
        job.candidate.as_ref().context("candidate absent")?
    ))
}

pub(crate) fn require_job_dispatcher(
    paths: &Paths,
    repo: &Path,
    job: &crate::store::Job,
) -> Result<()> {
    if job.policy.commands.is_none() {
        return require_completed_dispatcher(
            paths,
            repo,
            job.base.as_ref().context("fixed base absent")?,
            job.candidate.as_ref().context("candidate absent")?,
            job.options.run_tests,
            true,
        );
    }
    let key = job_dispatcher_key(job)?;
    let saved = broker::request(paths, &key, ResourceClass::Supervisor)?;
    // This comparison uses only the admitted job policy and source identities;
    // no current environment or executable selection participates in recovery.
    ensure!(
        serde_json::to_value(&saved)?
            == serde_json::to_value(job_dispatcher_spec(paths, repo, job)?)?,
        "retained check invocation does not match its job"
    );
    let result = broker::completed(paths, &key, ResourceClass::Supervisor, &saved)?;
    ensure!(result.success(), "Cell check dispatcher did not succeed");
    Ok(())
}

pub(crate) fn require_timed_out_dispatcher(
    paths: &Paths,
    repo: &Path,
    base: &CommitId,
    candidate: &CommitId,
    run_tests: bool,
    defer_release: bool,
) -> Result<()> {
    let result = broker::completed(
        paths,
        &format!(
            "validation/{base}/{candidate}/dispatcher-tests-{}-defer-{}",
            u8::from(run_tests),
            u8::from(defer_release)
        ),
        ResourceClass::Supervisor,
        &dispatcher_spec(
            paths,
            repo,
            base,
            candidate,
            run_tests,
            defer_release,
            Some(1800),
        ),
    )?;
    ensure!(result.timed_out, "validation dispatcher did not time out");
    Ok(())
}

pub(crate) fn job_bootstrap_key(job: &crate::store::Job) -> Result<String> {
    let candidate = job.candidate.as_ref().context("candidate absent")?;
    Ok(if job.policy.commands.is_some() {
        format!("command/{}/bootstrap/{candidate}", job.id)
    } else {
        let base = job.base.as_ref().context("fixed base absent")?;
        format!("validation/{base}/{candidate}/bootstrap")
    })
}

pub(crate) fn job_bootstrap_spec(
    paths: &Paths,
    repo: &Path,
    job: &crate::store::Job,
) -> Result<CommandSpec> {
    let candidate = job.candidate.as_ref().context("candidate absent")?;
    let target = paths
        .targets()
        .join("candidate-validator")
        .join(candidate.to_string());
    let mut build = cargo_command(paths, repo, "build", &["telete".into()], false);
    build.args.extend(["--bin".into(), "telete".into()]);
    build
        .env
        .insert("CARGO_TARGET_DIR".into(), target.display().to_string());
    if job.policy.commands.is_some() {
        build = crate::invocation::bind(
            paths,
            job,
            &format!("bootstrap-{candidate}"),
            "bootstrap",
            &serde_json::json!({}),
            &serde_json::json!({"target": target}),
            build,
        )?;
        build
            .env
            .insert("CARGO_TARGET_DIR".into(), target.display().to_string());
    }
    Ok(build)
}

fn require_no_dispatcher(paths: &Paths, job: &crate::store::Job) -> Result<()> {
    let base = job.base.as_ref().context("fixed base absent")?;
    let candidate = job.candidate.as_ref().context("candidate absent")?;
    let context = crate::invocation::directory(paths, job, &format!("check-{candidate}"))?;
    // Context materialization happens only after the dispatcher is admitted.
    // Its directory remains evidence even if broker records were lost.
    match fs::symlink_metadata(context) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("inspect dispatcher context"),
        Ok(_) => bail!("validation dispatcher context exists"),
    }
    let mut keys = vec![job_dispatcher_key(job)?];
    for run_tests in [false, true] {
        for defer_release in [false, true] {
            keys.push(check_dispatcher_key(
                base,
                candidate,
                run_tests,
                defer_release,
            ));
            for prefix in ["", "unbounded-"] {
                keys.push(format!(
                    "validation/{base}/{candidate}/dispatcher-{prefix}tests-{}-defer-{}",
                    u8::from(run_tests),
                    u8::from(defer_release)
                ));
            }
        }
    }
    for key in keys {
        // These keys contain only identity characters and slashes, so this is
        // the broker's exact record stem. Orphan process evidence also fences
        // abandonment, even when its execution record is unavailable.
        let stem = key.replace('/', "_");
        for extension in ["json", "process"] {
            let path = paths
                .root
                .join("broker")
                .join(format!("{stem}.{extension}"));
            match fs::symlink_metadata(&path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("inspect dispatcher admission"),
                Ok(_) => bail!("validation dispatcher admission exists: {key}"),
            }
        }
    }
    Ok(())
}

// The caller holds admission and settled resource ownership. This proof reads
// terminal evidence only; it never resolves or launches an executable.
pub(crate) fn require_failed_bootstrap(
    paths: &Paths,
    repo: &Path,
    job: &crate::store::Job,
) -> Result<()> {
    ensure!(
        job.options.workflow.is_none(),
        "Cell bootstrap proof requires a Cell job"
    );
    require_no_dispatcher(paths, job)?;
    let key = job_bootstrap_key(job)?;
    let saved = broker::request(paths, &key, ResourceClass::Heavy)?;
    let mut expected = job_bootstrap_spec(paths, repo, job)?;
    if job.policy.commands.is_some() {
        ensure!(
            saved.program.is_absolute()
                && std::env::split_paths(
                    expected.env.get("PATH").context("bootstrap PATH absent")?
                )
                .any(
                    |directory| directory.is_absolute() && directory.join("cargo") == saved.program
                ),
            "retained bootstrap executable is outside its frozen PATH"
        );
        expected.program.clone_from(&saved.program);
    }
    ensure!(
        serde_json::to_value(&saved)? == serde_json::to_value(&expected)?,
        "retained bootstrap invocation does not match its job"
    );
    let result = broker::completed(paths, &key, ResourceClass::Heavy, &saved)?;
    let code = result
        .exit_code
        .context("bootstrap has no normal exit status")?;
    ensure!(
        code > 0 && result.exited_with(&[code]),
        "bootstrap has no ordinary failed completion"
    );
    Ok(())
}

pub(crate) fn run_candidate(
    paths: &Paths,
    repo: &Path,
    base: &CommitId,
    candidate: &CommitId,
    run_tests: bool,
    defer_release: bool,
    job: &crate::store::Job,
) -> Result<ValidationReport> {
    ensure!(
        job.base.as_ref() == Some(base) && job.candidate.as_ref() == Some(candidate),
        "validation source differs from its job"
    );
    ensure!(
        fresh(paths, repo, candidate)?,
        "candidate bootstrap requires clean exact source"
    );
    let check = if let Some(policy) = &job.policy.commands {
        crate::inventory::checked_path(repo, &policy.check_script)?
    } else {
        repo.join("pipeline/cell/check.sh")
    };
    ensure!(
        fs::symlink_metadata(check)?.is_file(),
        "candidate Cell check entry point must be a regular file"
    );
    receipt_directory(paths, base, candidate)?;
    let mut build = job_bootstrap_spec(paths, repo, job)?;
    if job.policy.commands.is_some() {
        // Resolve the trusted Cargo command once, before retaining the request.
        build.program = crate::invocation::executable(&build.program, &build.env)?;
    }
    let bootstrap_key = job_bootstrap_key(job)?;
    let bootstrap = broker::run(paths, &bootstrap_key, ResourceClass::Heavy, &build)?;
    ensure!(
        bootstrap.success(),
        "candidate Rust validator bootstrap failed: {}{}",
        bootstrap.stdout,
        bootstrap.stderr
    );
    ensure!(
        fresh(paths, repo, candidate)?,
        "candidate changed during Rust validator bootstrap"
    );
    let output = receipt_path(paths, base, candidate, run_tests, defer_release);
    // Dispatch supervision retains process completion without holding a
    // compiler or light slot while the child admits its actual gates.
    let (key, command) = if job.policy.commands.is_some() {
        (
            job_dispatcher_key(job)?,
            job_dispatcher_spec(paths, repo, job)?,
        )
    } else {
        (
            check_dispatcher_key(base, candidate, run_tests, defer_release),
            check_dispatcher_spec(paths, repo, base, candidate, run_tests, defer_release),
        )
    };
    let result = broker::run(paths, &key, ResourceClass::Supervisor, &command)?;
    ensure!(
        result.success(),
        "candidate validator has no successful completed process result: {}{}",
        result.stdout,
        result.stderr
    );
    let report: ValidationReport = serde_json::from_slice(
        &fs::read(&output).context("candidate validator has no terminal receipt")?,
    )?;
    verify_report(&report, base, candidate, run_tests, defer_release)?;
    ensure!(
        report.state != ValidationState::Passed
            || (result.success()
                && report.tests_run == run_tests
                && report.required_gates.iter().all(|name| report
                    .gates
                    .iter()
                    .any(|gate| gate.name == *name && gate.success()))),
        "candidate validator pass has incomplete evidence"
    );
    ensure!(
        fresh(paths, repo, candidate)? || report.state == ValidationState::Stale,
        "candidate changed without a stale receipt"
    );
    Ok(report)
}

pub(crate) fn prepare_tools(paths: &Paths) -> Result<PathBuf> {
    ensure!(
        cfg!(target_os = "macos"),
        "pinned nextest preparation requires macOS"
    );
    let directory = paths
        .workspace
        .join(format!("tools/nextest/{NEXTEST_VERSION}"));
    fs::create_dir_all(&directory)?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(directory.join("install.lock"))?;
    fs2::FileExt::lock_exclusive(&lock)?;
    let destination = directory.join("cargo-nextest");
    if destination.exists() {
        let metadata = fs::symlink_metadata(&destination)?;
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "invalid pinned nextest file"
        );
        return Ok(destination);
    }
    let temporary = tempfile::Builder::new()
        .prefix("install-")
        .tempdir_in(&directory)?;
    let archive = temporary.path().join("nextest.tar.gz");
    let url = format!(
        "https://github.com/nextest-rs/nextest/releases/download/cargo-nextest-{NEXTEST_VERSION}/cargo-nextest-{NEXTEST_VERSION}-universal-apple-darwin.tar.gz"
    );
    let download = process::run(
        paths,
        &spec(
            paths,
            temporary.path(),
            "curl",
            vec![
                "--fail".into(),
                "--silent".into(),
                "--show-error".into(),
                "--location".into(),
                "--max-time".into(),
                "120".into(),
                url,
                "--output".into(),
                archive.display().to_string(),
            ],
            true,
        ),
    )?;
    ensure!(
        download.success(),
        "nextest download failed: {}",
        download.stderr
    );
    let unpack = process::run(
        paths,
        &spec(
            paths,
            temporary.path(),
            "tar",
            vec![
                "-xzf".into(),
                archive.display().to_string(),
                "cargo-nextest".into(),
            ],
            true,
        ),
    )?;
    ensure!(unpack.success(), "nextest unpack failed: {}", unpack.stderr);
    let candidate = temporary.path().join("cargo-nextest");
    let metadata = fs::symlink_metadata(&candidate)?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "archive has no regular nextest executable"
    );
    fs::set_permissions(&candidate, fs::Permissions::from_mode(0o700))?;
    let version = process::run(
        paths,
        &spec(
            paths,
            temporary.path(),
            &candidate.display().to_string(),
            vec!["--version".into()],
            true,
        ),
    )?;
    ensure!(
        version.success() && version.stdout.contains(NEXTEST_VERSION),
        "pinned nextest version probe failed"
    );
    fs::rename(candidate, &destination)?;
    Ok(destination)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bootstrap_job(paths: &Paths, frozen: bool) -> Result<crate::store::Job> {
        let mut commands = crate::invocation::CommandPolicy::capture(paths)?;
        commands
            .environment
            .insert("PATH".into(), "/private/retained-tools".into());
        serde_json::from_value(json!({
            "id": "bootstrap-proof", "sequence": 1, "revision": 1,
            "request_id": "bootstrap-proof", "phase": "blocked", "cancel_requested": false,
            "created": 1, "updated": 1, "input": "a".repeat(40),
            "options": crate::manager::SubmitOptions::default(),
            "policy": {
                "commands": frozen.then_some(commands),
                "signing_policy": null
            },
            "base": "a".repeat(40), "candidate": "b".repeat(40), "accepted": false,
            "validations": [], "preparation": null, "preparation_generation": 0,
            "deployment_request": null, "deployment_result": null,
            "operation": null, "outcome": "failed", "outcome_message": "bootstrap failed",
            "stopped_phase": "checking", "unresolved": true,
            "waiting_reason": null, "last_error": null,
            "cleanup": crate::store::Cleanup::default()
        }))
        .context("bootstrap proof fixture")
    }

    fn failed_bootstrap_result() -> ProcessResult {
        ProcessResult {
            exit_code: Some(1),
            stdout: String::new(),
            stderr: "compiler rejected source".into(),
            timed_out: false,
            output_error: None,
            signal: None,
            start_error: None,
        }
    }

    fn retain_bootstrap(
        paths: &Paths,
        job: &crate::store::Job,
        command: &CommandSpec,
        result: &ProcessResult,
    ) -> Result<PathBuf> {
        let key = job_bootstrap_key(job)?;
        let record = paths
            .root
            .join("broker")
            .join(format!("{}.json", key.replace('/', "_")));
        crate::paths::atomic_json(
            &record,
            &json!({
                "schema": 1, "key": key, "class": "heavy", "command": command,
                "state": "finished", "owner_pid": 1, "result": result
            }),
        )?;
        let process = record.with_extension("process");
        crate::paths::atomic_json(&process.join("request.json"), command)?;
        crate::paths::atomic_json(
            &process.join("result.json"),
            &json!({
                "schema": 1, "request": command, "result": result
            }),
        )?;
        Ok(process)
    }

    #[test]
    fn failed_bootstrap_proof_consumes_exact_legacy_and_frozen_completions() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let paths = Paths::for_test(temporary.path().join("telete"))?;
        let repo = temporary.path().join("candidate");
        for frozen in [false, true] {
            let job = bootstrap_job(&paths, frozen)?;
            let mut command = job_bootstrap_spec(&paths, &repo, &job)?;
            if frozen {
                command.program = "/private/retained-tools/cargo".into();
            }
            let process = retain_bootstrap(&paths, &job, &command, &failed_bootstrap_result())?;
            let before = fs::read(process.join("result.json"))?;
            require_failed_bootstrap(&paths, &repo, &job)?;
            assert_eq!(fs::read(process.join("result.json"))?, before);
            assert!(
                !receipt_path(
                    &paths,
                    job.base.as_ref().context("base")?,
                    job.candidate.as_ref().context("candidate")?,
                    false,
                    true
                )
                .exists()
            );
        }
        Ok(())
    }

    #[test]
    fn failed_bootstrap_proof_rejects_incomplete_successful_and_abnormal_results() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let paths = Paths::for_test(temporary.path().join("telete"))?;
        let repo = temporary.path().join("candidate");
        let job = bootstrap_job(&paths, false)?;
        let command = job_bootstrap_spec(&paths, &repo, &job)?;
        assert!(require_failed_bootstrap(&paths, &repo, &job).is_err());
        for fault in [
            "success",
            "negative-exit",
            "missing-exit",
            "timeout",
            "signal",
            "start",
            "output",
        ] {
            let mut result = failed_bootstrap_result();
            match fault {
                "success" => result.exit_code = Some(0),
                "negative-exit" => result.exit_code = Some(-1),
                "missing-exit" => result.exit_code = None,
                "timeout" => result.timed_out = true,
                "signal" => result.signal = Some(9),
                "start" => result.start_error = Some("start failed".into()),
                "output" => result.output_error = Some("read failed".into()),
                _ => unreachable!(),
            }
            retain_bootstrap(&paths, &job, &command, &result)?;
            assert!(
                require_failed_bootstrap(&paths, &repo, &job).is_err(),
                "{fault}"
            );
        }
        let process = retain_bootstrap(&paths, &job, &command, &failed_bootstrap_result())?;
        fs::remove_file(process.join("result.json"))?;
        assert!(require_failed_bootstrap(&paths, &repo, &job).is_err());
        Ok(())
    }

    #[test]
    fn failed_bootstrap_proof_rejects_changed_commands_and_process_evidence() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let paths = Paths::for_test(temporary.path().join("telete"))?;
        let repo = temporary.path().join("candidate");
        let job = bootstrap_job(&paths, true)?;
        let mut command = job_bootstrap_spec(&paths, &repo, &job)?;
        command.program = "/private/retained-tools/cargo".into();
        for fault in ["program", "args", "env", "context", "cwd", "timeout"] {
            let mut changed = command.clone();
            match fault {
                "program" => changed.program = "/different/cargo".into(),
                "args" => changed.args.push("--changed".into()),
                "env" => {
                    changed.env.insert("EXTRA".into(), "changed".into());
                }
                "context" => {
                    changed.context.as_mut().context("context")?.value["stage"] = json!("check");
                }
                "cwd" => changed.cwd = "/different/candidate".into(),
                "timeout" => changed.timeout_seconds = Some(1),
                _ => unreachable!(),
            }
            retain_bootstrap(&paths, &job, &changed, &failed_bootstrap_result())?;
            assert!(
                require_failed_bootstrap(&paths, &repo, &job).is_err(),
                "{fault}"
            );
        }
        let process = retain_bootstrap(&paths, &job, &command, &failed_bootstrap_result())?;
        let mut changed = command.clone();
        changed.args.push("--changed".into());
        crate::paths::atomic_json(&process.join("request.json"), &changed)?;
        assert!(require_failed_bootstrap(&paths, &repo, &job).is_err());
        Ok(())
    }

    #[test]
    fn failed_bootstrap_proof_rejects_every_dispatcher_family_and_orphan_process() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let paths = Paths::for_test(temporary.path().join("telete"))?;
        let repo = temporary.path().join("candidate");
        let job = bootstrap_job(&paths, false)?;
        let base = job.base.as_ref().context("base")?;
        let candidate = job.candidate.as_ref().context("candidate")?;
        let command = job_bootstrap_spec(&paths, &repo, &job)?;
        retain_bootstrap(&paths, &job, &command, &failed_bootstrap_result())?;
        let keys = [
            job_dispatcher_key(&job)?,
            check_dispatcher_key(base, candidate, true, false),
            format!("validation/{base}/{candidate}/dispatcher-unbounded-tests-1-defer-0"),
            format!("validation/{base}/{candidate}/dispatcher-tests-1-defer-0"),
        ];
        for key in keys {
            let record = paths
                .root
                .join("broker")
                .join(format!("{}.json", key.replace('/', "_")));
            crate::paths::atomic_json(&record, &json!({"state": "queued"}))?;
            assert!(require_failed_bootstrap(&paths, &repo, &job).is_err());
            fs::remove_file(&record)?;
            let process = record.with_extension("process");
            crate::paths::ensure_private(&process)?;
            assert!(require_failed_bootstrap(&paths, &repo, &job).is_err());
            fs::remove_dir(&process)?;
            for path in [&record, &process] {
                std::os::unix::fs::symlink("missing-dispatcher", path)?;
                assert!(require_failed_bootstrap(&paths, &repo, &job).is_err());
                fs::remove_file(path)?;
            }
        }
        let context = crate::invocation::directory(&paths, &job, &format!("check-{candidate}"))?;
        crate::paths::ensure_private(&context)?;
        assert!(require_failed_bootstrap(&paths, &repo, &job).is_err());
        fs::remove_dir(&context)?;
        std::os::unix::fs::symlink("missing-dispatcher-context", &context)?;
        assert!(require_failed_bootstrap(&paths, &repo, &job).is_err());
        fs::remove_file(context)?;
        require_failed_bootstrap(&paths, &repo, &job)?;
        Ok(())
    }

    #[test]
    fn cell_check_forwards_literal_arguments_and_the_validator_exit_status() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let script = temporary.path().join("check with spaces.sh");
        fs::write(
            &script,
            include_str!("../support/cell/pipeline/cell/check.sh"),
        )?;
        let output = std::process::Command::new("/bin/sh")
            .arg(&script)
            .args(["/usr/bin/printf", "%s\\n", "two words", "$(exit 9)"])
            .output()?;
        assert!(output.status.success());
        assert_eq!(output.stdout, b"two words\n$(exit 9)\n");
        let failed = std::process::Command::new("/bin/sh")
            .arg(&script)
            .arg("/usr/bin/false")
            .status()?;
        assert_eq!(failed.code(), Some(1));
        Ok(())
    }

    #[test]
    fn cell_check_requires_its_own_correlated_process_completion() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let paths = Paths::for_test(temporary.path().join("telete"))?;
        let base = CommitId::new("a".repeat(40))?;
        let repo = temporary.path().join("candidate");
        fs::create_dir(&repo)?;
        fs::create_dir_all(repo.join("pipeline/cell"))?;
        fs::write(repo.join("pipeline/cell/check.sh"), "exit 0\n")?;
        let key = check_dispatcher_key(&base, &base, true, true);
        let command = check_dispatcher_spec(&paths, &repo, &base, &base, true, true);
        assert!(broker::run(&paths, &key, ResourceClass::Supervisor, &command)?.success());
        require_completed_dispatcher(&paths, &repo, &base, &base, true, true)?;
        // A zero exit cannot supply the separately required aggregate report.
        assert!(!receipt_path(&paths, &base, &base, true, true).exists());
        let wrong_policy = check_dispatcher_spec(&paths, &repo, &base, &base, false, true);
        assert!(broker::completed(&paths, &key, ResourceClass::Supervisor, &wrong_policy).is_err());
        Ok(())
    }

    #[test]
    fn legacy_dispatchers_keep_their_recorded_limits_and_cannot_override_a_failed_check()
    -> Result<()> {
        for timeout in [Some(1800), None] {
            let temporary = tempfile::tempdir()?;
            let paths = Paths::for_test(temporary.path().join("telete"))?;
            let base = CommitId::new("a".repeat(40))?;
            let repo = temporary.path().join("candidate");
            fs::create_dir(&repo)?;
            let command = dispatcher_spec(&paths, &repo, &base, &base, false, true, timeout);
            fs::create_dir_all(command.program.parent().context("validator parent")?)?;
            fs::write(&command.program, "#!/bin/sh\nexit 0\n")?;
            fs::set_permissions(&command.program, fs::Permissions::from_mode(0o700))?;
            let key = format!(
                "validation/{base}/{base}/dispatcher-{}tests-0-defer-1",
                if timeout.is_none() { "unbounded-" } else { "" }
            );
            assert!(broker::run(&paths, &key, ResourceClass::Supervisor, &command)?.success());
            require_completed_dispatcher(&paths, &repo, &base, &base, false, true)?;
            fs::create_dir_all(repo.join("pipeline/cell"))?;
            fs::write(repo.join("pipeline/cell/check.sh"), "exit 1\n")?;
            let check_key = check_dispatcher_key(&base, &base, false, true);
            let check = check_dispatcher_spec(&paths, &repo, &base, &base, false, true);
            assert!(!broker::run(&paths, &check_key, ResourceClass::Supervisor, &check)?.success());
            assert!(
                require_completed_dispatcher(&paths, &repo, &base, &base, false, true).is_err()
            );
        }
        Ok(())
    }

    fn product(id: &str, root: &str, packages: &[&str]) -> Product {
        Product {
            id: crate::model::ProductId::new(id).unwrap_or_else(|error| panic!("{error}")),
            name: id.into(),
            root: root.into(),
            aliases: Vec::new(),
            packages: packages
                .iter()
                .map(|name| {
                    crate::model::PackageId::new(*name).unwrap_or_else(|error| panic!("{error}"))
                })
                .collect(),
            offline: false,
            providers: Vec::new(),
            binaries: Vec::new(),
            fields: BTreeMap::new(),
        }
    }

    fn metadata() -> CargoMetadata {
        serde_json::from_value(json!({"packages":[
            {"name":"telete","manifest_path":"/repo/infrastructure/telete/Cargo.toml","targets":[{"name":"telete","kind":["bin"]}]},
            {"name":"shared-api","manifest_path":"/repo/infrastructure/api/Cargo.toml","targets":[{"name":"shared_api","kind":["lib"]}]},
            {"name":"cell-install","manifest_path":"/repo/deployment/crates/cell-install/Cargo.toml","targets":[{"name":"cell_install","kind":["lib"]}]},
            {"name":"cell-maintenance","manifest_path":"/repo/deployment/crates/cell-maintenance/Cargo.toml","targets":[{"name":"cell_maintenance","kind":["lib"]}]},
            {"name":"owner","manifest_path":"/repo/products/owner/Cargo.toml","dependencies":[{"name":"shared-api","path":"/repo/infrastructure/api"},{"name":"cell-install","path":"/repo/deployment/crates/cell-install"},{"name":"cell-maintenance","path":"/repo/deployment/crates/cell-maintenance"}],"targets":[{"name":"owner","kind":["lib"]},{"name":"owner-install","kind":["bin"]}]},
            {"name":"consumer","manifest_path":"/repo/products/consumer/Cargo.toml","dependencies":[{"name":"owner","path":"/repo/products/owner"},{"name":"cell-install","path":"/repo/deployment/crates/cell-install"}],"targets":[{"name":"consumer","kind":["bin"]},{"name":"consumer-install","kind":["bin"]}]}
        ]})).unwrap_or_else(|error| panic!("{error}"))
    }

    fn inventory() -> Inventory {
        Inventory {
            products: vec![
                product("owner", "products/owner", &["owner"]),
                product("consumer", "products/consumer", &["consumer"]),
            ],
        }
    }

    #[test]
    fn ordinary_dependencies_and_root_cargo_files_do_not_expand_product_scope() {
        let mut inventory = inventory();
        inventory
            .products
            .push(product("api", "infrastructure/api", &["shared-api"]));
        for (path, expected) in [
            ("Cargo.toml", None),
            ("Cargo.lock", None),
            ("infrastructure/api/src/lib.rs", Some("api")),
            ("products/owner/src/lib.rs", Some("owner")),
            ("products/consumer/src/lib.rs", Some("consumer")),
        ] {
            let changes = BTreeSet::from([path.into()]);
            let selected = select(
                Path::new("/repo"),
                &inventory,
                &metadata(),
                &changes,
                &changes,
                &BTreeSet::new(),
            );
            assert_eq!(
                selected.products,
                expected.into_iter().map(str::to_owned).collect(),
                "{path}"
            );
            assert!(selected.platform.is_empty(), "{path}");
            assert!(selected.shared.is_empty(), "{path}");
        }
    }

    #[test]
    fn shared_lifecycle_changes_select_only_their_declared_consumers() {
        for (path, products, suites) in [
            (
                "deployment/crates/cell-install/src/lib.rs",
                vec!["consumer", "owner"],
                vec!["install", "maintenance"],
            ),
            (
                "deployment/crates/cell-maintenance/src/lib.rs",
                vec!["owner"],
                vec!["install", "maintenance"],
            ),
        ] {
            let changes = BTreeSet::from([path.into()]);
            let selected = select(
                Path::new("/repo"),
                &inventory(),
                &metadata(),
                &changes,
                &changes,
                &BTreeSet::new(),
            );
            let expected = products.into_iter().map(str::to_owned).collect();
            assert_eq!(selected.products, expected, "{path}");
            assert_eq!(selected.platform, expected, "{path}");
            assert_eq!(
                selected.shared,
                suites.into_iter().map(str::to_owned).collect(),
                "{path}"
            );
        }
    }

    #[test]
    fn product_platform_changes_include_shared_tests_without_selecting_other_products() {
        for (name, maintenance) in [("owner", true), ("consumer", false)] {
            for path in [
                format!("products/{name}/src/bin/{name}-install.rs"),
                format!("products/{name}/schema.sql"),
                format!("products/{name}/Cargo.toml"),
                format!("pipeline/products/{name}.sh"),
            ] {
                let changes = BTreeSet::from([path.clone()]);
                let selected = select(
                    Path::new("/repo"),
                    &inventory(),
                    &metadata(),
                    &changes,
                    &changes,
                    &BTreeSet::new(),
                );
                assert_eq!(selected.products, BTreeSet::from([name.into()]), "{path}");
                assert_eq!(selected.platform, BTreeSet::from([name.into()]), "{path}");
                assert!(selected.shared.contains("install"), "{path}");
                assert_eq!(
                    selected.shared.contains("maintenance"),
                    maintenance,
                    "{path}"
                );
                let plan = test_plan(&inventory(), &metadata(), &selected)
                    .unwrap_or_else(|error| panic!("{error}"));
                assert!(
                    plan.targets
                        .contains(&(name.into(), "bin".into(), format!("{name}-install"),)),
                    "{path}"
                );
                assert!(
                    plan.targets.contains(&(
                        "cell-install".into(),
                        "lib".into(),
                        "cell_install".into(),
                    )),
                    "{path}"
                );
                assert_eq!(
                    plan.targets.contains(&(
                        "cell-maintenance".into(),
                        "lib".into(),
                        "cell_maintenance".into(),
                    )),
                    maintenance,
                    "{path}"
                );
                assert!(packages(&inventory(), &selected).contains(&"cell-install".into()));
            }
        }
    }

    #[test]
    fn unowned_shared_dependency_does_not_select_its_consumers() {
        let inventory = Inventory {
            products: vec![
                product("owner", "products/owner", &["owner"]),
                product("consumer", "products/consumer", &["consumer"]),
            ],
        };
        let changes = BTreeSet::from(["infrastructure/api/src/lib.rs".into()]);
        let selected = select(
            Path::new("/repo"),
            &inventory,
            &metadata(),
            &changes,
            &changes,
            &BTreeSet::new(),
        );
        assert!(selected.products.is_empty());
        assert!(selected.platform.is_empty());
    }

    #[test]
    fn platform_selection_keeps_installer_targets_separate() {
        let inventory = Inventory {
            products: vec![product("owner", "products/owner", &["owner"])],
        };
        let selection = Selection {
            products: BTreeSet::from(["owner".into()]),
            ..Selection::default()
        };
        let plan = test_plan(&inventory, &metadata(), &selection)
            .unwrap_or_else(|error| panic!("{error}"));
        assert!(
            plan.targets
                .contains(&("owner".into(), "lib".into(), "owner".into()))
        );
        assert!(
            !plan
                .targets
                .iter()
                .any(|(_, _, name)| name.ends_with("-install"))
        );
        let selection = Selection {
            platform: BTreeSet::from(["owner".into()]),
            ..selection
        };
        assert!(
            test_plan(&inventory, &metadata(), &selection)
                .unwrap_or_else(|error| panic!("{error}"))
                .targets
                .iter()
                .any(|(_, _, name)| name.ends_with("-install"))
        );
    }

    #[test]
    fn filterset_escapes_exact_identifiers_and_cargo_names_are_deduplicated() {
        let plan = TestPlan {
            targets: BTreeSet::from([
                ("a".into(), "test".into(), "same,)".into()),
                ("b".into(), "test".into(), "same,)".into()),
            ]),
        };
        assert_eq!(
            plan.cargo_args()
                .iter()
                .filter(|arg| *arg == "--test")
                .count(),
            1
        );
        assert!(plan.filterset().contains("binary(=same\\,\\))"));
        assert_eq!(TestPlan::default().filterset(), "none()");
    }

    #[test]
    fn wildcard_matches_nested_paths_and_new_product_has_platform_coverage() {
        assert!(matches_pattern(
            "*/src/bin/*-install.rs",
            "products/a/crates/b/src/bin/a-install.rs"
        ));
        let inventory = Inventory {
            products: vec![product("owner", "products/owner", &["owner"])],
        };
        let selection = select(
            Path::new("/repo"),
            &inventory,
            &metadata(),
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeSet::from(["owner".into()]),
        );
        assert!(selection.products.contains("owner") && selection.platform.contains("owner"));
        assert_eq!(
            selection.shared,
            BTreeSet::from(["install".into(), "maintenance".into()])
        );
        let plan = test_plan(&inventory, &metadata(), &selection)
            .unwrap_or_else(|error| panic!("{error}"));
        assert!(plan.targets.contains(&(
            "cell-install".into(),
            "lib".into(),
            "cell_install".into()
        )));
        assert!(plan.targets.contains(&(
            "cell-maintenance".into(),
            "lib".into(),
            "cell_maintenance".into()
        )));
    }

    #[test]
    fn packaging_conditions_preserve_optional_macos_tool_checks() {
        assert!(packaging_condition("always", false, |_| false).unwrap_or(false));
        assert!(!packaging_condition("darwin-if-tool:plutil", false, |_| true).unwrap_or(true));
        assert!(!packaging_condition("darwin-if-tool:plutil", true, |_| false).unwrap_or(true));
        assert!(
            packaging_condition("darwin-if-tool:plutil", true, |tool| tool == "plutil")
                .unwrap_or(false)
        );
        assert!(packaging_condition("darwin-if-tool:../../foreign", true, |_| true).is_err());
    }

    #[test]
    fn independent_source_descriptor_selects_the_new_telete_crate() {
        let temporary = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
        let repo = temporary.path();
        fs::create_dir_all(repo.join("pipeline/products"))
            .unwrap_or_else(|error| panic!("{error}"));
        fs::create_dir_all(repo.join("infrastructure/telete"))
            .unwrap_or_else(|error| panic!("{error}"));
        fs::write(repo.join("infrastructure/telete/product.sh"), "PIPELINE_SCHEMA=1\nPRODUCT_ID=telete\nPRODUCT_NAME=Telete\nPRODUCT_DIR=infrastructure/telete\nCARGO_PACKAGES=telete\nPROVIDERS='telete|telete|infrastructure/telete/chancery|1'\n")
            .unwrap_or_else(|error| panic!("{error}"));
        let inventory = Inventory::load(repo).unwrap_or_else(|error| panic!("{error}"));
        let metadata = metadata();
        let changes = BTreeSet::from(["infrastructure/telete/src/main.rs".into()]);
        let selected = select(
            repo,
            &inventory,
            &metadata,
            &changes,
            &changes,
            &BTreeSet::from(["telete".into()]),
        );
        assert!(selected.products.contains("telete") && selected.platform.contains("telete"));
        assert!(
            test_plan(&inventory, &metadata, &selected)
                .unwrap_or_else(|error| panic!("{error}"))
                .targets
                .contains(&("telete".into(), "bin".into(), "telete".into()))
        );
    }
}
