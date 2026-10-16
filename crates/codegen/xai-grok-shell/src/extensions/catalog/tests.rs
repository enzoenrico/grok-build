use super::*;

use pretty_assertions::assert_eq;
use serde_json::{Value, json};

fn agent_skill(name: &str) -> SkillInfo {
    serde_json::from_value(json!({
        "name": name,
        "description": format!("{name} from the agent"),
        "path": format!("cursor://skills/{name}"),
        "scope": "user",
    }))
    .unwrap()
}

async fn answer(method: &str, params: Value, cwd: &Path, agent_skills: &[SkillInfo]) -> Value {
    let response = handle(method, &params, cwd, agent_skills)
        .await
        .unwrap_or_else(|error| panic!("{method} failed: {error:?}"));
    let reply: Value = serde_json::from_str(response.0.get()).unwrap();
    assert!(
        reply.get("error").is_none(),
        "{method} answered an error: {reply}"
    );
    reply["result"].clone()
}

#[test]
fn methods_match_with_or_without_the_extension_prefix() {
    assert_eq!(
        Some("x.ai/skills/list"),
        catalog_method("_x.ai/skills/list")
    );
    assert_eq!(Some("x.ai/hooks/list"), catalog_method("x.ai/hooks/list"));
    assert_eq!(None, catalog_method("x.ai/queue/clear"));
    assert_eq!(None, catalog_method("session/prompt"));
}

#[test]
fn skills_cwd_is_anchored_to_the_session_cwd() {
    let dir = tempfile::tempdir().unwrap();
    let root = dunce::canonicalize(dir.path()).unwrap();
    let root_str = root.to_string_lossy().into_owned();

    let anchored = with_absolute_cwd(&json!({ "cwd": ".", "name": "lint" }), &root);
    assert_eq!(json!({ "cwd": root_str, "name": "lint" }), anchored);

    let missing = with_absolute_cwd(&json!({}), &root);
    assert_eq!(json!({ "cwd": root_str }), missing);

    let absolute = with_absolute_cwd(&json!({ "cwd": root_str }), Path::new("/elsewhere"));
    assert_eq!(json!({ "cwd": root_str }), absolute);
}

#[tokio::test(flavor = "current_thread")]
async fn every_session_free_method_answers_for_a_cwd() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path();
    let session = json!({ "sessionId": "cursor-session" });

    let skills = answer(
        "_x.ai/skills/list",
        json!({ "cwd": ".", "sessionId": "cursor-session" }),
        cwd,
        &[],
    )
    .await;
    assert!(skills["skills"].is_array());
    answer("x.ai/skills/config", json!({ "cwd": "." }), cwd, &[]).await;
    answer("x.ai/skills/refresh-baseline", json!({}), cwd, &[]).await;

    let workflows = answer("x.ai/workflows/list", session.clone(), cwd, &[]).await;
    assert!(workflows["workflows"].is_array());

    let hooks = answer("x.ai/hooks/list", session.clone(), cwd, &[]).await;
    assert!(hooks["hooks"].is_array());
    let reloaded = answer(
        "x.ai/hooks/action",
        json!({ "sessionId": "cursor-session", "action": { "type": "reload" } }),
        cwd,
        &[],
    )
    .await;
    assert_eq!("success", reloaded["status"]);
    assert!(
        reloaded["message"]
            .as_str()
            .unwrap()
            .starts_with("Hooks reloaded")
    );

    let plugins = answer("x.ai/plugins/list", session.clone(), cwd, &[]).await;
    assert!(plugins["plugins"].is_array());
    let rebuilt = answer(
        "x.ai/plugins/action",
        json!({ "sessionId": "cursor-session", "action": { "type": "reload" } }),
        cwd,
        &[],
    )
    .await;
    assert_eq!("success", rebuilt["status"]);
    answer("x.ai/plugins/reload", json!({}), cwd, &[]).await;
    answer(
        "x.ai/plugins/notify-updates",
        json!({ "sessionId": "cursor-session", "updates": [] }),
        cwd,
        &[],
    )
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn agent_skills_list_once_alongside_discovered_skills() {
    let dir = tempfile::tempdir().unwrap();
    let only_agent = agent_skill("cursor-only-skill-for-catalog-test");

    let listed = answer(
        "x.ai/skills/list",
        json!({ "cwd": "." }),
        dir.path(),
        &[only_agent.clone(), only_agent.clone()],
    )
    .await;

    let matching: Vec<&Value> = listed["skills"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|skill| skill["name"] == only_agent.name.as_str())
        .collect();
    assert_eq!(1, matching.len(), "{listed}");
    assert_eq!(
        "cursor://skills/cursor-only-skill-for-catalog-test",
        matching[0]["path"]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn recorded_prompts_come_back_from_prompt_history() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().to_string_lossy().into_owned();
    record_prompt(&cwd, "cursor-a", Some("p-1"), "first");
    record_prompt(&cwd, "cursor-b", None, "other session");
    record_prompt(&cwd, "cursor-a", Some("task-completed-t1"), "a wake turn");
    record_prompt(&cwd, "cursor-a", Some("p-2"), "  ");
    record_prompt(&cwd, "cursor-a", Some("p-3"), "second");

    let history = |params: Value| {
        let cwd = dir.path().to_path_buf();
        async move {
            let response = handle("_x.ai/prompt_history", &params, &cwd, &[])
                .await
                .unwrap();
            serde_json::from_str::<Value>(response.0.get()).unwrap()["prompts"].clone()
        }
    };
    let scoped = history(json!({ "cwd": cwd, "filter_session_id": "cursor-a" })).await;
    let everything = history(json!({ "cwd": cwd })).await;
    let _ = std::fs::remove_dir_all(crate::util::grok_home::sessions_cwd_dir(&cwd));

    assert_eq!(json!(["second", "first"]), scoped);
    assert_eq!(json!(["second", "other session", "first"]), everything);
}

#[tokio::test(flavor = "current_thread")]
async fn an_unknown_method_is_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let error = handle("x.ai/queue/clear", &json!({}), dir.path(), &[])
        .await
        .unwrap_err();
    assert_eq!(acp::Error::method_not_found().code, error.code);
}
