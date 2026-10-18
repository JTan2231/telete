// Included in paths::tests to exercise selection without a Cell host setup.

fn local_path_fixture() -> Result<(tempfile::TempDir, PathBuf)> {
    let directory = tempfile::tempdir()?;
    let base = fs::canonicalize(directory.path())?;
    Ok((directory, base.join("state")))
}

fn retain_layout(root: &Path, project: bool, workspace: Option<&Path>) -> Result<()> {
    let store = crate::store::Store::open(root, true)?;
    let config = crate::store::Config {
        project,
        workspace: workspace.map(Path::to_path_buf),
        repository: "/fixture".into(),
        common_git_dir: "/fixture/.git".into(),
        accepted_baseline: None,
    };
    store.set("config", &config)
}

#[test]
fn new_project_uses_only_its_state_and_reopens_from_retained_binding() -> Result<()> {
    let (_directory, root) = local_path_fixture()?;
    let paths = Paths::selected(root.clone(), true)?;
    assert_eq!(paths.workspace, root);
    assert_eq!(paths.local_workspace(), Some(root.clone()));
    paths.require_capacity()?;
    let environment = paths.environment();
    for (key, relative) in [
        ("CARGO_HOME", "cargo"),
        ("CARGO_TARGET_DIR", "targets"),
        ("TMPDIR", "tmp"),
        ("XDG_CACHE_HOME", "cache"),
    ] {
        assert_eq!(environment[key], root.join(relative).display().to_string());
        assert!(root.join(relative).is_dir());
    }
    let repo = root.parent().context("fixture parent")?.join("repository");
    fs::create_dir(&repo)?;
    crate::git::value(&paths, &repo, &["init", "--initial-branch=main"])?;
    crate::manager::init_project(&paths, &repo, None)?;
    let config = crate::store::probe_config(&root)?.context("retained configuration")?;
    assert_eq!(config.workspace, Some(root.clone()));
    assert!(config.project);
    let reopened = Paths::selected(root.clone(), false)?;
    assert_eq!(reopened.environment(), environment);
    crate::manager::init_project(&reopened, &repo, None)?;
    assert_eq!(crate::store::probe_config(&root)?, Some(config));
    Ok(())
}

#[test]
fn legacy_project_and_cell_queues_keep_the_external_selection() -> Result<()> {
    for project in [false, true] {
        let (_directory, root) = local_path_fixture()?;
        retain_layout(&root, project, None)?;
        let original = fs::read(root.join("queue.sqlite3"))?;
        assert!(!Paths::select_local(&root, false)?);
        assert!(!Paths::select_local(&root, true)?);
        let config = crate::store::probe_config(&root)?.context("legacy configuration")?;
        assert!(config.workspace.is_none());
        assert_eq!(fs::read(root.join("queue.sqlite3"))?, original);
        assert!(!root.join("cargo").exists());
        assert!(!root.join("jobs").exists());
    }
    Ok(())
}

#[test]
fn invalid_retained_local_bindings_fail_before_creating_working_directories() -> Result<()> {
    for (project, workspace) in [
        (false, PathBuf::from("same")),
        (true, PathBuf::from("different")),
        (true, PathBuf::from("relative")),
        (true, PathBuf::from("parent")),
        (true, PathBuf::from("dot")),
    ] {
        let (_directory, root) = local_path_fixture()?;
        let binding = match workspace.to_str().context("fixture path")? {
            "same" => root.clone(),
            "different" => root.with_file_name("another"),
            "parent" => root.join("../state"),
            "dot" => root.join("."),
            _ => workspace,
        };
        retain_layout(&root, project, Some(&binding))?;
        assert!(Paths::selected(root.clone(), false).is_err());
        assert!(Paths::selected(root.clone(), true).is_err());
        assert!(!root.join("jobs").exists());
        assert!(!root.join("cargo").exists());
    }
    Ok(())
}

#[test]
fn only_explicit_new_project_initialization_can_select_a_local_layout() -> Result<()> {
    let (_directory, root) = local_path_fixture()?;
    assert!(Paths::selected(root.clone(), false).is_err());
    assert!(!root.exists());
    assert!(Paths::select_local(&root, false).is_err());
    assert!(!root.exists());
    ensure_private(&root)?;
    assert!(Paths::select_local(&root, true)?);
    fs::write(root.join("unrecognized"), "retained")?;
    assert!(Paths::selected(root.clone(), true).is_err());
    assert!(!root.join("queue.sqlite3").exists());
    assert!(!root.join("jobs").exists());
    Ok(())
}

#[test]
fn incomplete_foreign_and_malformed_journals_cannot_become_local_queues() -> Result<()> {
    for kind in ["empty", "foreign", "incomplete", "malformed", "not-sqlite"] {
        let (_directory, root) = local_path_fixture()?;
        ensure_private(&root)?;
        let database = root.join("queue.sqlite3");
        match kind {
            "empty" => {
                File::create(&database)?;
            }
            "not-sqlite" => fs::write(&database, "unfinished journal")?,
            "foreign" => {
                let db = rusqlite::Connection::open(&database)?;
                db.execute_batch("CREATE TABLE foreign_state (value TEXT)")?;
            }
            _ => {
                let store = crate::store::Store::open(&root, true)?;
                if kind == "malformed" {
                    store.set("config", &"unfinished config")?;
                }
            }
        }
        fs::set_permissions(&database, fs::Permissions::from_mode(0o600))?;
        let before = fs::read(&database)?;
        assert!(Paths::selected(root.clone(), true).is_err(), "{kind}");
        assert_eq!(fs::read(&database)?, before, "{kind}");
        assert!(!root.join("jobs").exists(), "{kind}");
    }
    Ok(())
}

#[test]
fn state_selection_rejects_symbolic_nonprivate_and_unnormalized_roots() -> Result<()> {
    let (_directory, root) = local_path_fixture()?;
    let actual = root.with_file_name("actual");
    ensure_private(&actual)?;
    std::os::unix::fs::symlink(&actual, &root)?;
    assert!(Paths::selected(root.clone(), true).is_err());
    fs::remove_file(&root)?;
    fs::create_dir(&root)?;
    fs::set_permissions(&root, fs::Permissions::from_mode(0o755))?;
    assert!(Paths::selected(root.clone(), true).is_err());
    assert!(Paths::selected(root.join("../other"), true).is_err());
    assert!(Paths::selected(root.join("."), true).is_err());
    assert!(Paths::selected(PathBuf::from("relative"), true).is_err());
    assert!(!root.join("queue.sqlite3").exists());
    Ok(())
}
