use super::*;

fn write(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// A marketplace repo with one in-repo plugin (`alpha`) and one entry pointing at another repo.
fn marketplace_repo(root: &Path) -> String {
    let repo = root.join("acme").join("cursor-plugins");
    write(
        &repo.join(".cursor-plugin/marketplace.json"),
        r#"{
            "name": "acme-tools",
            "plugins": [
                { "name": "alpha", "source": "./plugins/alpha", "description": "Alpha tools" },
                { "name": "Remote.Kit", "source": { "source": "github", "repo": "acme/remote-kit", "ref": "v1" } },
                { "name": "bad name", "source": "./plugins/alpha" }
            ]
        }"#,
    );
    let plugin = repo.join("plugins/alpha");
    write(
        &plugin.join(".cursor-plugin/plugin.json"),
        r#"{"name":"alpha"}"#,
    );
    write(
        &plugin.join("skills/review/SKILL.md"),
        "---\nname: review\ndescription: r\n---\nbody\n",
    );
    write(
        &plugin.join("rules/style.mdc"),
        "---\nalwaysApply: true\n---\nUse tabs.\n",
    );
    write(&plugin.join("commands/ship.txt"), "Ship it.\n");
    write(
        &plugin.join("hooks/hooks.json"),
        r#"{"version":1,"hooks":{"stop":[{"command":"./done.sh"}]}}"#,
    );
    git(&repo, &["init", "--quiet", "--initial-branch=main"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "--quiet", "-m", "init"]);
    format!("file://{}", repo.display())
}

#[test]
fn parse_git_url_forms() {
    let gh = parse_git_url("acme/plugins").unwrap();
    assert_eq!(gh.url, "https://github.com/acme/plugins");
    assert_eq!(
        (gh.host.as_str(), gh.owner.as_str(), gh.repo.as_str()),
        ("github.com", "acme", "plugins")
    );
    let https = parse_git_url("https://gitlab.com/group/sub/repo.git/").unwrap();
    assert_eq!(
        (
            https.host.as_str(),
            https.owner.as_str(),
            https.repo.as_str()
        ),
        ("gitlab.com", "group/sub", "repo")
    );
    let ssh = parse_git_url("git@github.com:acme/plugins.git").unwrap();
    assert_eq!(
        (ssh.host.as_str(), ssh.owner.as_str(), ssh.repo.as_str()),
        ("github.com", "acme", "plugins")
    );
    let file = parse_git_url("file:///srv/git/acme/plugins").unwrap();
    assert_eq!(
        (file.host.as_str(), file.owner.as_str(), file.repo.as_str()),
        ("file", "acme", "plugins")
    );
    assert!(parse_git_url("not a url").is_err());
    assert!(parse_git_url("./relative/path").is_err());
}

#[test]
fn clone_dir_follows_cursor_layout() {
    let repo = parse_git_url("https://github.com/acme/con").unwrap();
    let dir = marketplace_clone_dir(Path::new("/c"), &repo, "abc");
    assert_eq!(
        dir,
        Path::new("/c/plugins/marketplaces/github.com/acme/_con/abc")
    );
}

#[test]
fn upsert_keeps_enabled_and_save_round_trips_unknown_fields() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        &LocalMarketplaces::path(tmp.path()),
        r#"{"version":1,"marketplaces":[{"name":"m","gitUrl":"https://github.com/a/b","futureField":7,
            "plugins":[{"name":"p","enabled":true,"skills":[{"name":"s","description":"d"}],"futurePluginField":"x"}]}]}"#,
    );
    let mut store = LocalMarketplaces::load(tmp.path());
    store.upsert(LocalMarketplace {
        name: "m".into(),
        git_url: "https://github.com/a/b".into(),
        git_ref: Some("main".into()),
        commit_sha: None,
        plugins: vec![
            LocalMarketplacePlugin {
                name: "p".into(),
                ..Default::default()
            },
            LocalMarketplacePlugin {
                name: "q".into(),
                ..Default::default()
            },
        ],
        extra: Default::default(),
    });
    let p = &store.marketplaces[0].plugins[0];
    assert!(p.enabled, "re-index must keep the installed flag");
    assert!(!store.marketplaces[0].plugins[1].enabled);

    let mut original = LocalMarketplaces::load(tmp.path());
    assert!(original.set_plugin_enabled("m", "p", false));
    assert!(!original.set_plugin_enabled("m", "missing", true));
    original.save(tmp.path()).unwrap();
    let saved: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(LocalMarketplaces::path(tmp.path())).unwrap(),
    )
    .unwrap();
    assert_eq!(saved["version"], 1);
    assert_eq!(saved["marketplaces"][0]["futureField"], 7);
    assert_eq!(
        saved["marketplaces"][0]["plugins"][0]["futurePluginField"],
        "x"
    );
    assert_eq!(saved["marketplaces"][0]["plugins"][0]["enabled"], false);
    assert_eq!(
        saved["marketplaces"][0]["plugins"][0]["skills"][0]["description"],
        "d"
    );
}

#[test]
fn entry_sources() {
    assert_eq!(
        entry_source(Some(&serde_json::json!("./plugins/a")), None, "a"),
        Some(EntrySource::Local {
            git_path: "plugins/a".into()
        })
    );
    assert_eq!(
        entry_source(Some(&serde_json::json!("a")), Some("./plugins"), "a"),
        Some(EntrySource::Local {
            git_path: "plugins/a".into()
        })
    );
    assert_eq!(
        entry_source(Some(&serde_json::json!("../escape")), None, "a"),
        None
    );
    assert_eq!(
        entry_source(
            Some(
                &serde_json::json!({"source": "git-subdir", "url": "https://x/y.git", "path": "p/q", "sha": "abc"})
            ),
            None,
            "a"
        ),
        Some(EntrySource::External {
            git_url: "https://x/y.git".into(),
            git_ref: Some("abc".into()),
            git_path: Some("p/q".into()),
        })
    );
    assert_eq!(
        entry_source(
            Some(&serde_json::json!({"source": "github", "repo": "o/r"})),
            None,
            "a"
        ),
        Some(EntrySource::External {
            git_url: "https://github.com/o/r".into(),
            git_ref: None,
            git_path: None,
        })
    );
}

#[test]
fn add_install_update_uninstall_remove_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("cursor");
    let url = marketplace_repo(&tmp.path().join("remotes"));
    let repo_dir = tmp.path().join("remotes/acme/cursor-plugins");
    let first_sha = git(&repo_dir, &["rev-parse", "HEAD"]);

    let added = add_marketplace(&config, &url, None).unwrap();
    assert_eq!(added.name, "acme-tools");
    assert_eq!(added.git_ref.as_deref(), Some("main"));
    assert_eq!(added.commit_sha.as_deref(), Some(first_sha.as_str()));
    let names: Vec<&str> = added.plugins.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, vec!["alpha", "remote.kit"]);
    let alpha = &added.plugins[0];
    assert_eq!(alpha.git_path.as_deref(), Some("plugins/alpha"));
    assert_eq!(alpha.skills, vec![serde_json::json!({"name": "review"})]);
    assert_eq!(alpha.rules, vec![serde_json::json!({"name": "style"})]);
    assert_eq!(alpha.commands, vec![serde_json::json!({"name": "ship"})]);
    assert_eq!(alpha.hooks, vec![serde_json::json!({"name": "stop"})]);
    assert_eq!(alpha.description.as_deref(), Some("Alpha tools"));
    assert_eq!(
        added.plugins[1].git_url.as_deref(),
        Some("https://github.com/acme/remote-kit")
    );
    let clone = marketplace_clone_dir(&config, &parse_git_url(&url).unwrap(), &first_sha);
    assert!(clone.join(".cursor-plugin/marketplace.json").is_file());

    let installed = install_plugin(&config, "alpha@acme-tools").unwrap();
    assert_eq!(
        installed.root,
        cache_version_dir(&config, "acme-tools", "alpha", &first_sha)
    );
    assert!(installed.root.join(CACHE_COMPLETE_MARKER).is_file());
    assert!(installed.root.join("rules/style.mdc").is_file());
    assert!(!installed.root.join(".git").exists());
    assert!(LocalMarketplaces::load(&config).marketplaces[0].plugins[0].enabled);
    assert_eq!(
        super::super::cursor::installed_plugins(&config),
        vec![super::super::cursor::CursorInstall {
            name: None,
            marketplace: "acme-tools".into(),
            root: installed.root.clone(),
        }]
    );

    write(&repo_dir.join("plugins/alpha/rules/more.md"), "More.\n");
    git(&repo_dir, &["add", "."]);
    git(&repo_dir, &["commit", "--quiet", "-m", "more"]);
    let second_sha = git(&repo_dir, &["rev-parse", "HEAD"]);
    let refreshed = update(&config, Some("alpha")).unwrap();
    assert_eq!(refreshed.len(), 1);
    assert_eq!(
        refreshed[0].root,
        cache_version_dir(&config, "acme-tools", "alpha", &second_sha)
    );
    assert!(refreshed[0].root.join("rules/more.md").is_file());
    assert!(!installed.root.exists(), "old version is pruned");
    assert!(LocalMarketplaces::load(&config).marketplaces[0].plugins[0].enabled);

    assert!(install_plugin(&config, "missing").is_err());
    let (marketplace, plugin) = uninstall_plugin(&config, "alpha").unwrap();
    assert_eq!(
        (marketplace.as_str(), plugin.as_str()),
        ("acme-tools", "alpha")
    );
    assert!(!cache_dir(&config).join("acme-tools/alpha").exists());
    assert!(!LocalMarketplaces::load(&config).marketplaces[0].plugins[0].enabled);

    assert_eq!(remove_marketplace(&config, &url).unwrap(), "acme-tools");
    assert!(LocalMarketplaces::load(&config).marketplaces.is_empty());
    assert!(remove_marketplace(&config, "acme-tools").is_err());
}

#[test]
fn repo_without_marketplace_manifest_is_one_plugin() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("solo");
    write(
        &root.join(".cursor-plugin/plugin.json"),
        r#"{"name":"Solo.Plugin","description":"one"}"#,
    );
    write(&root.join("rules/a.mdc"), "x");
    let repo = parse_git_url("https://github.com/acme/solo").unwrap();
    let (name, plugins) = index_marketplace_repo(&root, &repo).unwrap();
    assert_eq!(name, "solo-plugin");
    assert_eq!(plugins.len(), 1);
    assert_eq!(plugins[0].git_path, None);
    assert_eq!(plugins[0].rules, vec![serde_json::json!({"name": "a"})]);
}
