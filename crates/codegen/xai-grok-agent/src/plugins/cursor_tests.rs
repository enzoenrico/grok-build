use super::*;

fn write(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

fn cached_plugin(config: &Path, marketplace: &str, plugin_id: &str, version: &str) -> PathBuf {
    let root = cache_version_dir(config, marketplace, plugin_id, version);
    write(
        &root.join("skills/demo/SKILL.md"),
        "---\nname: demo\ndescription: d\n---\nbody\n",
    );
    write(&root.join(CACHE_COMPLETE_MARKER), "");
    root
}

#[test]
fn sanitizers_match_cursor() {
    assert_eq!(sanitize_segment("cursor-public"), "cursor-public");
    assert_eq!(sanitize_segment("my.market/place"), "my_market_place");
    assert_eq!(sanitize_version("release/v0.1.4"), "release_v0.1.4");
    assert_eq!(sanitize_version(".."), "_");
}

#[test]
fn cloud_manifest_names_installs_and_requires_complete_cache() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path();
    let sha = "850d7f2e9f5e55dff7eb00a0a479337fe79eb22f";
    let by_sha = cached_plugin(config, "cursor-public", "677", sha);
    let by_release = cached_plugin(config, "cursor-public", "652", "release/v0.1.4");
    let incomplete = cache_version_dir(config, "cursor-public", "999", sha);
    std::fs::create_dir_all(&incomplete).unwrap();
    write(
        &cache_dir(config).join(CLOUD_MANIFEST_FILE),
        &serde_json::json!({
            "manifestVersion": 1,
            "plugins": [
                { "name": "cursor-team-kit", "pluginId": "677", "marketplaceSlug": "cursor-public", "resolvedCommitSha": sha },
                { "name": "supabase", "pluginId": "652", "marketplaceSlug": "cursor-public", "releaseTag": "v0.1.4" },
                { "name": "half", "pluginId": "999", "marketplaceSlug": "cursor-public", "resolvedCommitSha": sha }
            ]
        })
        .to_string(),
    );

    let installs = installed_plugins(config);
    assert_eq!(
        installs,
        vec![
            CursorInstall {
                name: Some("cursor-team-kit".into()),
                marketplace: "cursor-public".into(),
                root: by_sha,
            },
            CursorInstall {
                name: Some("supabase".into()),
                marketplace: "cursor-public".into(),
                root: by_release,
            },
        ]
    );
}

#[test]
fn cache_scan_without_manifest_picks_newest_complete_version_and_skips_uninstalled() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path();
    let old = cached_plugin(config, "acme", "tool", "aaaa");
    let new = cached_plugin(config, "acme", "tool", "bbbb");
    let past = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    std::fs::File::options()
        .write(true)
        .open(old.join(CACHE_COMPLETE_MARKER))
        .unwrap()
        .set_modified(past)
        .unwrap();
    let half = cache_version_dir(config, "acme", "tool", "cccc");
    std::fs::create_dir_all(&half).unwrap();
    cached_plugin(config, "acme", "off", "aaaa");
    write(
        &plugins_dir(config).join("local-marketplaces.json"),
        r#"{"version":1,"marketplaces":[{"name":"acme","gitUrl":"https://github.com/acme/plugins","plugins":[{"name":"off","enabled":false},{"name":"tool","enabled":true}]}]}"#,
    );

    let installs = installed_plugins(config);
    assert_eq!(
        installs,
        vec![CursorInstall {
            name: None,
            marketplace: "acme".into(),
            root: new,
        }]
    );
}

#[test]
fn enabled_local_marketplace_plugins_join_a_cloud_manifest() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path();
    write(
        &cache_dir(config).join(CLOUD_MANIFEST_FILE),
        r#"{"plugins":[]}"#,
    );
    let root = cached_plugin(config, "acme", "tool", "aaaa");
    write(
        &plugins_dir(config).join("local-marketplaces.json"),
        r#"{"version":1,"marketplaces":[{"name":"acme","gitUrl":"https://github.com/acme/plugins","plugins":[{"name":"tool","enabled":true}]}]}"#,
    );
    assert_eq!(
        installed_plugins(config),
        vec![CursorInstall {
            name: Some("tool".into()),
            marketplace: "acme".into(),
            root,
        }]
    );
}

#[test]
fn local_plugins_skip_dot_dirs_and_symlinks_that_leave_local() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path();
    let local = plugins_dir(config).join("local");
    std::fs::create_dir_all(local.join("alpha")).unwrap();
    std::fs::create_dir_all(local.join(".hidden")).unwrap();
    std::fs::create_dir_all(local.join("nested/inner")).unwrap();
    std::fs::create_dir_all(tmp.path().join("elsewhere")).unwrap();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(local.join("nested/inner"), local.join("inside")).unwrap();
        std::os::unix::fs::symlink(tmp.path().join("elsewhere"), local.join("outside")).unwrap();
    }
    let names: Vec<String> = local_plugin_dirs(config)
        .iter()
        .map(|dir| dir.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    #[cfg(unix)]
    assert_eq!(names, vec!["alpha", "inside", "nested"]);
    #[cfg(not(unix))]
    assert_eq!(names, vec!["alpha", "nested"]);
}
