use super::*;

fn write_config(dir: &Path, json: &str) {
    std::fs::create_dir_all(dir.join(".cursor")).unwrap();
    std::fs::write(dir.join(".cursor").join(FILE), json).unwrap();
}

#[test]
fn no_file_or_no_setup_key_means_no_setup() {
    let wt = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    assert_eq!(load(wt.path(), src.path()), Ok(None));
    write_config(src.path(), r#"{"other": 1}"#);
    assert_eq!(load(wt.path(), src.path()), Ok(None));
}

#[test]
fn the_worktree_copy_wins_over_the_source_copy() {
    let wt = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    write_config(src.path(), r#"{"setup-worktree": ["echo source"]}"#);
    assert_eq!(
        load(wt.path(), src.path()),
        Ok(Some(SetupSteps::Commands(vec!["echo source".into()])))
    );
    write_config(wt.path(), r#"{"setup-worktree": ["echo worktree"]}"#);
    assert_eq!(
        load(wt.path(), src.path()),
        Ok(Some(SetupSteps::Commands(vec!["echo worktree".into()])))
    );
}

#[test]
fn the_os_key_wins_and_a_string_is_a_script_beside_the_file() {
    let wt = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    write_config(
        src.path(),
        &format!(r#"{{"setup-worktree": ["echo generic"], "{OS_KEY}": "setup.sh"}}"#),
    );
    assert_eq!(
        load(wt.path(), src.path()),
        Ok(Some(SetupSteps::Script(
            src.path().join(".cursor").join("setup.sh")
        )))
    );
}

#[test]
fn malformed_setup_is_an_error() {
    let wt = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    write_config(src.path(), r#"{"setup-worktree": ["ok", 3]}"#);
    assert!(load(wt.path(), src.path()).is_err());
    write_config(src.path(), r#"{"setup-worktree": 3}"#);
    assert!(load(wt.path(), src.path()).is_err());
    write_config(src.path(), "not json");
    assert!(load(wt.path(), src.path()).is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn commands_run_in_order_in_the_worktree_with_the_root_path() {
    let wt = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    std::fs::write(src.path().join(".env"), "SECRET=1\n").unwrap();
    let steps = SetupSteps::Commands(vec![
        "cp \"$ROOT_WORKTREE_PATH/.env\" .env".into(),
        "echo second >> order.txt".into(),
    ]);
    run(&steps, wt.path(), src.path()).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(wt.path().join(".env")).unwrap(),
        "SECRET=1\n"
    );
    assert_eq!(
        std::fs::read_to_string(wt.path().join("order.txt")).unwrap(),
        "second\n"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn the_first_failure_stops_the_setup_and_names_the_command() {
    let wt = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let steps = SetupSteps::Commands(vec![
        "echo boom >&2; exit 3".into(),
        "touch never.txt".into(),
    ]);
    let err = run(&steps, wt.path(), src.path()).await.unwrap_err();
    assert!(err.contains("echo boom"), "{err}");
    assert!(err.contains("boom"), "{err}");
    assert!(!wt.path().join("never.txt").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn a_script_runs_in_the_worktree() {
    let wt = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let script = src.path().join("setup.sh");
    std::fs::write(&script, "pwd > where.txt\n").unwrap();
    run(&SetupSteps::Script(script), wt.path(), src.path())
        .await
        .unwrap();
    let where_ = std::fs::read_to_string(wt.path().join("where.txt")).unwrap();
    assert_eq!(
        dunce::canonicalize(where_.trim()).unwrap(),
        dunce::canonicalize(wt.path()).unwrap()
    );
}
