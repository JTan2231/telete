// Included in paths::tests to exercise private queue state selection.

fn path_fixture() -> Result<(tempfile::TempDir, PathBuf)> {
    let directory = tempfile::tempdir()?;
    let base = fs::canonicalize(directory.path())?;
    Ok((directory, base.join("state")))
}

fn retain_config(root: &Path, state_root: &Path) -> Result<()> {
    let mut config = crate::store::Config {
        state_root: root.to_path_buf(),
        repository: "/fixture".into(),
        common_git_dir: "/fixture/.git".into(),
    };
    let store = crate::store::Store::create(root, &config)?;
    config.state_root = state_root.to_path_buf();
    store.set("config", &config)
}

#[test]
fn selected_state_creates_only_telete_working_directories() -> Result<()> {
    let (_directory, root) = path_fixture()?;
    let paths = Paths::selected(root.clone(), true)?;
    assert_eq!(paths.root, root);
    for name in ["jobs", "tmp", "broker", "deployments"] {
        assert!(root.join(name).is_dir());
    }
    for name in ["cargo", "cache", "targets", "logs"] {
        assert!(!root.join(name).exists());
    }
    retain_config(&root, &root)?;
    let config = crate::store::probe_config(&root)?.context("retained configuration")?;
    paths.verify_config(&config)?;
    fs::remove_dir(root.join("tmp"))?;
    let reopened = Paths::selected(root.clone(), false)?;
    assert_eq!(reopened.root, root);
    assert!(!root.join("tmp").exists());
    Ok(())
}

#[test]
fn changed_state_binding_fails_before_creating_working_directories() -> Result<()> {
    let (_directory, root) = path_fixture()?;
    retain_config(&root, &root.with_file_name("another"))?;
    let original = fs::read(root.join("queue.sqlite3"))?;
    assert!(Paths::selected(root.clone(), false).is_err());
    assert!(Paths::selected(root.clone(), true).is_err());
    assert_eq!(fs::read(root.join("queue.sqlite3"))?, original);
    assert!(!root.join("jobs").exists());
    assert!(!root.join("cargo").exists());
    Ok(())
}

#[test]
fn read_only_selection_accepts_absent_state_without_creating_it() -> Result<()> {
    let (_directory, root) = path_fixture()?;
    let paths = Paths::selected(root.clone(), false)?;
    assert_eq!(paths.root, root);
    assert!(!root.exists());
    ensure_private(&root)?;
    fs::write(root.join("unrecognized"), "retained")?;
    Paths::selected(root.clone(), false)?;
    assert!(!root.join("queue.sqlite3").exists());
    assert!(!root.join("jobs").exists());
    Paths::selected(root.clone(), true)?;
    assert_eq!(fs::read_to_string(root.join("unrecognized"))?, "retained");
    assert!(root.join("jobs").is_dir());
    Ok(())
}

#[test]
fn incomplete_foreign_and_malformed_journals_fail_before_directory_creation() -> Result<()> {
    for kind in [
        "empty",
        "foreign",
        "incomplete",
        "malformed",
        "not-sqlite",
        "unsupported",
    ] {
        let (_directory, root) = path_fixture()?;
        ensure_private(&root)?;
        let database = root.join("queue.sqlite3");
        match kind {
            "empty" => {
                File::create(&database)?;
            }
            "not-sqlite" => fs::write(&database, "unfinished journal")?,
            "foreign" | "unsupported" => {
                let db = rusqlite::Connection::open(&database)?;
                db.execute_batch("CREATE TABLE foreign_state (value TEXT)")?;
                if kind == "unsupported" {
                    db.pragma_update(None, "user_version", 123)?;
                }
            }
            _ => {
                retain_config(&root, &root)?;
                if kind == "malformed" {
                    let store = crate::store::Store::open(&root)?;
                    store.set("config", &"unfinished config")?;
                } else {
                    let db = rusqlite::Connection::open(&database)?;
                    db.execute("DELETE FROM control WHERE key='config'", [])?;
                }
            }
        }
        fs::set_permissions(&database, fs::Permissions::from_mode(0o600))?;
        let before = fs::read(&database)?;
        assert!(Paths::selected(root.clone(), true).is_err(), "{kind}");
        assert_eq!(fs::read(&database)?, before, "{kind}");
        assert!(!root.join("jobs").exists(), "{kind}");
        assert_eq!(fs::read_dir(&root)?.count(), 1, "{kind}");
    }
    Ok(())
}

#[test]
fn state_selection_rejects_symbolic_nonprivate_and_unnormalized_roots() -> Result<()> {
    let (_directory, root) = path_fixture()?;
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
