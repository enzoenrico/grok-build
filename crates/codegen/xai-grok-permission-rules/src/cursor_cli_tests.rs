use super::{project_cli_json_paths, translate};
use crate::policy::CompiledPolicy;
use crate::types::{AccessKind, Decision, PermissionConfig, RuleAction};

fn policy(entries: &[(&str, RuleAction)]) -> CompiledPolicy {
    let rules = entries
        .iter()
        .flat_map(|(entry, action)| translate(entry, *action).unwrap())
        .collect();
    CompiledPolicy::new(PermissionConfig::new(rules))
}

fn bash(cmd: &str) -> AccessKind {
    AccessKind::Bash(cmd.to_string())
}

fn mcp(name: &str) -> AccessKind {
    AccessKind::MCPTool {
        name: name.to_string(),
        input: serde_json::Value::Null,
    }
}

fn fetch(url: &str) -> AccessKind {
    AccessKind::WebFetch(url.to_string())
}

fn denied(decision: Option<Decision>) -> bool {
    matches!(
        decision,
        Some(Decision::Reject(_) | Decision::PolicyDeny(_))
    )
}

#[test]
fn shell_command_base_allows_that_program_only() {
    let p = policy(&[("Shell(git)", RuleAction::Allow)]);
    assert_eq!(p.evaluate(&bash("git status")), Some(Decision::Allow));
    assert_eq!(p.evaluate(&bash("git")), Some(Decision::Allow));
    assert_ne!(p.evaluate(&bash("gitleaks detect")), Some(Decision::Allow));
}

#[test]
fn shell_multi_word_base_allows_that_subcommand_only() {
    let p = policy(&[("Shell(rtk git status)", RuleAction::Allow)]);
    assert_eq!(
        p.evaluate(&bash("rtk git status --short")),
        Some(Decision::Allow)
    );
    assert_ne!(p.evaluate(&bash("rtk git push")), Some(Decision::Allow));
}

#[test]
fn shell_command_args_form() {
    let p = policy(&[
        ("Shell(curl:*)", RuleAction::Allow),
        ("Shell(git:push*)", RuleAction::Deny),
    ]);
    assert_eq!(
        p.evaluate(&bash("curl -s https://x.dev")),
        Some(Decision::Allow)
    );
    assert!(denied(p.evaluate(&bash("git push --force"))));
    assert!(!denied(p.evaluate(&bash("git status"))));
}

#[test]
fn shell_colon_inside_an_argument_is_literal() {
    let p = policy(&[("Shell(docker run img:latest)", RuleAction::Allow)]);
    assert_eq!(
        p.evaluate(&bash("docker run img:latest")),
        Some(Decision::Allow)
    );
    assert_ne!(p.evaluate(&bash("docker run other")), Some(Decision::Allow));
}

#[test]
fn deny_beats_allow() {
    let p = policy(&[
        ("Shell(git)", RuleAction::Allow),
        ("Shell(git push)", RuleAction::Deny),
    ]);
    assert_eq!(p.evaluate(&bash("git log")), Some(Decision::Allow));
    assert!(denied(p.evaluate(&bash("git push origin main"))));
}

#[test]
fn mcp_server_tool_maps_to_qualified_names() {
    let p = policy(&[
        ("Mcp(github:*)", RuleAction::Allow),
        ("Mcp(*:search)", RuleAction::Allow),
    ]);
    assert_eq!(p.evaluate(&mcp("github__get_issue")), Some(Decision::Allow));
    assert_eq!(p.evaluate(&mcp("docs__search")), Some(Decision::Allow));
    assert_ne!(p.evaluate(&mcp("gitlab__get_issue")), Some(Decision::Allow));
}

#[test]
fn web_fetch_exact_host_allow_excludes_subdomains_and_lookalikes() {
    let p = policy(&[("WebFetch(example.com)", RuleAction::Allow)]);
    assert_eq!(
        p.evaluate(&fetch("https://example.com/a")),
        Some(Decision::Allow)
    );
    assert_eq!(
        p.evaluate(&fetch("http://example.com/")),
        Some(Decision::Allow)
    );
    for url in [
        "https://evil.example.com/",
        "https://example.com.evil.net/",
        "https://example.com@evil.net/",
        "https://evil.net/?u=https://example.com/",
    ] {
        assert_ne!(p.evaluate(&fetch(url)), Some(Decision::Allow), "{url}");
    }
}

#[test]
fn web_fetch_subdomain_wildcard_and_deny() {
    let p = policy(&[
        ("WebFetch(*.example.com)", RuleAction::Allow),
        ("WebFetch(blocked.dev)", RuleAction::Deny),
    ]);
    assert_eq!(
        p.evaluate(&fetch("https://api.example.com/v1")),
        Some(Decision::Allow)
    );
    assert!(denied(p.evaluate(&fetch("https://blocked.dev/x"))));
    assert!(denied(p.evaluate(&fetch("https://cdn.blocked.dev/x"))));
}

#[test]
fn write_and_read_map_to_edit_and_read() {
    let p = policy(&[
        ("Write(**/.env*)", RuleAction::Deny),
        ("Read(src/**)", RuleAction::Allow),
    ]);
    assert!(denied(
        p.evaluate(&AccessKind::Edit("config/.env.local".into()))
    ));
    assert_eq!(
        p.evaluate(&AccessKind::Read(Some("src/main.rs".into()))),
        Some(Decision::Allow)
    );
}

#[test]
fn malformed_and_unknown_entries_are_errors() {
    for entry in [
        "Shell",
        "Shell()",
        "Shell(git",
        "Browser(x)",
        "Mcp(:tool)",
        "Mcp(github)",
        "Shell(git:)",
        "WebFetch(https://example.com)",
    ] {
        assert!(translate(entry, RuleAction::Allow).is_err(), "{entry}");
    }
}

#[test]
fn shell_globbed_command_name_is_refused_for_allow_and_widened_for_deny() {
    assert!(translate("Shell(*sh)", RuleAction::Allow).is_err());
    let p = policy(&[
        ("Shell(ssh)", RuleAction::Allow),
        ("Shell(*sh)", RuleAction::Deny),
    ]);
    assert!(denied(p.evaluate(&bash("ssh prod-host"))));
    assert!(denied(p.evaluate(&bash("bash -c 'id'"))));
    assert!(!denied(p.evaluate(&bash("ls -la"))));
}

#[test]
fn shell_exact_arguments_allow_is_refused() {
    assert!(translate("Shell(git:status)", RuleAction::Allow).is_err());
    assert!(translate("Shell(git:status)", RuleAction::Deny).is_ok());
}

#[test]
fn relative_path_allows_stay_inside_the_workspace() {
    let p = policy(&[
        ("Write(*)", RuleAction::Allow),
        ("Read(**/*.md)", RuleAction::Allow),
    ]);
    assert_eq!(
        p.evaluate(&AccessKind::Edit("src/a.rs".into())),
        Some(Decision::Allow)
    );
    assert_eq!(
        p.evaluate(&AccessKind::Read(Some("docs/guide.md".into()))),
        Some(Decision::Allow)
    );
    assert_ne!(
        p.evaluate(&AccessKind::Edit("/Users/me/.zshrc".into())),
        Some(Decision::Allow)
    );
    assert_ne!(
        p.evaluate(&AccessKind::Read(Some("/Users/me/notes/secret.md".into()))),
        Some(Decision::Allow)
    );
}

#[test]
fn web_fetch_www_subdomain_allow_is_refused() {
    assert!(translate("WebFetch(*.www.example.com)", RuleAction::Allow).is_err());
}

#[test]
fn project_walk_runs_from_cwd_to_repo_root() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let cwd = repo.join("a").join("b");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    let cli = |dir: &std::path::Path| dir.join(".cursor").join("cli.json");
    assert_eq!(
        project_cli_json_paths(&cwd),
        vec![cli(&cwd), cli(&repo.join("a")), cli(&repo)]
    );

    let loose = tmp.path().join("loose");
    std::fs::create_dir_all(&loose).unwrap();
    assert_eq!(project_cli_json_paths(&loose), vec![cli(&loose)]);
}
