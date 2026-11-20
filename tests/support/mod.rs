use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use git2::{
    Commit, IndexAddOption, Oid, Repository, RepositoryInitOptions, Signature, WorktreeAddOptions,
};
use serde_json::Value;

pub fn init(path: &Path) -> Result<Repository> {
    Ok(Repository::init_opts(
        path,
        RepositoryInitOptions::new().initial_head("main"),
    )?)
}

pub fn commit(path: &Path, revision: &str) -> Result<Oid> {
    let repo = Repository::open(path)?;
    Ok(repo.revparse_single(revision)?.peel_to_commit()?.id())
}

fn create_commit(repo: &Repository, message: &str, parents: &[&Commit<'_>]) -> Result<Oid> {
    let tree = repo.find_tree(repo.index()?.write_tree()?)?;
    let author = Signature::now("Fixture", "fixture@localhost")?;
    Ok(repo.commit(Some("HEAD"), &author, &author, message, &tree, parents)?)
}

pub fn record(repo: &Repository, files: &[&str], message: &str) -> Result<Oid> {
    let mut index = repo.index()?;
    if files.is_empty() {
        index.add_all(["*"], IndexAddOption::DEFAULT, None)?;
    } else {
        for file in files {
            index.add_path(Path::new(file))?;
        }
    }
    index.write()?;
    match repo.head() {
        Ok(head) => create_commit(repo, message, &[&head.peel_to_commit()?]),
        Err(error) if error.code() == git2::ErrorCode::UnbornBranch => {
            create_commit(repo, message, &[])
        }
        Err(error) => Err(error.into()),
    }
}

pub fn branch(path: &Path, name: &str, revision: &str) -> Result<()> {
    let repo = Repository::open(path)?;
    let head = repo.revparse_single(revision)?.peel_to_commit()?;
    repo.branch(name, &head, false)?;
    Ok(())
}

pub fn switch(path: &Path, name: &str) -> Result<()> {
    let repo = Repository::open(path)?;
    let name = format!("refs/heads/{name}");
    let target = repo.revparse_single(&name)?;
    repo.checkout_tree(&target, None)?;
    repo.set_head(&name)?;
    Ok(())
}

pub fn worktree(repo: &Path, name: &str, path: &Path, revision: &str) -> Result<()> {
    branch(repo, name, revision)?;
    let repo = Repository::open(repo)?;
    let reference = repo.find_reference(&format!("refs/heads/{name}"))?;
    let mut options = WorktreeAddOptions::new();
    options.reference(Some(&reference));
    repo.worktree(name, path, Some(&options))?;
    Ok(())
}

pub fn remove_worktree(repo: &Path, path: &Path) -> Result<()> {
    let repo = Repository::open(repo)?;
    for name in &repo.worktrees()? {
        let worktree = repo.find_worktree(name?.context("worktree name")?)?;
        if worktree.path() == path {
            fs::remove_dir_all(path)?;
            worktree.prune(None)?;
            return Ok(());
        }
    }
    bail!("missing fixture worktree at {}", path.display())
}

pub fn contains(repo: &Repository, ancestor: Oid, tip: Oid) -> Result<bool> {
    Ok(ancestor == tip || repo.graph_descendant_of(tip, ancestor)?)
}

pub fn workflow_args() -> Vec<String> {
    [
        "--ignored",
        "--exact",
        "test_support::fixture_workflow",
        "--nocapture",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

fn context() -> Result<Value> {
    Ok(serde_json::from_slice(&fs::read(
        std::env::var_os("TELETE_CONTEXT").context("context")?,
    )?)?)
}

fn field<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key]
        .as_str()
        .with_context(|| format!("missing {key}"))
}

fn append(name: &str, line: &str) -> Result<()> {
    let path = std::env::var_os(name).with_context(|| format!("missing {name}"))?;
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(file, "{line}")?;
    Ok(())
}

fn write_composed_commit(repo: &Repository, context: &Value) -> Result<()> {
    let directory = Path::new(field(&context["outputs"], "directory")?);
    fs::write(
        directory.join("commit"),
        format!("{}\n", repo.head()?.peel_to_commit()?.id()),
    )?;
    Ok(())
}

fn compose(repo: &Repository) -> Result<()> {
    let context = context()?;
    let batch = &context["inputs"]["batch"];
    append("COMPOSED", &batch.to_string())?;
    for entry in batch["members"].as_array().context("missing members")? {
        let member = Oid::from_str(field(entry, "commit")?)?;
        let head = repo.head()?.peel_to_commit()?;
        if contains(repo, member, head.id())? {
            continue;
        }
        let annotated = repo.find_annotated_commit(member)?;
        repo.merge(&[&annotated], None, None)?;
        ensure!(
            !repo.index()?.has_conflicts(),
            "CONFLICT during fixture composition"
        );
        create_commit(
            repo,
            "Compose fixture member",
            &[&head, &repo.find_commit(member)?],
        )?;
        repo.cleanup_state()?;
    }
    write_composed_commit(repo, &context)
}

fn merge_repair(repo: &Repository) -> Result<()> {
    let context = context()?;
    let merge = &context["inputs"]["merge"];
    let head = repo.head()?.peel_to_commit()?;
    let member = repo.revparse_single("MERGE_HEAD")?.peel_to_commit()?;
    ensure!(context["stage"] == "merge_repair" && context["source"]["commit"] == merge["head"]);
    ensure!(merge["head"] == head.id().to_string() && merge["member"] == member.id().to_string());
    let mut diagnostics = String::new();
    io::stdin().read_to_string(&mut diagnostics)?;
    ensure!(diagnostics.contains("CONFLICT"), "missing merge diagnostic");
    append("REPAIRED", &merge.to_string())?;
    fs::write("payload.txt", "combined source\n")?;
    let mut index = repo.index()?;
    index.add_path(Path::new("payload.txt"))?;
    index.write()?;
    create_commit(repo, "Resolve merge", &[&head, &member])?;
    repo.cleanup_state()?;
    Ok(())
}

fn repair(repo: &Repository) -> Result<()> {
    ensure!(repo.head_detached()?);
    let mut attempts = OpenOptions::new()
        .create(true)
        .append(true)
        .open(std::env::var_os("ATTEMPTS").context("attempt log")?)?;
    attempts.write_all(b"repair")?;
    let mut diagnostics = Vec::new();
    io::stdin().read_to_end(&mut diagnostics)?;
    fs::write(
        std::env::var_os("DIAGNOSTICS").context("repair input log")?,
        &diagnostics,
    )?;
    ensure!(diagnostics == b"stdoutstderr");
    fs::write(
        "payload.txt",
        format!("{}\n", std::env::var("REPLACEMENT")?),
    )?;
    record(repo, &["payload.txt"], "Repair source")?;
    Ok(())
}

fn batch_deploy(repo: &Repository) -> Result<()> {
    let context = context()?;
    let oid = field(&context["source"], "commit")?;
    let destination = std::env::var_os("REPO").context("destination repo")?;
    let primary = commit(Path::new(&destination), "HEAD")?;
    let prepared = std::env::var_os("PREPARED").context("prepared marker")?;
    ensure!(oid == primary.to_string() && oid == fs::read_to_string(prepared)?);
    ensure!(fs::read_to_string("previous.txt")? == "previous release");
    let development = std::env::var_os("DEVELOPMENT").context("development root")?;
    for name in ["change-a", "change-b"] {
        ensure!(fs::read_to_string(format!("{name}.txt"))? == name);
        ensure!(!Path::new(&development).join(name).exists());
    }
    let refs = repo
        .references_glob("refs/heads/*")?
        .map(|item| -> Result<String> { Ok(item?.name().context("branch name")?.to_owned()) })
        .collect::<Result<Vec<_>>>()?;
    ensure!(refs == ["refs/heads/main"]);
    append("DEPLOYED", oid)
}

#[test]
#[ignore = "executed as a configured workflow command by other tests"]
fn fixture_workflow() -> Result<()> {
    let Some(action) = std::env::var_os("FIXTURE_ACTION") else {
        return Ok(());
    };
    let repo = Repository::open(".")?;
    match action.to_str().context("fixture action")? {
        "compose" => compose(&repo),
        "empty_compose" => write_composed_commit(&repo, &context()?),
        "merge_repair" => merge_repair(&repo),
        "repair" => repair(&repo),
        "batch_deploy" => batch_deploy(&repo),
        "prepare_head" => append("PREPARED", &repo.head()?.peel_to_commit()?.id().to_string()),
        "deploy_head_failure" => {
            let destination = std::env::var_os("REPO").context("destination repo")?;
            append(
                "DEPLOYMENT",
                &commit(Path::new(&destination), "HEAD")?.to_string(),
            )?;
            std::process::exit(23);
        }
        "stage_fix" => {
            fs::write("payload.txt", "staged only\n")?;
            fs::write("build-output", "generated")?;
            let mut index = repo.index()?;
            index.add_path(Path::new("payload.txt"))?;
            index.add_path(Path::new("build-output"))?;
            index.write()?;
            fs::write("payload.txt", "corrected source\n")?;
            Ok(())
        }
        "observe_fix" => {
            let context = context()?;
            let source = &context["source"];
            let head = repo.head()?.peel_to_commit()?.id();
            ensure!(source["commit"] == head.to_string());
            ensure!(source["submitted_commit"] != source["commit"]);
            ensure!(Path::new(field(&context["inputs"], "fix")?).is_dir());
            fs::write(
                std::env::var_os("MARKER").context("source marker")?,
                context.to_string(),
            )?;
            Ok(())
        }
        _ => bail!("unexpected fixture action"),
    }
}
