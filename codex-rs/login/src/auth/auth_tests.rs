use super::*;
use crate::account_pool::ChatgptAccountPoolAuthStatus;
use crate::auth::storage::FileAuthStorage;
use crate::auth::storage::get_auth_file;
use crate::token_data::IdTokenInfo;
use codex_app_server_protocol::AuthMode;
use codex_protocol::account::PlanType as AccountPlanType;
use codex_protocol::auth::KnownPlan as InternalKnownPlan;
use codex_protocol::auth::PlanType as InternalPlanType;

use base64::Engine;
use codex_protocol::config_types::ForcedLoginMethod;
use codex_protocol::config_types::ModelProviderAuthInfo;
use pretty_assertions::assert_eq;
use serde::Serialize;
use serde_json::json;
use sqlx::Connection;
use sqlx::SqliteConnection;
use sqlx::sqlite::SqliteConnectOptions;
use std::sync::Arc;
use tempfile::TempDir;
use tempfile::tempdir;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::header;
use wiremock::matchers::method;
use wiremock::matchers::path;

const WORKSPACE_ID_ALLOWED: &str = "123e4567-e89b-42d3-a456-426614174000";
const WORKSPACE_ID_SECOND_ALLOWED: &str = "123e4567-e89b-42d3-a456-426614174001";
const WORKSPACE_ID_DISALLOWED: &str = "123e4567-e89b-42d3-a456-426614174002";

fn chatgpt_backend_base_url(server: &MockServer) -> String {
    format!("{}/backend-api", server.uri())
}

async fn mount_usage_probe(server: &MockServer, account_id: &str, status: u16) {
    Mock::given(method("GET"))
        .and(path("/backend-api/wham/usage"))
        .and(header("ChatGPT-Account-Id", account_id))
        .respond_with(ResponseTemplate::new(status))
        .expect(1)
        .mount(server)
        .await;
}

#[tokio::test]
async fn refresh_without_id_token() {
    let codex_home = tempdir().unwrap();
    let fake_jwt = write_auth_file(
        AuthFileParams {
            openai_api_key: None,
            chatgpt_plan_type: Some("pro".to_string()),
            chatgpt_account_id: None,
        },
        codex_home.path(),
    )
    .expect("failed to write auth file");

    let storage = create_auth_storage(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
    );
    let updated = super::persist_tokens(
        &storage,
        /*id_token*/ None,
        Some("new-access-token".to_string()),
        Some("new-refresh-token".to_string()),
    )
    .expect("update_tokens should succeed");

    let tokens = updated.tokens.expect("tokens should exist");
    assert_eq!(tokens.id_token.raw_jwt, fake_jwt);
    assert_eq!(tokens.access_token, "new-access-token");
    assert_eq!(tokens.refresh_token, "new-refresh-token");
}

#[test]
fn login_with_api_key_overwrites_existing_auth_json() {
    let dir = tempdir().unwrap();
    let auth_path = dir.path().join("auth.json");
    let stale_auth = json!({
        "OPENAI_API_KEY": "sk-old",
        "tokens": {
            "id_token": "stale.header.payload",
            "access_token": "stale-access",
            "refresh_token": "stale-refresh",
            "account_id": "stale-acc"
        }
    });
    std::fs::write(
        &auth_path,
        serde_json::to_string_pretty(&stale_auth).unwrap(),
    )
    .unwrap();

    super::login_with_api_key(dir.path(), "sk-new", AuthCredentialsStoreMode::File)
        .expect("login_with_api_key should succeed");

    let storage = FileAuthStorage::new(dir.path().to_path_buf());
    let auth = storage
        .try_read_auth_json(&auth_path)
        .expect("auth.json should parse");
    assert_eq!(auth.openai_api_key.as_deref(), Some("sk-new"));
    assert!(auth.tokens.is_none(), "tokens should be cleared");
}

#[tokio::test]
async fn login_with_access_token_writes_only_token() {
    let dir = tempdir().unwrap();
    let auth_path = dir.path().join("auth.json");
    let record = agent_identity_record(WORKSPACE_ID_ALLOWED);
    let agent_identity =
        signed_agent_identity_jwt(&record, json!(record.plan_type)).expect("signed agent identity");
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/backend-api/wham/agent-identities/jwks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(test_jwks_body()))
        .expect(1)
        .mount(&server)
        .await;
    let chatgpt_base_url = format!("{}/backend-api", server.uri());

    super::login_with_access_token(
        dir.path(),
        &agent_identity,
        AuthCredentialsStoreMode::File,
        Some(&chatgpt_base_url),
    )
    .await
    .expect("login_with_access_token should succeed");

    let storage = FileAuthStorage::new(dir.path().to_path_buf());
    let auth = storage
        .try_read_auth_json(&auth_path)
        .expect("auth.json should parse");
    assert_eq!(auth.auth_mode, Some(AuthMode::AgentIdentity));
    assert_eq!(
        auth.agent_identity.as_deref(),
        Some(agent_identity.as_str())
    );
    assert!(auth.tokens.is_none(), "tokens should be cleared");
    assert!(auth.openai_api_key.is_none(), "API key should be cleared");
    server.verify().await;
}

#[tokio::test]
async fn login_with_access_token_rejects_invalid_jwt() {
    let dir = tempdir().unwrap();

    let err = super::login_with_access_token(
        dir.path(),
        "not-a-jwt",
        AuthCredentialsStoreMode::File,
        /*chatgpt_base_url*/ None,
    )
    .await
    .expect_err("invalid access token should fail");

    assert_eq!(err.kind(), std::io::ErrorKind::Other);
    assert!(
        !get_auth_file(dir.path()).exists(),
        "invalid access token should not write auth.json"
    );
}

#[tokio::test]
async fn login_with_access_token_rejects_unsigned_jwt() {
    let dir = tempdir().unwrap();
    let record = agent_identity_record(WORKSPACE_ID_ALLOWED);
    let agent_identity = fake_agent_identity_jwt(&record).expect("fake agent identity");
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/backend-api/wham/agent-identities/jwks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(test_jwks_body()))
        .expect(1)
        .mount(&server)
        .await;
    let chatgpt_base_url = format!("{}/backend-api", server.uri());

    super::login_with_access_token(
        dir.path(),
        &agent_identity,
        AuthCredentialsStoreMode::File,
        Some(&chatgpt_base_url),
    )
    .await
    .expect_err("unsigned access token should fail");

    assert!(
        !get_auth_file(dir.path()).exists(),
        "unsigned access token should not write auth.json"
    );
    server.verify().await;
}

#[tokio::test]
#[serial(codex_auth_env)]
async fn missing_auth_json_returns_none() {
    let dir = tempdir().unwrap();
    let _access_token_guard = remove_access_token_env_var();
    let auth = CodexAuth::from_auth_storage(
        dir.path(),
        AuthCredentialsStoreMode::File,
        /*chatgpt_base_url*/ None,
    )
    .await
    .expect("call should succeed");
    assert_eq!(auth, None);
}

#[tokio::test]
#[serial(codex_auth_env)]
async fn startup_pool_selection_keeps_chatgpt_auth_when_pool_does_not_know_account() {
    let codex_home = tempdir().unwrap();
    let _access_token_guard = remove_access_token_env_var();
    let pool = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        None,
    )
    .await
    .expect("pool should open");
    let _jwt = write_auth_file(
        AuthFileParams {
            openai_api_key: None,
            chatgpt_plan_type: Some("pro".to_string()),
            chatgpt_account_id: Some(WORKSPACE_ID_ALLOWED.to_string()),
        },
        codex_home.path(),
    )
    .expect("failed to write auth file");
    let mut pool_managed_auth = CodexAuth::from_auth_storage(
        codex_home.path(),
        AuthCredentialsStoreMode::File,
        /*chatgpt_base_url*/ None,
    )
    .await
    .expect("auth load should succeed")
    .expect("managed auth should exist")
    .get_current_auth_json()
    .expect("auth json should exist");
    pool_managed_auth
        .tokens
        .as_mut()
        .expect("tokens should exist")
        .account_id = Some(WORKSPACE_ID_ALLOWED.to_string());
    save_auth(
        codex_home.path(),
        &pool_managed_auth,
        AuthCredentialsStoreMode::File,
    )
    .expect("updated auth should save");
    let managed_auth = CodexAuth::from_auth_storage(
        codex_home.path(),
        AuthCredentialsStoreMode::File,
        /*chatgpt_base_url*/ None,
    )
    .await
    .expect("auth reload should succeed");

    let selected_auth = load_startup_account_pool_auth(
        codex_home.path(),
        AuthCredentialsStoreMode::File,
        /*chatgpt_base_url*/ None,
        managed_auth,
        Some(&pool),
    )
    .await;

    assert!(selected_auth.is_some());
    assert!(
        get_auth_file(codex_home.path()).exists(),
        "plain auth.json should be kept when the pool has no matching account"
    );
}

#[tokio::test]
#[serial(codex_auth_env)]
async fn startup_pool_selection_keeps_selected_account_after_probe_401() {
    let codex_home = tempdir().unwrap();
    let _access_token_guard = remove_access_token_env_var();

    write_auth_file(
        AuthFileParams {
            openai_api_key: None,
            chatgpt_plan_type: Some("pro".to_string()),
            chatgpt_account_id: Some(WORKSPACE_ID_ALLOWED.to_string()),
        },
        codex_home.path(),
    )
    .expect("failed to write first auth file");
    let mut first_auth = load_auth_dot_json(codex_home.path(), AuthCredentialsStoreMode::File)
        .expect("first auth should load")
        .expect("first auth should exist");
    first_auth
        .tokens
        .as_mut()
        .expect("tokens should exist")
        .account_id = Some(WORKSPACE_ID_ALLOWED.to_string());
    first_auth
        .tokens
        .as_mut()
        .expect("tokens should exist")
        .access_token = fake_access_token(WORKSPACE_ID_ALLOWED, Utc::now().timestamp() + 3600);
    save_auth(
        codex_home.path(),
        &first_auth,
        AuthCredentialsStoreMode::File,
    )
    .expect("first auth should save");
    let pool = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        None,
    )
    .await
    .expect("pool should open for first account");

    write_auth_file(
        AuthFileParams {
            openai_api_key: None,
            chatgpt_plan_type: Some("pro".to_string()),
            chatgpt_account_id: Some(WORKSPACE_ID_SECOND_ALLOWED.to_string()),
        },
        codex_home.path(),
    )
    .expect("failed to write second auth file");
    let mut second_auth = load_auth_dot_json(codex_home.path(), AuthCredentialsStoreMode::File)
        .expect("second auth should load")
        .expect("second auth should exist");
    second_auth
        .tokens
        .as_mut()
        .expect("tokens should exist")
        .account_id = Some(WORKSPACE_ID_SECOND_ALLOWED.to_string());
    second_auth
        .tokens
        .as_mut()
        .expect("tokens should exist")
        .access_token =
        fake_access_token(WORKSPACE_ID_SECOND_ALLOWED, Utc::now().timestamp() + 3600);
    save_auth(
        codex_home.path(),
        &second_auth,
        AuthCredentialsStoreMode::File,
    )
    .expect("second auth should save");
    ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        None,
    )
    .await
    .expect("pool should open for second account");

    save_auth(
        codex_home.path(),
        &first_auth,
        AuthCredentialsStoreMode::File,
    )
    .expect("active auth should be reset to first account");
    let managed_auth = CodexAuth::from_auth_storage(
        codex_home.path(),
        AuthCredentialsStoreMode::File,
        /*chatgpt_base_url*/ None,
    )
    .await
    .expect("managed auth should load");

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/backend-api/wham/usage"))
        .and(header("ChatGPT-Account-Id", WORKSPACE_ID_ALLOWED))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&server)
        .await;
    let selected_auth = load_startup_account_pool_auth(
        codex_home.path(),
        AuthCredentialsStoreMode::File,
        Some(&format!("{}/backend-api", server.uri())),
        managed_auth,
        Some(&pool),
    )
    .await
    .expect("startup should keep the selected account");

    assert_eq!(
        selected_auth.get_account_id().as_deref(),
        Some(WORKSPACE_ID_ALLOWED)
    );

    let accounts = pool.list_accounts().await.expect("accounts should list");
    assert_eq!(
        accounts
            .iter()
            .find(|account| account.account_id == WORKSPACE_ID_ALLOWED)
            .expect("first account should remain in pool")
            .auth_status,
        ChatgptAccountPoolAuthStatus::Valid
    );
    assert_eq!(
        load_auth_dot_json(codex_home.path(), AuthCredentialsStoreMode::File)
            .expect("active auth should load")
            .expect("active auth should exist")
            .tokens
            .expect("tokens should exist")
            .account_id
            .as_deref(),
        Some(WORKSPACE_ID_ALLOWED)
    );
    server.verify().await;
}

#[tokio::test]
#[serial(codex_auth_env)]
async fn startup_pool_selection_refreshes_selected_account_after_probe_401() {
    let codex_home = tempdir().unwrap();
    let _access_token_guard = remove_access_token_env_var();

    write_auth_file(
        AuthFileParams {
            openai_api_key: None,
            chatgpt_plan_type: Some("pro".to_string()),
            chatgpt_account_id: Some(WORKSPACE_ID_ALLOWED.to_string()),
        },
        codex_home.path(),
    )
    .expect("failed to write auth file");
    let mut auth_dot_json = load_auth_dot_json(codex_home.path(), AuthCredentialsStoreMode::File)
        .expect("auth should load")
        .expect("auth should exist");
    auth_dot_json
        .tokens
        .as_mut()
        .expect("tokens should exist")
        .account_id = Some(WORKSPACE_ID_ALLOWED.to_string());
    auth_dot_json
        .tokens
        .as_mut()
        .expect("tokens should exist")
        .access_token = fake_access_token(WORKSPACE_ID_ALLOWED, Utc::now().timestamp() + 3600);
    auth_dot_json
        .tokens
        .as_mut()
        .expect("tokens should exist")
        .refresh_token = "refresh-initial".to_string();
    save_auth(
        codex_home.path(),
        &auth_dot_json,
        AuthCredentialsStoreMode::File,
    )
    .expect("auth should save");
    let pool = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        None,
    )
    .await
    .expect("pool should open");
    let managed_auth = CodexAuth::from_auth_storage(
        codex_home.path(),
        AuthCredentialsStoreMode::File,
        /*chatgpt_base_url*/ None,
    )
    .await
    .expect("managed auth should load");

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/backend-api/wham/usage"))
        .and(header("ChatGPT-Account-Id", WORKSPACE_ID_ALLOWED))
        .and(header(
            "Authorization",
            format!(
                "Bearer {}",
                auth_dot_json.tokens.as_ref().expect("tokens").access_token
            ),
        ))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&server)
        .await;

    let selected_auth = load_startup_account_pool_auth(
        codex_home.path(),
        AuthCredentialsStoreMode::File,
        Some(&format!("{}/backend-api", server.uri())),
        managed_auth,
        Some(&pool),
    )
    .await
    .expect("startup should keep the selected account");

    assert_eq!(
        selected_auth.get_account_id().as_deref(),
        Some(WORKSPACE_ID_ALLOWED)
    );
    assert_eq!(
        selected_auth
            .get_current_auth_json()
            .expect("auth json should exist")
            .tokens
            .expect("tokens should exist")
            .refresh_token,
        "refresh-initial".to_string()
    );

    let account = pool
        .list_accounts()
        .await
        .expect("accounts should load")
        .into_iter()
        .find(|account| account.account_id == WORKSPACE_ID_ALLOWED)
        .expect("selected account should remain in pool");
    assert_eq!(account.auth_status, ChatgptAccountPoolAuthStatus::Valid);
    server.verify().await;
}

#[tokio::test]
#[serial(codex_auth_env)]
async fn startup_pool_selection_does_not_return_401_auth_after_transient_refresh_failure() {
    let codex_home = tempdir().unwrap();
    let _access_token_guard = remove_access_token_env_var();

    write_auth_file(
        AuthFileParams {
            openai_api_key: None,
            chatgpt_plan_type: Some("pro".to_string()),
            chatgpt_account_id: Some(WORKSPACE_ID_ALLOWED.to_string()),
        },
        codex_home.path(),
    )
    .expect("failed to write auth file");
    let mut auth_dot_json = load_auth_dot_json(codex_home.path(), AuthCredentialsStoreMode::File)
        .expect("auth should load")
        .expect("auth should exist");
    auth_dot_json
        .tokens
        .as_mut()
        .expect("tokens should exist")
        .account_id = Some(WORKSPACE_ID_ALLOWED.to_string());
    auth_dot_json
        .tokens
        .as_mut()
        .expect("tokens should exist")
        .access_token = fake_access_token(WORKSPACE_ID_ALLOWED, Utc::now().timestamp() + 3600);
    auth_dot_json
        .tokens
        .as_mut()
        .expect("tokens should exist")
        .refresh_token = "refresh-initial".to_string();
    save_auth(
        codex_home.path(),
        &auth_dot_json,
        AuthCredentialsStoreMode::File,
    )
    .expect("auth should save");
    let pool = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        None,
    )
    .await
    .expect("pool should open");
    let managed_auth = CodexAuth::from_auth_storage(
        codex_home.path(),
        AuthCredentialsStoreMode::File,
        /*chatgpt_base_url*/ None,
    )
    .await
    .expect("managed auth should load");

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/backend-api/wham/usage"))
        .and(header("ChatGPT-Account-Id", WORKSPACE_ID_ALLOWED))
        .and(header(
            "Authorization",
            format!(
                "Bearer {}",
                auth_dot_json.tokens.as_ref().expect("tokens").access_token
            ),
        ))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&server)
        .await;

    let selected_auth = load_startup_account_pool_auth(
        codex_home.path(),
        AuthCredentialsStoreMode::File,
        Some(&format!("{}/backend-api", server.uri())),
        managed_auth,
        Some(&pool),
    )
    .await;

    assert_eq!(
        selected_auth
            .as_ref()
            .and_then(CodexAuth::get_account_id)
            .as_deref(),
        Some(WORKSPACE_ID_ALLOWED)
    );
    let account = pool
        .list_accounts()
        .await
        .expect("accounts should load")
        .into_iter()
        .find(|account| account.account_id == WORKSPACE_ID_ALLOWED)
        .expect("selected account should remain in pool");
    assert_eq!(account.auth_status, ChatgptAccountPoolAuthStatus::Valid);
    server.verify().await;
}

#[tokio::test]
#[serial(codex_auth_env)]
async fn startup_pool_selection_keeps_managed_auth_when_pool_selection_errors() {
    let codex_home = tempdir().unwrap();
    let _access_token_guard = remove_access_token_env_var();

    write_auth_file(
        AuthFileParams {
            openai_api_key: None,
            chatgpt_plan_type: Some("pro".to_string()),
            chatgpt_account_id: Some(WORKSPACE_ID_ALLOWED.to_string()),
        },
        codex_home.path(),
    )
    .expect("failed to write auth file");
    let mut auth_dot_json = load_auth_dot_json(codex_home.path(), AuthCredentialsStoreMode::File)
        .expect("auth should load")
        .expect("auth should exist");
    auth_dot_json
        .tokens
        .as_mut()
        .expect("tokens should exist")
        .account_id = Some(WORKSPACE_ID_ALLOWED.to_string());
    save_auth(
        codex_home.path(),
        &auth_dot_json,
        AuthCredentialsStoreMode::File,
    )
    .expect("auth should save");
    let pool = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        None,
    )
    .await
    .expect("pool should open");
    let managed_auth = CodexAuth::from_auth_storage(
        codex_home.path(),
        AuthCredentialsStoreMode::File,
        /*chatgpt_base_url*/ None,
    )
    .await
    .expect("managed auth load should succeed");

    let mut conn = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(crate::account_pool_db_path(codex_home.path()))
            .create_if_missing(false),
    )
    .await
    .expect("schema connection should open");
    sqlx::query("DROP TABLE accounts")
        .execute(&mut conn)
        .await
        .expect("accounts table should drop");

    let selected_auth = load_startup_account_pool_auth(
        codex_home.path(),
        AuthCredentialsStoreMode::File,
        /*chatgpt_base_url*/ None,
        managed_auth.clone(),
        Some(&pool),
    )
    .await;

    assert_eq!(selected_auth, managed_auth);
}

#[tokio::test]
#[serial(codex_auth_env)]
async fn pro_account_with_no_api_key_uses_chatgpt_auth() {
    let codex_home = tempdir().unwrap();
    let _access_token_guard = remove_access_token_env_var();
    let fake_jwt = write_auth_file(
        AuthFileParams {
            openai_api_key: None,
            chatgpt_plan_type: Some("pro".to_string()),
            chatgpt_account_id: None,
        },
        codex_home.path(),
    )
    .expect("failed to write auth file");

    let auth = super::load_auth(
        codex_home.path(),
        /*enable_codex_api_key_env*/ false,
        AuthCredentialsStoreMode::File,
        /*chatgpt_base_url*/ None,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(None, auth.api_key());
    assert_eq!(AuthMode::Chatgpt, auth.auth_mode());
    assert_eq!(auth.get_chatgpt_user_id().as_deref(), Some("user-12345"));

    let auth_dot_json = auth
        .get_current_auth_json()
        .expect("AuthDotJson should exist");
    let last_refresh = auth_dot_json
        .last_refresh
        .expect("last_refresh should be recorded");

    assert_eq!(
        AuthDotJson {
            auth_mode: None,
            openai_api_key: None,
            tokens: Some(TokenData {
                id_token: IdTokenInfo {
                    email: Some("user@example.com".to_string()),
                    chatgpt_plan_type: Some(InternalPlanType::Known(InternalKnownPlan::Pro)),
                    chatgpt_user_id: Some("user-12345".to_string()),
                    subject: None,
                    chatgpt_account_id: None,
                    chatgpt_account_is_fedramp: false,
                    raw_jwt: fake_jwt,
                },
                access_token: "test-access-token".to_string(),
                refresh_token: "test-refresh-token".to_string(),
                account_id: None,
            }),
            pool_account_id: None,
            last_refresh: Some(last_refresh),
            agent_identity: None,
        },
        auth_dot_json
    );
}

#[tokio::test]
#[serial(codex_auth_env)]
async fn loads_api_key_from_auth_json() {
    let dir = tempdir().unwrap();
    let _access_token_guard = remove_access_token_env_var();
    let auth_file = dir.path().join("auth.json");
    std::fs::write(
        auth_file,
        r#"{"OPENAI_API_KEY":"sk-test-key","tokens":null,"last_refresh":null}"#,
    )
    .unwrap();

    let auth = super::load_auth(
        dir.path(),
        /*enable_codex_api_key_env*/ false,
        AuthCredentialsStoreMode::File,
        /*chatgpt_base_url*/ None,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(auth.auth_mode(), AuthMode::ApiKey);
    assert_eq!(auth.api_key(), Some("sk-test-key"));

    assert!(auth.get_token_data().is_err());
}

#[test]
fn logout_removes_auth_file() -> Result<(), std::io::Error> {
    let dir = tempdir()?;
    let auth_dot_json = AuthDotJson {
        auth_mode: Some(ApiAuthMode::ApiKey),
        openai_api_key: Some("sk-test-key".to_string()),
        tokens: None,
        pool_account_id: None,
        last_refresh: None,
        agent_identity: None,
    };
    super::save_auth(dir.path(), &auth_dot_json, AuthCredentialsStoreMode::File)?;
    let auth_file = get_auth_file(dir.path());
    assert!(auth_file.exists());
    assert!(logout(dir.path(), AuthCredentialsStoreMode::File)?);
    assert!(!auth_file.exists());
    Ok(())
}

#[tokio::test]
#[serial(codex_auth_env)]
async fn startup_does_not_reactivate_logged_out_account_pool_auth() {
    let codex_home = tempdir().unwrap();
    let _access_token_guard = remove_access_token_env_var();

    write_auth_file(
        AuthFileParams {
            openai_api_key: None,
            chatgpt_plan_type: Some("pro".to_string()),
            chatgpt_account_id: Some(WORKSPACE_ID_ALLOWED.to_string()),
        },
        codex_home.path(),
    )
    .expect("failed to write auth file");
    let mut auth_dot_json = load_auth_dot_json(codex_home.path(), AuthCredentialsStoreMode::File)
        .expect("auth should load")
        .expect("auth should exist");
    auth_dot_json
        .tokens
        .as_mut()
        .expect("tokens should exist")
        .account_id = Some(WORKSPACE_ID_ALLOWED.to_string());
    save_auth(
        codex_home.path(),
        &auth_dot_json,
        AuthCredentialsStoreMode::File,
    )
    .expect("auth should save");
    ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        None,
    )
    .await
    .expect("pool should open");

    let manager = AuthManager::new(
        codex_home.path().to_path_buf(),
        /*enable_codex_api_key_env*/ false,
        AuthCredentialsStoreMode::File,
        /*chatgpt_base_url*/ None,
    )
    .await;
    assert!(manager.auth_cached().is_some(), "auth should be cached");

    assert!(manager.logout().await.expect("logout should succeed"));
    assert!(
        manager.auth_cached().is_none(),
        "logout should clear cached auth"
    );
    assert!(
        !get_auth_file(codex_home.path()).exists(),
        "logout should keep auth.json deleted"
    );

    let restarted = AuthManager::new(
        codex_home.path().to_path_buf(),
        /*enable_codex_api_key_env*/ false,
        AuthCredentialsStoreMode::File,
        /*chatgpt_base_url*/ None,
    )
    .await;
    assert!(
        restarted.auth_cached().is_none(),
        "startup should not reactivate logged-out pool auth"
    );
    assert!(
        !get_auth_file(codex_home.path()).exists(),
        "startup should not rewrite auth.json from the pool"
    );
}

#[tokio::test]
#[serial(codex_auth_env)]
async fn prepare_chatgpt_account_pool_for_turn_skips_pool_after_logout() {
    let codex_home = tempdir().unwrap();
    let _access_token_guard = remove_access_token_env_var();

    write_auth_file(
        AuthFileParams {
            openai_api_key: None,
            chatgpt_plan_type: Some("pro".to_string()),
            chatgpt_account_id: Some(WORKSPACE_ID_ALLOWED.to_string()),
        },
        codex_home.path(),
    )
    .expect("failed to write auth file");
    let mut auth_dot_json = load_auth_dot_json(codex_home.path(), AuthCredentialsStoreMode::File)
        .expect("auth should load")
        .expect("auth should exist");
    auth_dot_json
        .tokens
        .as_mut()
        .expect("tokens should exist")
        .account_id = Some(WORKSPACE_ID_ALLOWED.to_string());
    save_auth(
        codex_home.path(),
        &auth_dot_json,
        AuthCredentialsStoreMode::File,
    )
    .expect("auth should save");
    ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        None,
    )
    .await
    .expect("pool should open");
    let pool_secret_home = crate::account_pool_secret_dir(codex_home.path(), WORKSPACE_ID_ALLOWED);

    let manager = AuthManager::new(
        codex_home.path().to_path_buf(),
        /*enable_codex_api_key_env*/ false,
        AuthCredentialsStoreMode::File,
        /*chatgpt_base_url*/ None,
    )
    .await;
    assert!(manager.logout().await.expect("logout should succeed"));
    std::fs::remove_file(pool_secret_home.join("auth.json"))
        .expect("pool secret should be removable");

    manager
        .prepare_chatgpt_account_pool_for_turn()
        .await
        .expect("auth-free turns should skip account-pool selection after logout");
    assert!(
        manager.auth_cached().is_none(),
        "turn preparation should not reactivate logged-out auth"
    );
    assert!(
        !get_auth_file(codex_home.path()).exists(),
        "turn preparation should not recreate auth.json"
    );
}

#[tokio::test]
async fn unauthorized_recovery_reports_mode_and_step_names() {
    let dir = tempdir().unwrap();
    let manager = AuthManager::shared(
        dir.path().to_path_buf(),
        /*enable_codex_api_key_env*/ false,
        AuthCredentialsStoreMode::File,
        /*chatgpt_base_url*/ None,
    )
    .await;
    let managed = UnauthorizedRecovery {
        manager: Arc::clone(&manager),
        step: UnauthorizedRecoveryStep::Reload,
        expected_account_id: None,
        mode: UnauthorizedRecoveryMode::Managed,
    };
    assert_eq!(managed.mode_name(), "managed");
    assert_eq!(managed.step_name(), "reload");

    let external = UnauthorizedRecovery {
        manager,
        step: UnauthorizedRecoveryStep::ExternalRefresh,
        expected_account_id: None,
        mode: UnauthorizedRecoveryMode::External,
    };
    assert_eq!(external.mode_name(), "external");
    assert_eq!(external.step_name(), "external_refresh");
}

#[tokio::test]
#[serial(codex_auth_env)]
async fn refresh_failure_is_scoped_to_the_matching_auth_snapshot() {
    let codex_home = tempdir().unwrap();
    let _access_token_guard = remove_access_token_env_var();
    write_auth_file(
        AuthFileParams {
            openai_api_key: None,
            chatgpt_plan_type: Some("pro".to_string()),
            chatgpt_account_id: Some(WORKSPACE_ID_ALLOWED.to_string()),
        },
        codex_home.path(),
    )
    .expect("failed to write auth file");

    let auth = super::load_auth(
        codex_home.path(),
        /*enable_codex_api_key_env*/ false,
        AuthCredentialsStoreMode::File,
        /*chatgpt_base_url*/ None,
    )
    .await
    .expect("load auth")
    .expect("auth available");
    let mut updated_auth_dot_json = auth
        .get_current_auth_json()
        .expect("AuthDotJson should exist");
    let updated_tokens = updated_auth_dot_json
        .tokens
        .as_mut()
        .expect("tokens should exist");
    updated_tokens.access_token = "new-access-token".to_string();
    updated_tokens.refresh_token = "new-refresh-token".to_string();
    let updated_auth = CodexAuth::from_auth_dot_json(
        codex_home.path(),
        updated_auth_dot_json,
        AuthCredentialsStoreMode::File,
        /*chatgpt_base_url*/ None,
    )
    .await
    .expect("updated auth should parse");

    let manager = AuthManager::from_auth_for_testing(auth.clone());
    let error = RefreshTokenFailedError::new(
        RefreshTokenFailedReason::Exhausted,
        "refresh token already used",
    );
    manager.record_permanent_refresh_failure_if_unchanged(&auth, &error);

    assert_eq!(manager.refresh_failure_for_auth(&auth), Some(error));
    assert_eq!(manager.refresh_failure_for_auth(&updated_auth), None);
}

#[tokio::test]
#[serial(codex_auth_env)]
async fn pool_managed_auth_failure_marks_cached_refresh_failure() {
    let codex_home = tempdir().unwrap();
    let _access_token_guard = remove_access_token_env_var();
    let server = MockServer::start().await;
    mount_usage_probe(&server, WORKSPACE_ID_ALLOWED, 200).await;
    write_auth_file(
        AuthFileParams {
            openai_api_key: None,
            chatgpt_plan_type: Some("pro".to_string()),
            chatgpt_account_id: Some(WORKSPACE_ID_ALLOWED.to_string()),
        },
        codex_home.path(),
    )
    .expect("failed to write auth file");
    let mut auth_dot_json = load_auth_dot_json(codex_home.path(), AuthCredentialsStoreMode::File)
        .expect("auth should load")
        .expect("auth should exist");
    auth_dot_json
        .tokens
        .as_mut()
        .expect("tokens should exist")
        .account_id = Some(WORKSPACE_ID_ALLOWED.to_string());
    save_auth(
        codex_home.path(),
        &auth_dot_json,
        AuthCredentialsStoreMode::File,
    )
    .expect("updated auth should save");

    let manager = AuthManager::new(
        codex_home.path().to_path_buf(),
        /*enable_codex_api_key_env*/ false,
        AuthCredentialsStoreMode::File,
        Some(chatgpt_backend_base_url(&server)),
    )
    .await;
    let auth = manager.auth_cached().expect("auth should be cached");
    let error = RefreshTokenFailedError::new(
        RefreshTokenFailedReason::Exhausted,
        "refresh token already used",
    );

    let failover = manager
        .handle_chatgpt_account_pool_auth_failure(/*safe_to_retry*/ false, &error)
        .await
        .expect("auth failure handling should succeed");

    assert!(!failover);
    assert_eq!(manager.refresh_failure_for_auth(&auth), Some(error));
}

#[tokio::test]
#[serial(codex_auth_env)]
async fn non_forced_pool_refresh_reloads_newer_pool_copy_even_when_cached_token_looks_fresh() {
    let codex_home = tempdir().unwrap();
    let _access_token_guard = remove_access_token_env_var();
    let server = MockServer::start().await;
    mount_usage_probe(&server, WORKSPACE_ID_ALLOWED, 200).await;
    write_auth_file(
        AuthFileParams {
            openai_api_key: None,
            chatgpt_plan_type: Some("pro".to_string()),
            chatgpt_account_id: Some(WORKSPACE_ID_ALLOWED.to_string()),
        },
        codex_home.path(),
    )
    .expect("failed to write auth file");

    let mut active_auth_dot_json =
        load_auth_dot_json(codex_home.path(), AuthCredentialsStoreMode::File)
            .expect("auth should load")
            .expect("auth should exist");
    let initial_access_token =
        fake_access_token(WORKSPACE_ID_ALLOWED, Utc::now().timestamp() + 3600);
    let active_tokens = active_auth_dot_json
        .tokens
        .as_mut()
        .expect("tokens should exist");
    active_tokens.access_token = initial_access_token.clone();
    active_tokens.account_id = Some(WORKSPACE_ID_ALLOWED.to_string());
    save_auth(
        codex_home.path(),
        &active_auth_dot_json,
        AuthCredentialsStoreMode::File,
    )
    .expect("updated auth should save");

    let manager = AuthManager::new(
        codex_home.path().to_path_buf(),
        /*enable_codex_api_key_env*/ false,
        AuthCredentialsStoreMode::File,
        Some(chatgpt_backend_base_url(&server)),
    )
    .await;
    let cached_before = manager.auth_cached().expect("auth should be cached");
    assert_eq!(
        cached_before
            .get_current_auth_json()
            .expect("auth json should exist")
            .tokens
            .expect("tokens should exist")
            .access_token,
        initial_access_token
    );

    let pool_secret_home = crate::account_pool_secret_dir(codex_home.path(), WORKSPACE_ID_ALLOWED);
    let mut pool_auth_dot_json =
        load_auth_dot_json(&pool_secret_home, AuthCredentialsStoreMode::File)
            .expect("pool auth should load")
            .expect("pool auth should exist");
    let reloaded_access_token =
        fake_access_token(WORKSPACE_ID_ALLOWED, Utc::now().timestamp() + 7200);
    pool_auth_dot_json
        .tokens
        .as_mut()
        .expect("tokens should exist")
        .access_token = reloaded_access_token.clone();
    save_auth(
        &pool_secret_home,
        &pool_auth_dot_json,
        AuthCredentialsStoreMode::File,
    )
    .expect("pool auth should save");

    manager
        .refresh_token_from_authority()
        .await
        .expect("non-forced refresh should reload newer pool copy");

    let cached_after = manager.auth_cached().expect("auth should remain cached");
    assert_eq!(
        cached_after
            .get_current_auth_json()
            .expect("auth json should exist")
            .tokens
            .expect("tokens should exist")
            .access_token,
        reloaded_access_token
    );
}

#[tokio::test]
#[serial(codex_auth_env)]
async fn auth_keeps_current_pool_managed_account_until_this_process_hits_failure() {
    let codex_home = tempdir().unwrap();
    let _access_token_guard = remove_access_token_env_var();
    let server = MockServer::start().await;
    mount_usage_probe(&server, WORKSPACE_ID_ALLOWED, 200).await;

    write_auth_file(
        AuthFileParams {
            openai_api_key: None,
            chatgpt_plan_type: Some("pro".to_string()),
            chatgpt_account_id: Some(WORKSPACE_ID_ALLOWED.to_string()),
        },
        codex_home.path(),
    )
    .expect("failed to write first auth file");
    let mut first_auth = load_auth_dot_json(codex_home.path(), AuthCredentialsStoreMode::File)
        .expect("first auth should load")
        .expect("first auth should exist");
    first_auth
        .tokens
        .as_mut()
        .expect("tokens should exist")
        .account_id = Some(WORKSPACE_ID_ALLOWED.to_string());
    first_auth
        .tokens
        .as_mut()
        .expect("tokens should exist")
        .access_token = fake_access_token(WORKSPACE_ID_ALLOWED, Utc::now().timestamp() + 3600);
    save_auth(
        codex_home.path(),
        &first_auth,
        AuthCredentialsStoreMode::File,
    )
    .expect("first auth should save");
    let pool = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        None,
    )
    .await
    .expect("pool should open for first account");

    write_auth_file(
        AuthFileParams {
            openai_api_key: None,
            chatgpt_plan_type: Some("pro".to_string()),
            chatgpt_account_id: Some(WORKSPACE_ID_SECOND_ALLOWED.to_string()),
        },
        codex_home.path(),
    )
    .expect("failed to write second auth file");
    let mut second_auth = load_auth_dot_json(codex_home.path(), AuthCredentialsStoreMode::File)
        .expect("second auth should load")
        .expect("second auth should exist");
    second_auth
        .tokens
        .as_mut()
        .expect("tokens should exist")
        .account_id = Some(WORKSPACE_ID_SECOND_ALLOWED.to_string());
    second_auth
        .tokens
        .as_mut()
        .expect("tokens should exist")
        .access_token =
        fake_access_token(WORKSPACE_ID_SECOND_ALLOWED, Utc::now().timestamp() + 3600);
    save_auth(
        codex_home.path(),
        &second_auth,
        AuthCredentialsStoreMode::File,
    )
    .expect("second auth should save");
    ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        None,
    )
    .await
    .expect("pool should open for second account");

    save_auth(
        codex_home.path(),
        &first_auth,
        AuthCredentialsStoreMode::File,
    )
    .expect("active auth should be reset to first account");
    let manager = AuthManager::new(
        codex_home.path().to_path_buf(),
        /*enable_codex_api_key_env*/ false,
        AuthCredentialsStoreMode::File,
        Some(chatgpt_backend_base_url(&server)),
    )
    .await;

    pool.mark_current_account_rate_limited(
        WORKSPACE_ID_ALLOWED,
        Some(&RateLimitSnapshot {
            limit_id: Some("codex".to_string()),
            limit_name: None,
            primary: Some(codex_protocol::protocol::RateLimitWindow {
                used_percent: 100.0,
                window_minutes: Some(300),
                resets_at: Some(Utc::now().timestamp() + 3600),
            }),
            secondary: None,
            credits: None,
            plan_type: None,
            rate_limit_reached_type: Some(
                codex_protocol::protocol::RateLimitReachedType::RateLimitReached,
            ),
        }),
        None,
    )
    .await
    .expect("current account should become rate limited");
    let selection = pool
        .resolve_turn_selection(Some(WORKSPACE_ID_ALLOWED), false)
        .await
        .expect("fallback selection should succeed");
    let ChatgptAccountPoolSelectionOutcome::Activated {
        account_id,
        failover,
        ..
    } = selection
    else {
        panic!("expected fallback activation");
    };
    assert_eq!(account_id, WORKSPACE_ID_SECOND_ALLOWED);
    assert!(failover);

    let auth = manager.auth().await.expect("auth should still resolve");
    assert_eq!(auth.get_account_id().as_deref(), Some(WORKSPACE_ID_ALLOWED));
    assert_eq!(
        manager
            .auth_cached()
            .expect("cached auth should remain on the original account")
            .get_account_id()
            .as_deref(),
        Some(WORKSPACE_ID_ALLOWED)
    );
}

#[tokio::test]
#[serial(codex_auth_env)]
async fn non_forced_pool_refresh_reloads_newer_pool_copy_with_ephemeral_store() {
    let codex_home = tempdir().unwrap();
    let _access_token_guard = remove_access_token_env_var();
    let server = MockServer::start().await;
    mount_usage_probe(&server, WORKSPACE_ID_ALLOWED, 200).await;
    write_auth_file(
        AuthFileParams {
            openai_api_key: None,
            chatgpt_plan_type: Some("pro".to_string()),
            chatgpt_account_id: Some(WORKSPACE_ID_ALLOWED.to_string()),
        },
        codex_home.path(),
    )
    .expect("failed to write auth file");

    let mut active_auth_dot_json =
        load_auth_dot_json(codex_home.path(), AuthCredentialsStoreMode::File)
            .expect("auth should load")
            .expect("auth should exist");
    let initial_access_token =
        fake_access_token(WORKSPACE_ID_ALLOWED, Utc::now().timestamp() + 3600);
    let active_tokens = active_auth_dot_json
        .tokens
        .as_mut()
        .expect("tokens should exist");
    active_tokens.access_token = initial_access_token.clone();
    active_tokens.account_id = Some(WORKSPACE_ID_ALLOWED.to_string());
    save_auth(
        codex_home.path(),
        &active_auth_dot_json,
        AuthCredentialsStoreMode::Ephemeral,
    )
    .expect("updated auth should save");

    let manager = AuthManager::new(
        codex_home.path().to_path_buf(),
        /*enable_codex_api_key_env*/ false,
        AuthCredentialsStoreMode::Ephemeral,
        Some(chatgpt_backend_base_url(&server)),
    )
    .await;
    let cached_before = manager.auth_cached().expect("auth should be cached");
    assert_eq!(
        cached_before
            .get_current_auth_json()
            .expect("auth json should exist")
            .tokens
            .expect("tokens should exist")
            .access_token,
        initial_access_token
    );

    let pool_secret_home = crate::account_pool_secret_dir(codex_home.path(), WORKSPACE_ID_ALLOWED);
    let mut pool_auth_dot_json =
        load_auth_dot_json(&pool_secret_home, AuthCredentialsStoreMode::Ephemeral)
            .expect("pool auth should load")
            .expect("pool auth should exist");
    let reloaded_access_token =
        fake_access_token(WORKSPACE_ID_ALLOWED, Utc::now().timestamp() + 7200);
    pool_auth_dot_json
        .tokens
        .as_mut()
        .expect("tokens should exist")
        .access_token = reloaded_access_token.clone();
    save_auth(
        &pool_secret_home,
        &pool_auth_dot_json,
        AuthCredentialsStoreMode::Ephemeral,
    )
    .expect("pool auth should save");

    manager
        .refresh_token_from_authority()
        .await
        .expect("non-forced refresh should reload newer pool copy");

    let cached_after = manager.auth_cached().expect("auth should remain cached");
    assert_eq!(
        cached_after
            .get_current_auth_json()
            .expect("auth json should exist")
            .tokens
            .expect("tokens should exist")
            .access_token,
        reloaded_access_token
    );
}

#[tokio::test]
#[serial(codex_auth_env)]
async fn pool_managed_refresh_fails_over_when_current_pool_secret_is_missing() {
    let codex_home = tempdir().unwrap();
    let _access_token_guard = remove_access_token_env_var();
    let server = MockServer::start().await;
    mount_usage_probe(&server, WORKSPACE_ID_ALLOWED, 200).await;

    write_auth_file(
        AuthFileParams {
            openai_api_key: None,
            chatgpt_plan_type: Some("pro".to_string()),
            chatgpt_account_id: Some(WORKSPACE_ID_ALLOWED.to_string()),
        },
        codex_home.path(),
    )
    .expect("failed to write first auth file");
    let mut first_auth = load_auth_dot_json(codex_home.path(), AuthCredentialsStoreMode::File)
        .expect("first auth should load")
        .expect("first auth should exist");
    first_auth
        .tokens
        .as_mut()
        .expect("tokens should exist")
        .account_id = Some(WORKSPACE_ID_ALLOWED.to_string());
    first_auth
        .tokens
        .as_mut()
        .expect("tokens should exist")
        .access_token = fake_access_token(WORKSPACE_ID_ALLOWED, Utc::now().timestamp() - 60);
    save_auth(
        codex_home.path(),
        &first_auth,
        AuthCredentialsStoreMode::File,
    )
    .expect("first auth should save");
    ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        None,
    )
    .await
    .expect("pool should open for first account");

    write_auth_file(
        AuthFileParams {
            openai_api_key: None,
            chatgpt_plan_type: Some("pro".to_string()),
            chatgpt_account_id: Some(WORKSPACE_ID_SECOND_ALLOWED.to_string()),
        },
        codex_home.path(),
    )
    .expect("failed to write second auth file");
    let mut second_auth = load_auth_dot_json(codex_home.path(), AuthCredentialsStoreMode::File)
        .expect("second auth should load")
        .expect("second auth should exist");
    second_auth
        .tokens
        .as_mut()
        .expect("tokens should exist")
        .account_id = Some(WORKSPACE_ID_SECOND_ALLOWED.to_string());
    second_auth
        .tokens
        .as_mut()
        .expect("tokens should exist")
        .access_token =
        fake_access_token(WORKSPACE_ID_SECOND_ALLOWED, Utc::now().timestamp() + 3600);
    save_auth(
        codex_home.path(),
        &second_auth,
        AuthCredentialsStoreMode::File,
    )
    .expect("second auth should save");
    ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        None,
    )
    .await
    .expect("pool should open for second account");

    save_auth(
        codex_home.path(),
        &first_auth,
        AuthCredentialsStoreMode::File,
    )
    .expect("active auth should be reset to first account");
    std::fs::remove_file(
        crate::account_pool_secret_dir(codex_home.path(), WORKSPACE_ID_ALLOWED).join("auth.json"),
    )
    .expect("first pool secret should be removable");

    let manager = AuthManager::new(
        codex_home.path().to_path_buf(),
        /*enable_codex_api_key_env*/ false,
        AuthCredentialsStoreMode::File,
        Some(chatgpt_backend_base_url(&server)),
    )
    .await;

    manager
        .refresh_token_from_authority()
        .await
        .expect("refresh should fail over to the other pool account");

    let cached_after = manager.auth_cached().expect("auth should remain cached");
    assert_eq!(
        cached_after.get_account_id().as_deref(),
        Some(WORKSPACE_ID_SECOND_ALLOWED)
    );
}

#[test]
fn external_auth_tokens_without_chatgpt_metadata_cannot_seed_chatgpt_auth() {
    let err = AuthDotJson::from_external_tokens(&ExternalAuthTokens::access_token_only(
        "test-access-token",
    ))
    .expect_err("bearer-only external auth should not seed ChatGPT auth");

    assert_eq!(
        err.to_string(),
        "external auth tokens are missing ChatGPT metadata"
    );
}

#[tokio::test]
async fn external_bearer_only_auth_manager_uses_cached_provider_token() {
    let script = ProviderAuthScript::new(&["provider-token", "next-token"]).unwrap();
    let manager = AuthManager::external_bearer_only(script.auth_config());

    let first = manager
        .auth()
        .await
        .and_then(|auth| auth.api_key().map(str::to_string));
    let second = manager
        .auth()
        .await
        .and_then(|auth| auth.api_key().map(str::to_string));

    assert_eq!(first.as_deref(), Some("provider-token"));
    assert_eq!(second.as_deref(), Some("provider-token"));
    assert_eq!(manager.auth_mode(), Some(AuthMode::ApiKey));
    assert_eq!(manager.get_api_auth_mode(), Some(ApiAuthMode::ApiKey));
}

#[tokio::test]
async fn external_bearer_only_auth_manager_disables_auto_refresh_when_interval_is_zero() {
    let script = ProviderAuthScript::new(&["provider-token", "next-token"]).unwrap();
    let mut auth_config = script.auth_config();
    auth_config.refresh_interval_ms = 0;
    let manager = AuthManager::external_bearer_only(auth_config);

    let first = manager
        .auth()
        .await
        .and_then(|auth| auth.api_key().map(str::to_string));
    let second = manager
        .auth()
        .await
        .and_then(|auth| auth.api_key().map(str::to_string));

    assert_eq!(first.as_deref(), Some("provider-token"));
    assert_eq!(second.as_deref(), Some("provider-token"));
}

#[tokio::test]
async fn external_bearer_only_auth_manager_returns_none_when_command_fails() {
    let script = ProviderAuthScript::new_failing().unwrap();
    let manager = AuthManager::external_bearer_only(script.auth_config());

    assert_eq!(manager.auth().await, None);
}

#[tokio::test]
async fn unauthorized_recovery_uses_external_refresh_for_bearer_manager() {
    let script = ProviderAuthScript::new(&["provider-token", "refreshed-provider-token"]).unwrap();
    let mut auth_config = script.auth_config();
    auth_config.refresh_interval_ms = 0;
    let manager = AuthManager::external_bearer_only(auth_config);
    let initial_token = manager
        .auth()
        .await
        .and_then(|auth| auth.api_key().map(str::to_string));
    let mut recovery = manager.unauthorized_recovery();

    assert!(recovery.has_next());
    assert_eq!(recovery.mode_name(), "external");
    assert_eq!(recovery.step_name(), "external_refresh");

    let result = recovery
        .next()
        .await
        .expect("external refresh should succeed");

    assert_eq!(result.auth_state_changed(), Some(true));
    let refreshed_token = manager
        .auth()
        .await
        .and_then(|auth| auth.api_key().map(str::to_string));
    assert_eq!(initial_token.as_deref(), Some("provider-token"));
    assert_eq!(refreshed_token.as_deref(), Some("refreshed-provider-token"));
}

struct ProviderAuthScript {
    tempdir: TempDir,
    command: String,
    args: Vec<String>,
}

impl ProviderAuthScript {
    fn new(tokens: &[&str]) -> std::io::Result<Self> {
        let tempdir = tempfile::tempdir()?;
        let token_file = tempdir.path().join("tokens.txt");
        // `cmd.exe`'s `set /p` treats LF-only input as one line, so use CRLF on Windows.
        let token_line_ending = if cfg!(windows) { "\r\n" } else { "\n" };
        let mut token_file_contents = String::new();
        for token in tokens {
            token_file_contents.push_str(token);
            token_file_contents.push_str(token_line_ending);
        }
        std::fs::write(&token_file, token_file_contents)?;

        #[cfg(unix)]
        let (command, args) = {
            let script_path = tempdir.path().join("print-token.sh");
            std::fs::write(
                &script_path,
                r#"#!/bin/sh
first_line=$(sed -n '1p' tokens.txt)
printf '%s\n' "$first_line"
tail -n +2 tokens.txt > tokens.next
mv tokens.next tokens.txt
"#,
            )?;
            let mut permissions = std::fs::metadata(&script_path)?.permissions();
            {
                use std::os::unix::fs::PermissionsExt;
                permissions.set_mode(0o755);
            }
            std::fs::set_permissions(&script_path, permissions)?;
            ("./print-token.sh".to_string(), Vec::new())
        };

        #[cfg(windows)]
        let (command, args) = {
            let script_path = tempdir.path().join("print-token.cmd");
            std::fs::write(
                &script_path,
                r#"@echo off
setlocal EnableExtensions DisableDelayedExpansion
set "first_line="
<tokens.txt set /p "first_line="
if not defined first_line exit /b 1
setlocal EnableDelayedExpansion
echo(!first_line!
endlocal
more +1 tokens.txt > tokens.next
move /y tokens.next tokens.txt >nul
"#,
            )?;
            (
                "cmd.exe".to_string(),
                vec![
                    "/d".to_string(),
                    "/s".to_string(),
                    "/c".to_string(),
                    ".\\print-token.cmd".to_string(),
                ],
            )
        };

        Ok(Self {
            tempdir,
            command,
            args,
        })
    }

    fn new_failing() -> std::io::Result<Self> {
        let tempdir = tempfile::tempdir()?;

        #[cfg(unix)]
        let (command, args) = {
            let script_path = tempdir.path().join("fail.sh");
            std::fs::write(
                &script_path,
                r#"#!/bin/sh
exit 1
"#,
            )?;
            let mut permissions = std::fs::metadata(&script_path)?.permissions();
            {
                use std::os::unix::fs::PermissionsExt;
                permissions.set_mode(0o755);
            }
            std::fs::set_permissions(&script_path, permissions)?;
            ("./fail.sh".to_string(), Vec::new())
        };

        #[cfg(windows)]
        let (command, args) = (
            "cmd.exe".to_string(),
            vec![
                "/d".to_string(),
                "/s".to_string(),
                "/c".to_string(),
                "exit /b 1".to_string(),
            ],
        );

        Ok(Self {
            tempdir,
            command,
            args,
        })
    }

    fn auth_config(&self) -> ModelProviderAuthInfo {
        serde_json::from_value(json!({
            "command": self.command,
            "args": self.args,
            // Process startup can be slow on loaded Windows CI workers, so leave enough slack to
            // avoid turning these auth-cache assertions into a process-launch timing test.
            "timeout_ms": 10_000,
            "refresh_interval_ms": 60000,
            "cwd": self.tempdir.path(),
        }))
        .expect("provider auth config should deserialize")
    }
}

struct AuthFileParams {
    openai_api_key: Option<String>,
    chatgpt_plan_type: Option<String>,
    chatgpt_account_id: Option<String>,
}

fn write_auth_file(params: AuthFileParams, codex_home: &Path) -> std::io::Result<String> {
    let fake_jwt = fake_jwt_for_auth_file_params(&params)?;
    let pool_account_id = params.chatgpt_account_id.clone();
    let auth_file = get_auth_file(codex_home);
    let auth_json_data = json!({
        "OPENAI_API_KEY": params.openai_api_key,
        "tokens": {
            "id_token": fake_jwt,
            "access_token": "test-access-token",
            "refresh_token": "test-refresh-token",
            "account_id": params.chatgpt_account_id,
        },
        "pool_account_id": pool_account_id,
        "last_refresh": Utc::now(),
    });
    let auth_json = serde_json::to_string_pretty(&auth_json_data)?;
    std::fs::write(auth_file, auth_json)?;
    Ok(fake_jwt)
}

fn fake_jwt_for_auth_file_params(params: &AuthFileParams) -> std::io::Result<String> {
    #[derive(Serialize)]
    struct Header {
        alg: &'static str,
        typ: &'static str,
    }

    let header = Header {
        alg: "none",
        typ: "JWT",
    };
    let mut auth_payload = serde_json::json!({
        "chatgpt_user_id": "user-12345",
        "user_id": "user-12345",
    });

    if let Some(chatgpt_plan_type) = params.chatgpt_plan_type.as_ref() {
        auth_payload["chatgpt_plan_type"] = serde_json::Value::String(chatgpt_plan_type.clone());
    }

    if let Some(chatgpt_account_id) = params.chatgpt_account_id.as_ref() {
        auth_payload["chatgpt_account_id"] = serde_json::Value::String(chatgpt_account_id.clone());
    }

    let payload = serde_json::json!({
        "email": "user@example.com",
        "email_verified": true,
        "https://api.openai.com/auth": auth_payload,
    });
    let b64 = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
    let header_b64 = b64(&serde_json::to_vec(&header)?);
    let payload_b64 = b64(&serde_json::to_vec(&payload)?);
    let signature_b64 = b64(b"sig");
    Ok(format!("{header_b64}.{payload_b64}.{signature_b64}"))
}

fn fake_access_token(account_id: &str, exp: i64) -> String {
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
        "exp": exp,
        "https://api.openai.com/auth": {
            "chatgpt_account_id": account_id,
        },
    });
    let b64 = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
    let header_b64 = b64(&serde_json::to_vec(&header).expect("header should serialize"));
    let payload_b64 = b64(&serde_json::to_vec(&payload).expect("payload should serialize"));
    let signature_b64 = b64(b"sig");
    format!("{header_b64}.{payload_b64}.{signature_b64}")
}

async fn build_config(
    codex_home: &Path,
    forced_login_method: Option<ForcedLoginMethod>,
    forced_chatgpt_workspace_id: Option<Vec<String>>,
) -> AuthConfig {
    AuthConfig {
        codex_home: codex_home.to_path_buf(),
        auth_credentials_store_mode: AuthCredentialsStoreMode::File,
        forced_login_method,
        forced_chatgpt_workspace_id,
        chatgpt_base_url: None,
    }
}

/// Use sparingly.
/// TODO (gpeal): replace this with an injectable env var provider.
#[cfg(test)]
struct EnvVarGuard {
    key: &'static str,
    original: Option<std::ffi::OsString>,
}

#[cfg(test)]
impl EnvVarGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let original = env::var_os(key);
        unsafe {
            env::set_var(key, value);
        }
        Self { key, original }
    }

    fn remove(key: &'static str) -> Self {
        let original = env::var_os(key);
        unsafe {
            env::remove_var(key);
        }
        Self { key, original }
    }
}

#[cfg(test)]
impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        unsafe {
            match &self.original {
                Some(value) => env::set_var(self.key, value),
                None => env::remove_var(self.key),
            }
        }
    }
}

fn remove_access_token_env_var() -> EnvVarGuard {
    EnvVarGuard::remove(CODEX_ACCESS_TOKEN_ENV_VAR)
}

#[tokio::test]
#[serial(codex_auth_env)]
async fn load_auth_reads_access_token_from_env() {
    let codex_home = tempdir().unwrap();
    let expected_record = agent_identity_record(WORKSPACE_ID_ALLOWED);
    let agent_identity =
        signed_agent_identity_jwt(&expected_record, json!(expected_record.plan_type))
            .expect("signed agent identity");
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/backend-api/wham/agent-identities/jwks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(test_jwks_body()))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/backend-api/v1/agent/agent-runtime-id/task/register"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "task_id": "task-123",
        })))
        .expect(1)
        .mount(&server)
        .await;
    let _access_token_guard = EnvVarGuard::set(CODEX_ACCESS_TOKEN_ENV_VAR, &agent_identity);

    let chatgpt_base_url = format!("{}/backend-api", server.uri());
    let _authapi_guard =
        EnvVarGuard::set("CODEX_AGENT_IDENTITY_AUTHAPI_BASE_URL", &chatgpt_base_url);
    let auth = super::load_auth(
        codex_home.path(),
        /*enable_codex_api_key_env*/ false,
        AuthCredentialsStoreMode::File,
        Some(&chatgpt_base_url),
    )
    .await
    .expect("env auth should load")
    .expect("env auth should be present");

    let CodexAuth::AgentIdentity(agent_identity) = auth else {
        panic!("env auth should load as agent identity");
    };
    assert_eq!(agent_identity.record(), &expected_record);
    assert_eq!(agent_identity.process_task_id(), "task-123");
    assert!(
        !get_auth_file(codex_home.path()).exists(),
        "env auth should not write auth.json"
    );
    server.verify().await;
}

#[tokio::test]
#[serial(codex_auth_env)]
async fn load_auth_keeps_codex_api_key_env_precedence() {
    let codex_home = tempdir().unwrap();
    let record = agent_identity_record(WORKSPACE_ID_ALLOWED);
    let agent_identity = fake_agent_identity_jwt(&record).expect("fake agent identity");
    let _access_token_guard = EnvVarGuard::set(CODEX_ACCESS_TOKEN_ENV_VAR, &agent_identity);
    let _api_key_guard = EnvVarGuard::set(CODEX_API_KEY_ENV_VAR, "sk-env");

    let auth = super::load_auth(
        codex_home.path(),
        /*enable_codex_api_key_env*/ true,
        AuthCredentialsStoreMode::File,
        /*chatgpt_base_url*/ None,
    )
    .await
    .expect("env auth should load")
    .expect("env auth should be present");

    assert_eq!(auth.api_key(), Some("sk-env"));
}

#[tokio::test]
#[serial(codex_auth_env)]
async fn enforce_login_restrictions_logs_out_for_method_mismatch() {
    let codex_home = tempdir().unwrap();
    let _access_token_guard = remove_access_token_env_var();
    login_with_api_key(codex_home.path(), "sk-test", AuthCredentialsStoreMode::File)
        .expect("seed api key");

    let config = build_config(
        codex_home.path(),
        Some(ForcedLoginMethod::Chatgpt),
        /*forced_chatgpt_workspace_id*/ None,
    )
    .await;

    let err = super::enforce_login_restrictions(&config)
        .await
        .expect_err("expected method mismatch to error");
    assert!(err.to_string().contains("ChatGPT login is required"));
    assert!(
        !codex_home.path().join("auth.json").exists(),
        "auth.json should be removed on mismatch"
    );
}

#[tokio::test]
#[serial(codex_auth_env)]
async fn enforce_login_restrictions_logs_out_for_workspace_mismatch() {
    let codex_home = tempdir().unwrap();
    let _access_token_guard = remove_access_token_env_var();
    let _jwt = write_auth_file(
        AuthFileParams {
            openai_api_key: None,
            chatgpt_plan_type: Some("pro".to_string()),
            chatgpt_account_id: Some(WORKSPACE_ID_DISALLOWED.to_string()),
        },
        codex_home.path(),
    )
    .expect("failed to write auth file");

    let config = build_config(
        codex_home.path(),
        /*forced_login_method*/ None,
        Some(vec![WORKSPACE_ID_ALLOWED.to_string()]),
    )
    .await;

    let err = super::enforce_login_restrictions(&config)
        .await
        .expect_err("expected workspace mismatch to error");
    assert!(
        err.to_string()
            .contains(&format!("workspace(s) {WORKSPACE_ID_ALLOWED}"))
    );
    assert!(
        !codex_home.path().join("auth.json").exists(),
        "auth.json should be removed on mismatch"
    );
}

#[tokio::test]
#[serial(codex_auth_env)]
async fn enforce_login_restrictions_allows_matching_workspace() {
    let codex_home = tempdir().unwrap();
    let _access_token_guard = remove_access_token_env_var();
    let _jwt = write_auth_file(
        AuthFileParams {
            openai_api_key: None,
            chatgpt_plan_type: Some("pro".to_string()),
            chatgpt_account_id: Some(WORKSPACE_ID_ALLOWED.to_string()),
        },
        codex_home.path(),
    )
    .expect("failed to write auth file");

    let config = build_config(
        codex_home.path(),
        /*forced_login_method*/ None,
        Some(vec![WORKSPACE_ID_ALLOWED.to_string()]),
    )
    .await;

    super::enforce_login_restrictions(&config)
        .await
        .expect("matching workspace should succeed");
    assert!(
        codex_home.path().join("auth.json").exists(),
        "auth.json should remain when restrictions pass"
    );
}

#[tokio::test]
#[serial(codex_auth_env)]
async fn enforce_login_restrictions_allows_any_matching_workspace_in_list() {
    let codex_home = tempdir().unwrap();
    let _jwt = write_auth_file(
        AuthFileParams {
            openai_api_key: None,
            chatgpt_plan_type: Some("pro".to_string()),
            chatgpt_account_id: Some(WORKSPACE_ID_ALLOWED.to_string()),
        },
        codex_home.path(),
    )
    .expect("failed to write auth file");

    let config = build_config(
        codex_home.path(),
        /*forced_login_method*/ None,
        Some(vec![
            WORKSPACE_ID_SECOND_ALLOWED.to_string(),
            WORKSPACE_ID_ALLOWED.to_string(),
        ]),
    )
    .await;

    super::enforce_login_restrictions(&config)
        .await
        .expect("any matching workspace in the allowed list should succeed");
}

#[tokio::test]
#[serial(codex_auth_env)]
async fn enforce_login_restrictions_logs_out_for_agent_identity_workspace_mismatch() {
    let codex_home = tempdir().unwrap();
    let _access_token_guard = remove_access_token_env_var();
    let record = agent_identity_record(WORKSPACE_ID_DISALLOWED);
    let agent_identity =
        signed_agent_identity_jwt(&record, json!(record.plan_type)).expect("signed agent identity");
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/backend-api/wham/agent-identities/jwks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(test_jwks_body()))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/backend-api/v1/agent/agent-runtime-id/task/register"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "task_id": "task-123",
        })))
        .expect(1)
        .mount(&server)
        .await;
    let chatgpt_base_url = format!("{}/backend-api", server.uri());
    let _authapi_guard =
        EnvVarGuard::set("CODEX_AGENT_IDENTITY_AUTHAPI_BASE_URL", &chatgpt_base_url);
    save_auth(
        codex_home.path(),
        &AuthDotJson {
            auth_mode: Some(ApiAuthMode::AgentIdentity),
            openai_api_key: None,
            tokens: None,
            pool_account_id: None,
            last_refresh: None,
            agent_identity: Some(agent_identity),
        },
        AuthCredentialsStoreMode::File,
    )
    .expect("seed agent identity auth");

    let config = AuthConfig {
        codex_home: codex_home.path().to_path_buf(),
        auth_credentials_store_mode: AuthCredentialsStoreMode::File,
        forced_login_method: None,
        forced_chatgpt_workspace_id: Some(vec![WORKSPACE_ID_ALLOWED.to_string()]),
        chatgpt_base_url: Some(chatgpt_base_url),
    };

    let err = super::enforce_login_restrictions(&config)
        .await
        .expect_err("expected workspace mismatch to error");
    assert!(err.to_string().contains(&format!(
        "current credentials belong to {WORKSPACE_ID_DISALLOWED}"
    )));
    assert!(
        !codex_home.path().join("auth.json").exists(),
        "auth.json should be removed on mismatch"
    );
    server.verify().await;
}

#[tokio::test]
async fn enforce_login_restrictions_allows_api_key_if_login_method_not_set_but_forced_chatgpt_workspace_id_is_set()
 {
    let codex_home = tempdir().unwrap();
    let _access_token_guard = remove_access_token_env_var();
    login_with_api_key(codex_home.path(), "sk-test", AuthCredentialsStoreMode::File)
        .expect("seed api key");

    let config = build_config(
        codex_home.path(),
        /*forced_login_method*/ None,
        Some(vec![WORKSPACE_ID_ALLOWED.to_string()]),
    )
    .await;

    super::enforce_login_restrictions(&config)
        .await
        .expect("matching workspace should succeed");
    assert!(
        codex_home.path().join("auth.json").exists(),
        "auth.json should remain when restrictions pass"
    );
}

#[tokio::test]
#[serial(codex_auth_env)]
async fn enforce_login_restrictions_blocks_env_api_key_when_chatgpt_required() {
    let _guard = EnvVarGuard::set(CODEX_API_KEY_ENV_VAR, "sk-env");
    let _access_token_guard = remove_access_token_env_var();
    let codex_home = tempdir().unwrap();

    let config = build_config(
        codex_home.path(),
        Some(ForcedLoginMethod::Chatgpt),
        /*forced_chatgpt_workspace_id*/ None,
    )
    .await;

    let err = super::enforce_login_restrictions(&config)
        .await
        .expect_err("environment API key should not satisfy forced ChatGPT login");
    assert!(
        err.to_string()
            .contains("ChatGPT login is required, but an API key is currently being used.")
    );
}

fn agent_identity_record(account_id: &str) -> AgentIdentityAuthRecord {
    let key_material =
        codex_agent_identity::generate_agent_key_material().expect("generate agent key material");
    AgentIdentityAuthRecord {
        agent_runtime_id: "agent-runtime-id".to_string(),
        agent_private_key: key_material.private_key_pkcs8_base64,
        account_id: account_id.to_string(),
        chatgpt_user_id: "user-id".to_string(),
        email: "user@example.com".to_string(),
        plan_type: AccountPlanType::Pro,
        chatgpt_account_is_fedramp: false,
    }
}

fn fake_agent_identity_jwt(record: &AgentIdentityAuthRecord) -> std::io::Result<String> {
    fake_agent_identity_jwt_with_plan_type(record, serde_json::to_value(record.plan_type)?)
}

fn fake_agent_identity_jwt_with_plan_type(
    record: &AgentIdentityAuthRecord,
    plan_type: serde_json::Value,
) -> std::io::Result<String> {
    let encode = |bytes: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let header_b64 = encode(br#"{"alg":"EdDSA","typ":"JWT"}"#);
    let payload = json!({
        "iss": "https://chatgpt.com/codex-backend/agent-identity",
        "aud": "codex-app-server",
        "iat": 1_700_000_000usize,
        "exp": 4_000_000_000usize,
        "agent_runtime_id": record.agent_runtime_id,
        "agent_private_key": record.agent_private_key,
        "account_id": record.account_id,
        "chatgpt_user_id": record.chatgpt_user_id,
        "email": record.email,
        "plan_type": plan_type,
        "chatgpt_account_is_fedramp": record.chatgpt_account_is_fedramp,
    });
    let payload_b64 = encode(&serde_json::to_vec(&payload)?);
    let signature_b64 = encode(b"sig");
    Ok(format!("{header_b64}.{payload_b64}.{signature_b64}"))
}

fn signed_agent_identity_jwt(
    record: &AgentIdentityAuthRecord,
    plan_type: serde_json::Value,
) -> jsonwebtoken::errors::Result<String> {
    let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
    header.kid = Some("test-key".to_string());
    jsonwebtoken::encode(
        &header,
        &json!({
            "iss": "https://chatgpt.com/codex-backend/agent-identity",
            "aud": "codex-app-server",
            "iat": 1_700_000_000usize,
            "exp": 4_000_000_000usize,
            "agent_runtime_id": record.agent_runtime_id,
            "agent_private_key": record.agent_private_key,
            "account_id": record.account_id,
            "chatgpt_user_id": record.chatgpt_user_id,
            "email": record.email,
            "plan_type": plan_type,
            "chatgpt_account_is_fedramp": record.chatgpt_account_is_fedramp,
        }),
        &jsonwebtoken::EncodingKey::from_rsa_pem(TEST_AGENT_IDENTITY_RSA_PRIVATE_KEY_PEM)?,
    )
}

fn test_jwks_body() -> serde_json::Value {
    json!({
        "keys": [{
            "kty": "RSA",
            "kid": "test-key",
            "use": "sig",
            "alg": "RS256",
            "n": "1qQF2MqTrGAMDm7wXbjJP5sWqGA83tAGUs2ksy7iJXLJdhCg4AtwGm4SFl4f6kxhCSzlN1QdXuZjvRT2wZZiGUi9xUE28rf4WLrTxSnwqLuTy5knMP08yC0t_0YU_FGPZMcWb14hG05IvZr8UbmRaVagxSR8H4rSIymRoVwwmFSrqz068XrWGSYNIfLEASyo5GdAaqmk1JALINHgYGQJVxMxtwcvDxoVKmC7eltUNymMNBZhsv4E8sx9YNLpBoEibznfEpDU_DGzrM5eZCsQzaqbhBOlGd427ifud_Nnd9cPqzgCUc23-0FXSPfpbgksCXAwAmD0OFjQWrgqVdKL6Q",
            "e": "AQAB",
        }]
    })
}

const TEST_AGENT_IDENTITY_RSA_PRIVATE_KEY_PEM: &[u8] = br#"-----BEGIN PRIVATE KEY-----
MIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQDWpAXYypOsYAwO
bvBduMk/mxaoYDze0AZSzaSzLuIlcsl2EKDgC3AabhIWXh/qTGEJLOU3VB1e5mO9
FPbBlmIZSL3FQTbyt/hYutPFKfCou5PLmScw/TzILS3/RhT8UY9kxxZvXiEbTki9
mvxRuZFpVqDFJHwfitIjKZGhXDCYVKurPTrxetYZJg0h8sQBLKjkZ0BqqaTUkAsg
0eBgZAlXEzG3By8PGhUqYLt6W1Q3KYw0FmGy/gTyzH1g0ukGgSJvOd8SkNT8MbOs
zl5kKxDNqpuEE6UZ3jbuJ+5382d31w+rOAJRzbf7QVdI9+luCSwJcDACYPQ4WNBa
uCpV0ovpAgMBAAECggEAVu84LwZdqYN9XpswX8VoPYrjMm9IODapWQBRpQFoNyK2
1ksF3bjEPvA2Azk8U/l7k+vLKw22l6lY3EyRZPcz5GnB8xLm3ogE3mtNOp4yCyVu
RxhQ91aaN7mU17/a4BdorLi2LYVCg3zBmYociD1Q2AluNGsCmwPu+K7tfR2J0Sg8
NjqiTbDG1XDpR/icwgC9t6vh8lZpCHDhF4tbQfLLVLeA/OdcuzXDyMCXbmdVIdBQ
rm4aIFmr2e1/2ctTbCg85S6AGFTH+pSLjrwTzyvf+F6NW5uNjLQAQLFj+EznBDxj
Xdx90cySrjsKK6PVWQF4RiTvkSW8eWL7R6B2FZbGwQKBgQDuVQRj72hWloR7mbEL
aUEEv3pIXTMXWEsoMBNczos/1L1RnAN1AI44TurznasPZAWvQj+kVbLDR+TAeZrL
iA8HIWswQUI18hFmgKzSkwIXGtubcKVrgsKeS4lMDKCM/Ef6WAYdeq6ronoY5lCN
YrJFmGp81W5zcV7lyiycgbSiGwKBgQDmjWYf6pZjrK7Z+OJ3X1AZfi2vss15SCvL
3fPgzIDbViztpGyQhc3DQZIsBNIu0xZp/veGce9TEeTds2ro9NfdJFeou8+fC7Pq
sOsM3amGFFi+ZW/9BWyjZEM88bgWWAjqLHbpfHDxjAf5CSxddqxgHlbP0Ytyb1Vg
gmPDn9YKSwKBgQDbTi3hC35WFuDHn0/zcSHcDZmnFuOZeqyFyV83yfMGhGrEuqvP
sPgtRikajJ3IZsB4WZyYSidZXEFY/0z6NjOl2xF38MTNQPbT/FmK1q1Yt2UWrlv5
BvSwlk87RG9D7C0LZo4R+D7cPoDdgqjiwMvMEIkEX5zn641oI1ZTmWKuuwKBgQCD
KF+3unnRvHRAVoFnTZbA2fJdqMeRvogD04GhGlYX8V9f1hFY6nXTJaNlXVzA/J8c
r8ra9kgjJuPfZ+ljG58OFFW2DRohLcQtuHYPfK6rMzoFHqnl9EcIcMp7ijuionR3
29HOJFgQYgxLFXfit9d6WugiE+BTupiEbckZif13HwKBgE/lAlkVHP6YahOO2Ljc
J1bwkqKZTB5dHolX9A58e/xXnfZ5P8f3Z83+Izap3FwqQulk7b1WO1MQcHuVg2NN
5da0D4h2rYOXnbYIg0BVu4spQbaM6ewsp66b8+MzLOBvj8SzWdt1Oyw0q/MRyQAR
8U4M2TSWCKUY/A6sT4W8+mT9
-----END PRIVATE KEY-----"#;

#[tokio::test]
#[serial(codex_auth_env)]
async fn agent_identity_plan_type_maps_raw_enterprise_alias() {
    assert_agent_identity_plan_alias(json!("hc"), AccountPlanType::Enterprise).await;
}

#[tokio::test]
#[serial(codex_auth_env)]
async fn agent_identity_plan_type_maps_raw_education_alias() {
    assert_agent_identity_plan_alias(json!("education"), AccountPlanType::Edu).await;
}

async fn assert_agent_identity_plan_alias(
    plan_type: serde_json::Value,
    expected_plan_type: AccountPlanType,
) {
    let record = agent_identity_record("account-id");
    let jwt = signed_agent_identity_jwt(&record, plan_type).expect("agent identity jwt");
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/backend-api/wham/agent-identities/jwks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(test_jwks_body()))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/backend-api/v1/agent/agent-runtime-id/task/register"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "task_id": "task-123",
        })))
        .expect(1)
        .mount(&server)
        .await;
    let chatgpt_base_url = format!("{}/backend-api", server.uri());
    let _authapi_guard =
        EnvVarGuard::set("CODEX_AGENT_IDENTITY_AUTHAPI_BASE_URL", &chatgpt_base_url);
    let auth = CodexAuth::from_agent_identity_jwt(&jwt, Some(&chatgpt_base_url))
        .await
        .expect("agent identity auth");

    pretty_assertions::assert_eq!(auth.account_plan_type(), Some(expected_plan_type));
    server.verify().await;
}

#[tokio::test]
#[serial(codex_auth_env)]
async fn plan_type_maps_known_plan() {
    let codex_home = tempdir().unwrap();
    let _access_token_guard = remove_access_token_env_var();
    let _jwt = write_auth_file(
        AuthFileParams {
            openai_api_key: None,
            chatgpt_plan_type: Some("pro".to_string()),
            chatgpt_account_id: None,
        },
        codex_home.path(),
    )
    .expect("failed to write auth file");

    let auth = super::load_auth(
        codex_home.path(),
        /*enable_codex_api_key_env*/ false,
        AuthCredentialsStoreMode::File,
        /*chatgpt_base_url*/ None,
    )
    .await
    .expect("load auth")
    .expect("auth available");

    pretty_assertions::assert_eq!(auth.account_plan_type(), Some(AccountPlanType::Pro));
}

#[tokio::test]
#[serial(codex_auth_env)]
async fn plan_type_maps_self_serve_business_usage_based_plan() {
    let codex_home = tempdir().unwrap();
    let _access_token_guard = remove_access_token_env_var();
    let _jwt = write_auth_file(
        AuthFileParams {
            openai_api_key: None,
            chatgpt_plan_type: Some("self_serve_business_usage_based".to_string()),
            chatgpt_account_id: None,
        },
        codex_home.path(),
    )
    .expect("failed to write auth file");

    let auth = super::load_auth(
        codex_home.path(),
        /*enable_codex_api_key_env*/ false,
        AuthCredentialsStoreMode::File,
        /*chatgpt_base_url*/ None,
    )
    .await
    .expect("load auth")
    .expect("auth available");

    pretty_assertions::assert_eq!(
        auth.account_plan_type(),
        Some(AccountPlanType::SelfServeBusinessUsageBased)
    );
}

#[tokio::test]
#[serial(codex_auth_env)]
async fn plan_type_maps_enterprise_cbp_usage_based_plan() {
    let codex_home = tempdir().unwrap();
    let _access_token_guard = remove_access_token_env_var();
    let _jwt = write_auth_file(
        AuthFileParams {
            openai_api_key: None,
            chatgpt_plan_type: Some("enterprise_cbp_usage_based".to_string()),
            chatgpt_account_id: None,
        },
        codex_home.path(),
    )
    .expect("failed to write auth file");

    let auth = super::load_auth(
        codex_home.path(),
        /*enable_codex_api_key_env*/ false,
        AuthCredentialsStoreMode::File,
        /*chatgpt_base_url*/ None,
    )
    .await
    .expect("load auth")
    .expect("auth available");

    pretty_assertions::assert_eq!(
        auth.account_plan_type(),
        Some(AccountPlanType::EnterpriseCbpUsageBased)
    );
}

#[tokio::test]
#[serial(codex_auth_env)]
async fn plan_type_maps_unknown_to_unknown() {
    let codex_home = tempdir().unwrap();
    let _access_token_guard = remove_access_token_env_var();
    let _jwt = write_auth_file(
        AuthFileParams {
            openai_api_key: None,
            chatgpt_plan_type: Some("mystery-tier".to_string()),
            chatgpt_account_id: None,
        },
        codex_home.path(),
    )
    .expect("failed to write auth file");

    let auth = super::load_auth(
        codex_home.path(),
        /*enable_codex_api_key_env*/ false,
        AuthCredentialsStoreMode::File,
        /*chatgpt_base_url*/ None,
    )
    .await
    .expect("load auth")
    .expect("auth available");

    pretty_assertions::assert_eq!(auth.account_plan_type(), Some(AccountPlanType::Unknown));
}

#[tokio::test]
#[serial(codex_auth_env)]
async fn missing_plan_type_maps_to_unknown() {
    let codex_home = tempdir().unwrap();
    let _access_token_guard = remove_access_token_env_var();
    let _jwt = write_auth_file(
        AuthFileParams {
            openai_api_key: None,
            chatgpt_plan_type: None,
            chatgpt_account_id: None,
        },
        codex_home.path(),
    )
    .expect("failed to write auth file");

    let auth = super::load_auth(
        codex_home.path(),
        /*enable_codex_api_key_env*/ false,
        AuthCredentialsStoreMode::File,
        /*chatgpt_base_url*/ None,
    )
    .await
    .expect("load auth")
    .expect("auth available");

    pretty_assertions::assert_eq!(auth.account_plan_type(), Some(AccountPlanType::Unknown));
}
