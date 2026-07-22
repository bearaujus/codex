use anyhow::Result;
use app_test_support::ChatGptAuthFixture;
use app_test_support::TestAppServer;
use app_test_support::mark_pool_auth_failed;
use app_test_support::to_response;
use app_test_support::write_chatgpt_auth;
use chrono::Duration;
use chrono::Utc;
use codex_app_server_protocol::AuthMode;
use codex_app_server_protocol::GetAuthStatusParams;
use codex_app_server_protocol::GetAuthStatusResponse;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::RequestId;
use codex_config::types::AuthCredentialsStoreMode;
use pretty_assertions::assert_eq;
use std::path::Path;
use tempfile::TempDir;
use tokio::time::timeout;

// Bazel CI can spend tens of seconds starting app-server subprocesses or
// processing auth RPCs under load.
const DEFAULT_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

fn create_config_toml(codex_home: &Path) -> std::io::Result<()> {
    let config_toml = codex_home.join("config.toml");
    std::fs::write(
        config_toml,
        r#"
model = "mock-model"
approval_policy = "never"
sandbox_mode = "danger-full-access"

[features]
shell_snapshot = false
"#,
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_auth_status_no_auth() -> Result<()> {
    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path())?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .with_env_overrides(&[("OPENAI_API_KEY", None)])
        .build()
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let request_id = mcp
        .send_get_auth_status_request(GetAuthStatusParams {
            include_token: Some(true),
            refresh_token: Some(false),
        })
        .await?;

    let resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(request_id)),
    )
    .await??;
    let status: GetAuthStatusResponse = to_response(resp)?;
    assert_eq!(status.auth_method, None, "expected no auth method");
    assert_eq!(status.auth_token, None, "expected no token");
    Ok(())
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_auth_status_omits_token_after_permanent_refresh_failure() -> Result<()> {
    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path())?;
    write_chatgpt_auth(
        codex_home.path(),
        ChatGptAuthFixture::new("stale-access-token")
            .refresh_token("stale-refresh-token")
            .account_id("acct_123")
            .email("user@example.com")
            .plan_type("pro"),
        AuthCredentialsStoreMode::File,
    )?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .with_env_overrides(&[("OPENAI_API_KEY", None)])
        .build()
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;
    mark_pool_auth_failed(codex_home.path(), "acct_123").await?;

    let request_id = mcp
        .send_get_auth_status_request(GetAuthStatusParams {
            include_token: Some(true),
            refresh_token: Some(true),
        })
        .await?;

    let resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(request_id)),
    )
    .await??;
    let status: GetAuthStatusResponse = to_response(resp)?;
    assert_eq!(
        status,
        GetAuthStatusResponse {
            auth_method: Some(AuthMode::Chatgpt),
            auth_token: None,
            requires_openai_auth: Some(true),
        }
    );

    let second_request_id = mcp
        .send_get_auth_status_request(GetAuthStatusParams {
            include_token: Some(true),
            refresh_token: Some(true),
        })
        .await?;

    let second_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(second_request_id)),
    )
    .await??;
    let second_status: GetAuthStatusResponse = to_response(second_resp)?;
    assert_eq!(second_status, status);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_auth_status_omits_token_after_proactive_refresh_failure() -> Result<()> {
    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path())?;
    write_chatgpt_auth(
        codex_home.path(),
        ChatGptAuthFixture::new("stale-access-token")
            .refresh_token("stale-refresh-token")
            .account_id("acct_123")
            .email("user@example.com")
            .plan_type("pro")
            .last_refresh(Some(Utc::now() - Duration::days(9))),
        AuthCredentialsStoreMode::File,
    )?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .with_env_overrides(&[("OPENAI_API_KEY", None)])
        .build()
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;
    mark_pool_auth_failed(codex_home.path(), "acct_123").await?;

    let request_id = mcp
        .send_get_auth_status_request(GetAuthStatusParams {
            include_token: Some(true),
            // Pool-managed ChatGPT auth has no standalone OAuth refresh path.
            refresh_token: Some(true),
        })
        .await?;

    let resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(request_id)),
    )
    .await??;
    let status: GetAuthStatusResponse = to_response(resp)?;
    assert_eq!(
        status,
        GetAuthStatusResponse {
            auth_method: Some(AuthMode::Chatgpt),
            auth_token: None,
            requires_openai_auth: Some(true),
        }
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_auth_status_returns_token_after_proactive_refresh_recovery() -> Result<()> {
    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path())?;
    write_chatgpt_auth(
        codex_home.path(),
        ChatGptAuthFixture::new("stale-access-token")
            .refresh_token("stale-refresh-token")
            .account_id("acct_123")
            .email("user@example.com")
            .plan_type("pro")
            .last_refresh(Some(Utc::now() - Duration::days(9))),
        AuthCredentialsStoreMode::File,
    )?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .with_env_overrides(&[("OPENAI_API_KEY", None)])
        .build()
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;
    mark_pool_auth_failed(codex_home.path(), "acct_123").await?;

    let failed_request_id = mcp
        .send_get_auth_status_request(GetAuthStatusParams {
            include_token: Some(true),
            refresh_token: Some(true),
        })
        .await?;

    let failed_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(failed_request_id)),
    )
    .await??;
    let failed_status: GetAuthStatusResponse = to_response(failed_resp)?;
    assert_eq!(
        failed_status,
        GetAuthStatusResponse {
            auth_method: Some(AuthMode::Chatgpt),
            auth_token: None,
            requires_openai_auth: Some(true),
        }
    );

    write_chatgpt_auth(
        codex_home.path(),
        ChatGptAuthFixture::new("recovered-access-token")
            .refresh_token("recovered-refresh-token")
            .account_id("acct_123")
            .email("user@example.com")
            .plan_type("pro")
            .last_refresh(Some(Utc::now())),
        AuthCredentialsStoreMode::File,
    )?;
    // Re-open the pool and persist the recovered secret so the next auth() reload
    // picks up a valid pool copy (codex-accounts is the normal writer of this path).
    {
        use codex_login::ChatgptAccountPool;
        use codex_login::load_auth_dot_json;
        use codex_login::token_data::derive_pool_account_id;
        let pool = ChatgptAccountPool::open(
            codex_home.path().to_path_buf(),
            AuthCredentialsStoreMode::File,
            /*chatgpt_base_url*/ None,
        )
        .await?;
        let pool_account_id = derive_pool_account_id("acct_123", /*member_identity_key*/ None);
        let recovered = load_auth_dot_json(
            codex_home.path(),
            AuthCredentialsStoreMode::File,
            codex_login::AuthKeyringBackendKind::default(),
        )?
        .expect("recovered auth should exist");
        pool.persist_refreshed_account_auth(&pool_account_id, &recovered)
            .await?;
    }

    let recovered_request_id = mcp
        .send_get_auth_status_request(GetAuthStatusParams {
            include_token: Some(true),
            refresh_token: Some(false),
        })
        .await?;

    let recovered_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(recovered_request_id)),
    )
    .await??;
    let recovered_status: GetAuthStatusResponse = to_response(recovered_resp)?;
    assert_eq!(
        recovered_status,
        GetAuthStatusResponse {
            auth_method: Some(AuthMode::Chatgpt),
            auth_token: Some("recovered-access-token".to_string()),
            requires_openai_auth: Some(true),
        }
    );

    Ok(())
}
