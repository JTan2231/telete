use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::broker::{self, ResourceClass};
use crate::paths::Paths;
use crate::process::{self, CommandSpec, ProcessResult};

pub(crate) struct FixResult {
    pub(crate) patch: Option<PathBuf>,
    pub(crate) format_command: CommandSpec,
    pub(crate) format_result: ProcessResult,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Edit {
    path: String,
    start: usize,
    end: usize,
    replacement: String,
}

fn suggestion_groups(message: &Value, result: &mut Vec<Vec<Value>>) {
    if let Some(spans) = message.get("spans").and_then(Value::as_array) {
        let replacements: Vec<_> = spans
            .iter()
            .filter(|span| {
                span.get("suggested_replacement")
                    .is_some_and(|v| !v.is_null())
            })
            .cloned()
            .collect();
        if !replacements.is_empty()
            && replacements.iter().all(|span| {
                span.get("suggestion_applicability").and_then(Value::as_str)
                    == Some("MachineApplicable")
            })
        {
            result.push(replacements);
        }
    }
    if let Some(children) = message.get("children").and_then(Value::as_array) {
        for child in children {
            suggestion_groups(child, result);
        }
    }
}

fn relative_source(root: &Path, name: &str) -> Option<String> {
    let path = Path::new(name);
    let relative = if path.is_absolute() {
        path.strip_prefix(root).ok()?
    } else {
        path
    };
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return None;
    }
    relative.to_str().map(str::to_owned)
}

fn overlaps(left: &Edit, right: &Edit) -> bool {
    if left.path != right.path || left == right {
        return false;
    }
    if left.start == left.end {
        return right.start <= left.start && left.start <= right.end;
    }
    if right.start == right.end {
        return left.start <= right.start && right.start <= left.end;
    }
    left.start < right.end && right.start < left.end
}

fn select_edits(
    groups: &[Vec<Value>],
    root: &Path,
    originals: &BTreeMap<String, Vec<u8>>,
) -> Vec<Edit> {
    let mut accepted = Vec::new();
    let mut seen = BTreeSet::new();
    for group in groups {
        let mut proposed = BTreeSet::new();
        let valid = !group.is_empty()
            && group.iter().all(|span| {
                let Some(path) = span
                    .get("file_name")
                    .and_then(Value::as_str)
                    .and_then(|name| relative_source(root, name))
                else {
                    return false;
                };
                let Some(source) = originals.get(&path) else {
                    return false;
                };
                let Some(replacement) = span.get("suggested_replacement").and_then(Value::as_str)
                else {
                    return false;
                };
                let Some(start) = span
                    .get("byte_start")
                    .and_then(Value::as_u64)
                    .and_then(|value| usize::try_from(value).ok())
                else {
                    return false;
                };
                let Some(end) = span
                    .get("byte_end")
                    .and_then(Value::as_u64)
                    .and_then(|value| usize::try_from(value).ok())
                else {
                    return false;
                };
                let Ok(text) = std::str::from_utf8(source) else {
                    return false;
                };
                if Path::new(&path)
                    .extension()
                    .is_none_or(|extension| extension != "rs")
                    || span.get("suggestion_applicability").and_then(Value::as_str)
                        != Some("MachineApplicable")
                    || start > end
                    || end > source.len()
                    || !text.is_char_boundary(start)
                    || !text.is_char_boundary(end)
                {
                    return false;
                }
                proposed.insert(Edit {
                    path,
                    start,
                    end,
                    replacement: replacement.to_owned(),
                });
                true
            });
        if !valid || !seen.insert(proposed.clone()) {
            continue;
        }
        let proposed: Vec<_> = proposed.into_iter().collect();
        if proposed.iter().enumerate().any(|(index, edit)| {
            proposed[..index].iter().any(|other| overlaps(edit, other))
                || accepted.iter().any(|other| overlaps(edit, other))
        }) {
            continue;
        }
        for edit in proposed {
            if !accepted.contains(&edit) {
                accepted.push(edit);
            }
        }
    }
    accepted
}

fn apply_edits(originals: &BTreeMap<String, Vec<u8>>, edits: &[Edit]) -> BTreeMap<String, Vec<u8>> {
    let mut changed = BTreeMap::new();
    for path in edits.iter().map(|edit| &edit.path).collect::<BTreeSet<_>>() {
        let Some(original) = originals.get(path) else {
            continue;
        };
        let mut source = original.clone();
        let mut selected: Vec<_> = edits.iter().filter(|edit| edit.path == *path).collect();
        selected.sort_by_key(|edit| std::cmp::Reverse((edit.start, edit.end)));
        for edit in selected {
            source.splice(edit.start..edit.end, edit.replacement.bytes());
        }
        if &source != original {
            changed.insert(path.clone(), source);
        }
    }
    changed
}

fn git(paths: &Paths, repo: &Path, args: &[String]) -> Result<String> {
    let result = process::run(
        paths,
        &CommandSpec {
            program: PathBuf::from("git"),
            args: args.to_vec(),
            cwd: repo.to_owned(),
            env: paths.environment(),
            timeout_seconds: Some(120),
            stdin: None,
            confined: false,
            context: None,
        },
    )?;
    if !result.success() {
        bail!("cannot prepare Rust fix: {}", result.stderr);
    }
    Ok(result.stdout)
}

fn snapshot(paths: &Paths, root: &Path, destination: &Path) -> Result<BTreeMap<String, Vec<u8>>> {
    let listed = git(paths, root, &["ls-files".into(), "-z".into()])?;
    let mut originals = BTreeMap::new();
    for name in listed.split('\0').filter(|name| !name.is_empty()) {
        let relative = relative_source(root, name).context("tracked path is outside candidate")?;
        let source = root.join(&relative);
        let copied = destination.join(&relative);
        for parent in source
            .ancestors()
            .skip(1)
            .take_while(|parent| *parent != root)
        {
            if fs::symlink_metadata(parent)?.file_type().is_symlink() {
                bail!("tracked source has a symlink parent: {relative}");
            }
        }
        fs::create_dir_all(copied.parent().context("tracked file has no parent")?)?;
        let metadata = fs::symlink_metadata(&source)?;
        if metadata.is_file() {
            fs::copy(&source, &copied)?;
            fs::set_permissions(&copied, metadata.permissions())?;
            if Path::new(&relative)
                .extension()
                .is_some_and(|extension| extension == "rs")
            {
                originals.insert(relative, fs::read(&source)?);
            }
        } else if metadata.file_type().is_symlink() {
            let target = fs::read_link(&source)?;
            let resolved = source.canonicalize()?;
            if !resolved.starts_with(root) {
                bail!("tracked symlink escapes candidate: {relative}");
            }
            if target.is_absolute() {
                bail!("absolute tracked symlink cannot be isolated: {relative}");
            }
            #[cfg(unix)]
            std::os::unix::fs::symlink(target, copied)?;
            #[cfg(not(unix))]
            bail!("Rust fixes require a Unix filesystem");
        } else {
            bail!("tracked source is not a regular file or symlink: {relative}");
        }
    }
    Ok(originals)
}

/// Prepare a patch against a tracked snapshot. The checked candidate stays unchanged.
#[allow(clippy::too_many_lines)] // Keep snapshot, formatting and patch retention in execution order.
pub(crate) fn prepare(
    paths: &Paths,
    repo: &Path,
    packages: &[String],
    diagnostics: &str,
    output: &Path,
) -> Result<FixResult> {
    crate::paths::ensure_private(&paths.root.join("scratch"))?;
    let scratch = tempfile::Builder::new()
        .prefix("autofix-")
        .tempdir_in(paths.root.join("scratch"))?;
    let copied = scratch.path().join("b");
    fs::create_dir_all(&copied)?;
    let originals = snapshot(paths, repo, &copied)?;
    let mut groups = Vec::new();
    for line in diagnostics.lines() {
        if let Ok(value) = serde_json::from_str::<Value>(line)
            && value.get("reason").and_then(Value::as_str) == Some("compiler-message")
            && let Some(message) = value.get("message")
        {
            suggestion_groups(message, &mut groups);
        }
    }
    for (relative, bytes) in apply_edits(&originals, &select_edits(&groups, repo, &originals)) {
        fs::write(copied.join(relative), bytes)?;
    }
    let mut args = vec![
        "fmt".into(),
        "--manifest-path".into(),
        copied.join("Cargo.toml").display().to_string(),
    ];
    for package in packages {
        args.extend(["--package".into(), package.clone()]);
    }
    let command = CommandSpec {
        program: PathBuf::from("cargo"),
        args,
        cwd: copied.clone(),
        env: paths.environment(),
        timeout_seconds: None,
        stdin: None,
        confined: true,
        context: None,
    };
    let key = format!(
        "rust.autofix.format/{}",
        scratch
            .path()
            .file_name()
            .context("snapshot has no identity")?
            .to_string_lossy()
    );
    let result = broker::run(paths, &key, ResourceClass::Heavy, &command)?;
    if !result.success() {
        bail!("snapshot formatter failed: {}", result.stderr);
    }
    let mut patch = String::new();
    for (relative, original) in originals {
        let formatted = copied.join(&relative);
        let metadata = fs::symlink_metadata(&formatted)?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            bail!("formatter replaced a tracked regular file: {relative}");
        }
        if fs::read(&formatted)? == original {
            continue;
        }
        let before = scratch.path().join("a").join(&relative);
        fs::create_dir_all(before.parent().context("source has no parent")?)?;
        fs::write(&before, original)?;
        fs::set_permissions(&before, fs::metadata(repo.join(&relative))?.permissions())?;
        let result = process::run(
            paths,
            &CommandSpec {
                program: PathBuf::from("git"),
                args: vec![
                    "diff".into(),
                    "--no-index".into(),
                    "--no-prefix".into(),
                    "--binary".into(),
                    "--no-ext-diff".into(),
                    "--no-textconv".into(),
                    "--".into(),
                    format!("a/{relative}"),
                    format!("b/{relative}"),
                ],
                cwd: scratch.path().to_owned(),
                env: paths.environment(),
                timeout_seconds: Some(120),
                stdin: None,
                confined: true,
                context: None,
            },
        )?;
        if !result.exited_with(&[0, 1]) {
            bail!("cannot retain Rust fix patch: {}", result.stderr);
        }
        patch.push_str(&result.stdout);
    }
    if patch.is_empty() {
        return Ok(FixResult {
            patch: None,
            format_command: command,
            format_result: result,
        });
    }
    if output.starts_with(repo) || !output.starts_with(&paths.root) {
        bail!("fix patch must be retained outside the candidate in Telete state");
    }
    crate::paths::ensure_private(output.parent().context("patch has no parent")?)?;
    let mut temporary =
        tempfile::NamedTempFile::new_in(output.parent().context("patch has no parent")?)?;
    temporary.write_all(patch.as_bytes())?;
    temporary.as_file().sync_all()?;
    temporary.persist(output).map_err(|error| error.error)?;
    fs::File::open(output.parent().context("patch has no parent")?)?.sync_all()?;
    Ok(FixResult {
        patch: Some(output.to_owned()),
        format_command: command,
        format_result: result,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn span(start: usize, end: usize, replacement: &str) -> Value {
        json!({"file_name":"src/lib.rs", "byte_start":start, "byte_end":end,
            "suggested_replacement":replacement,"suggestion_applicability":"MachineApplicable"})
    }

    #[test]
    fn multipart_suggestion_is_rejected_as_a_whole() {
        let originals = BTreeMap::from([("src/lib.rs".into(), b"abcdef".to_vec())]);
        let groups = vec![
            vec![span(0, 1, "A"), span(30, 31, "B")],
            vec![span(2, 3, "C")],
        ];
        let edits = select_edits(&groups, Path::new("/candidate"), &originals);
        assert_eq!(edits.len(), 1);
        assert_eq!(apply_edits(&originals, &edits)["src/lib.rs"], b"abCdef");
    }

    #[test]
    fn duplicate_groups_do_not_conflict_but_overlapping_insertions_do() {
        let originals = BTreeMap::from([("src/lib.rs".into(), b"abcdef".to_vec())]);
        let groups = vec![
            vec![span(1, 3, "BC")],
            vec![span(1, 3, "BC")],
            vec![span(3, 3, "!")],
        ];
        assert_eq!(
            select_edits(&groups, Path::new("/candidate"), &originals).len(),
            1
        );
    }

    #[test]
    fn offsets_must_be_utf8_boundaries_and_paths_must_stay_tracked() {
        let originals = BTreeMap::from([("src/lib.rs".into(), "é".as_bytes().to_vec())]);
        let mut outside = span(0, 2, "x");
        outside["file_name"] = json!("../outside.rs");
        let groups = vec![vec![span(1, 2, "x")], vec![outside], vec![span(0, 2, "e")]];
        assert_eq!(
            select_edits(&groups, Path::new("/candidate"), &originals).len(),
            1
        );
    }

    #[test]
    fn suggestion_children_keep_applicability_boundaries() {
        let mut unsafe_span = span(0, 1, "x");
        unsafe_span["suggestion_applicability"] = json!("MaybeIncorrect");
        let mut groups = Vec::new();
        suggestion_groups(
            &json!({"spans":[span(0, 1, "x"), unsafe_span],
            "children":[{"spans":[span(2, 3, "y")]}]}),
            &mut groups,
        );
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0][0]["byte_start"], 2);
    }
}
