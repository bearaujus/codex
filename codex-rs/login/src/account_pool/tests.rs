use super::*;

use base64::Engine;
use chrono::Utc;
use pretty_assertions::assert_eq;
use serde::Serialize;
use serde_json::json;
use tempfile::TempDir;

use crate::token_data::TokenData;
use crate::token_data::parse_chatgpt_jwt_claims;

fn fake_jwt(email: &str, account_id: &str, plan_type: &str) -> String {
    #[derive(Serialize)]
    struct Header {
        alg: &'static str,
        typ: &'static str,
    }

    let header = Header {
        alg: "none",
        typ: "JWT",
    };
    let payload = json!({
        "email": email,
        "https://api.openai.com/auth": {
            "chatgpt_account_id": account_id,
            "chatgpt_plan_type": plan_type,
        },
    });
    let encode = |bytes: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let header_b64 = encode(&serde_json::to_vec(&header).expect("serialize header"));
    let payload_b64 = encode(&serde_json::to_vec(&payload).expect("serialize payload"));
    let signature_b64 = encode(b"sig");
    format!("{header_b64}.{payload_b64}.{signature_b64}")
}

fn chatgpt_auth(email: &str, account_id: &str, plan_type: &str) -> AuthDotJson {
    let id_token = fake_jwt(email, account_id, plan_type);
    AuthDotJson {
        auth_mode: Some(codex_app_server_protocol::AuthMode::Chatgpt),
        openai_api_key: None,
        tokens: Some(TokenData {
            id_token: parse_chatgpt_jwt_claims(&id_token).expect("id token should parse"),
            access_token: fake_jwt(email, account_id, plan_type),
            refresh_token: format!("refresh-{account_id}"),
            account_id: Some(account_id.to_string()),
        }),
        last_refresh: Some(Utc::now()),
        agent_identity: None,
    }
}

#[tokio::test]
async fn register_account_sets_default_and_selected() {
    let codex_home = TempDir::new().expect("tempdir");
    let pool = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        None,
    )
    .await
    .expect("pool should open");
    let registered = pool
        .register_account(&chatgpt_auth("one@example.com", "workspace-1", "pro"))
        .await
        .expect("register should succeed");
    assert_eq!(registered.account_id, "workspace-1");
    assert!(registered.is_default);
    assert!(registered.is_selected);

    let selected = pool
        .selected_account_auth()
        .await
        .expect("selected auth lookup should succeed")
        .expect("selected auth should exist");
    assert_eq!(selected.0, "workspace-1");
}

#[tokio::test]
async fn migrate_legacy_auth_into_pool_on_open() {
    let codex_home = TempDir::new().expect("tempdir");
    save_auth(
        codex_home.path(),
        &chatgpt_auth("legacy@example.com", "workspace-legacy", "plus"),
        AuthCredentialsStoreMode::File,
    )
    .expect("legacy auth should save");

    let pool = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        None,
    )
    .await
    .expect("pool should open");
    let accounts = pool.list_accounts().await.expect("accounts should list");
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].account_id, "workspace-legacy");
    assert!(accounts[0].is_selected);
}

#[tokio::test]
async fn resolve_turn_selection_keeps_current_selected_account() {
    let codex_home = TempDir::new().expect("tempdir");
    let pool = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        None,
    )
    .await
    .expect("pool should open");
    pool.register_account(&chatgpt_auth("one@example.com", "workspace-1", "pro"))
        .await
        .expect("first account");
    pool.register_account(&chatgpt_auth("two@example.com", "workspace-2", "pro"))
        .await
        .expect("second account");

    let selection = pool
        .resolve_turn_selection(Some("workspace-1"), false)
        .await
        .expect("selection should succeed");
    assert_eq!(selection, ChatgptAccountPoolSelectionOutcome::Unchanged);
}

#[tokio::test]
async fn resolve_turn_selection_skips_cooling_down_account_and_prefers_oldest_unused() {
    let codex_home = TempDir::new().expect("tempdir");
    let pool = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        None,
    )
    .await
    .expect("pool should open");
    pool.register_account(&chatgpt_auth("one@example.com", "workspace-1", "pro"))
        .await
        .expect("first account");
    pool.register_account(&chatgpt_auth("two@example.com", "workspace-2", "pro"))
        .await
        .expect("second account");
    pool.select_account("workspace-1")
        .await
        .expect("selection should succeed");
    pool.mark_current_account_rate_limited(
        "workspace-1",
        Some(&RateLimitSnapshot {
            limit_id: Some("codex".to_string()),
            limit_name: None,
            primary: Some(RateLimitWindow {
                used_percent: 100.0,
                window_minutes: Some(300),
                resets_at: Some(now_ts() + 3600),
            }),
            secondary: None,
            credits: None,
            plan_type: None,
            rate_limit_reached_type: Some(RateLimitReachedType::RateLimitReached),
        }),
        None,
    )
    .await
    .expect("marking limit should succeed");

    let selection = pool
        .resolve_turn_selection(Some("workspace-1"), false)
        .await
        .expect("selection should succeed");
    let ChatgptAccountPoolSelectionOutcome::Activated {
        account_id,
        failover,
        ..
    } = selection
    else {
        panic!("expected failover activation");
    };
    assert_eq!(account_id, "workspace-2");
    assert!(failover);
}
