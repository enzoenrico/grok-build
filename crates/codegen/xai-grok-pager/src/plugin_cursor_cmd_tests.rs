use clap::Parser;

use super::*;

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: CursorPluginCommand,
}

fn parse(args: &[&str]) -> CursorPluginCommand {
    let mut argv = vec!["cursor"];
    argv.extend_from_slice(args);
    Cli::try_parse_from(argv).expect("parses").command
}

fn write(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

#[test]
fn parses_plugin_and_marketplace_subcommands() {
    assert!(matches!(
        parse(&["install", "docs@acme", "--trust"]),
        CursorPluginCommand::Install { plugin, trust: true } if plugin == "docs@acme"
    ));
    assert!(matches!(
        parse(&["uninstall", "docs"]),
        CursorPluginCommand::Remove { plugin } if plugin == "docs"
    ));
    assert!(matches!(
        parse(&["update"]),
        CursorPluginCommand::Update { target: None }
    ));
    assert!(matches!(
        parse(&["list", "--json"]),
        CursorPluginCommand::List { json: true }
    ));
    match parse(&["marketplace", "add", "acme/plugins", "--ref", "v2"]) {
        CursorPluginCommand::Marketplace(CursorMarketplaceArgs {
            command: CursorMarketplaceCommand::Add { url, git_ref },
        }) => {
            assert_eq!(url, "acme/plugins");
            assert_eq!(git_ref.as_deref(), Some("v2"));
        }
        other => panic!("unexpected {other:?}"),
    }
    assert!(matches!(
        parse(&["marketplace", "rm", "acme"]),
        CursorPluginCommand::Marketplace(CursorMarketplaceArgs {
            command: CursorMarketplaceCommand::Remove { marketplace },
        }) if marketplace == "acme"
    ));
}

#[test]
fn installed_entries_name_cache_and_local_plugins() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path();
    let cached = cursor::cache_version_dir(config, "acme", "docs", "abc123");
    write(&cached.join(cursor::CACHE_COMPLETE_MARKER), "");
    write(
        &cached.join(".cursor-plugin/plugin.json"),
        r#"{"name":"Docs.Helper"}"#,
    );
    let local = cursor::plugins_dir(config).join("local/my-tool");
    write(
        &local.join("skills/demo/SKILL.md"),
        "---\nname: demo\n---\n",
    );

    let entries = installed_entries(config);
    assert_eq!(
        entries,
        vec![
            InstalledEntry {
                name: sanitize_cursor_name("Docs.Helper"),
                marketplace: Some("acme".into()),
                root: cached,
            },
            InstalledEntry {
                name: "my-tool".into(),
                marketplace: None,
                root: local,
            },
        ]
    );
}

#[test]
fn marketplace_entries_report_installed_flag() {
    let store: LocalMarketplaces = serde_json::from_str(
        r#"{"version":1,"marketplaces":[{"name":"acme","gitUrl":"https://github.com/acme/plugins.git",
            "gitRef":"main","commitSha":"abc","plugins":[
              {"name":"docs","description":"Docs","enabled":true},
              {"name":"lint"}]}]}"#,
    )
    .unwrap();
    let entries = marketplace_entries(&store);
    let json = serde_json::to_value(&entries).unwrap();
    assert_eq!(json[0]["name"], "acme");
    assert_eq!(json[0]["plugins"][0]["installed"], true);
    assert_eq!(json[0]["plugins"][1]["installed"], false);
    assert_eq!(
        json[0]["plugins"][1]["description"],
        serde_json::Value::Null
    );
}
