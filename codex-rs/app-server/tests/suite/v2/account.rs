use anyhow::Result;
use anyhow::bail;
use app_test_support::TestAppServer;
use app_test_support::to_response;

use app_test_support::ChatGptAuthFixture;
use app_test_support::ChatGptIdTokenClaims;
use app_test_support::encode_id_token;
use app_test_support::write_chatgpt_auth;
use app_test_support::write_models_cache;
use chrono::Duration as ChronoDuration;
use chrono::Utc;
use codex_app_server_protocol::Account;
use codex_app_server_protocol::AccountUpdatedNotification;
use codex_app_server_protocol::AuthMode;
use codex_app_server_protocol::CancelLoginAccountParams;
use codex_app_server_protocol::CancelLoginAccountResponse;
use codex_app_server_protocol::CancelLoginAccountStatus;
use codex_app_server_protocol::GetAccountParams;
use codex_app_server_protocol::GetAccountResponse;
use codex_app_server_protocol::GetAuthStatusParams;
use codex_app_server_protocol::GetAuthStatusResponse;
use codex_app_server_protocol::JSONRPCError;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::LoginAccountResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ServerNotification;
use codex_config::types::AuthCredentialsStoreMode;
use codex_login::CLIENT_ID_OVERRIDE_ENV_VAR;
use codex_login::ChatgptAccountPool;
use codex_protocol::account::PlanType as AccountPlanType;
use pretty_assertions::assert_eq;
use serde_json::json;
use serial_test::serial;
use std::path::Path;
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::timeout;
use url::Url;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

const DEFAULT_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
const LOGIN_ISSUER_ENV_VAR: &str = "CODEX_APP_SERVER_LOGIN_ISSUER";
const LOGIN_OPEN_APP_URL_ENV_VAR: &str = "CODEX_APP_SERVER_DEV_OPEN_APP_URL";
const WORKSPACE_ID_ALLOWED: &str = "123e4567-e89b-42d3-a456-426614174000";
const WORKSPACE_ID_SECOND_ALLOWED: &str = "123e4567-e89b-42d3-a456-426614174001";
const WORKSPACE_ID_EMBEDDED: &str = "123e4567-e89b-42d3-a456-426614174010";
const WORKSPACE_ID_DEVICE: &str = "123e4567-e89b-42d3-a456-426614174013";
const WORKSPACE_ID_STALE: &str = "123e4567-e89b-42d3-a456-426614174014";

// Helper to create a minimal config.toml for the app server
#[derive(Default)]
struct CreateConfigTomlParams {
    forced_method: Option<String>,
    forced_workspace_id: Option<String>,
    forced_workspace_ids: Option<Vec<String>>,
    requires_openai_auth: Option<bool>,
    base_url: Option<String>,
    model_provider_id: Option<String>,
    extra_provider_config: Option<String>,
}

fn create_config_toml(codex_home: &Path, params: CreateConfigTomlParams) -> std::io::Result<()> {
    let config_toml = codex_home.join("config.toml");
    let base_url = params
        .base_url
        .unwrap_or_else(|| "http://127.0.0.1:0/v1".to_string());
    let forced_line = if let Some(method) = params.forced_method {
        format!("forced_login_method = \"{method}\"\n")
    } else {
        String::new()
    };
    let forced_workspace_line = if let Some(ws) = params.forced_workspace_id {
        format!("forced_chatgpt_workspace_id = \"{ws}\"\n")
    } else if let Some(workspaces) = params.forced_workspace_ids {
        let workspaces = workspaces
            .into_iter()
            .map(|workspace_id| format!("\"{workspace_id}\""))
            .collect::<Vec<_>>()
            .join(", ");
        format!("forced_chatgpt_workspace_id = [{workspaces}]\n")
    } else {
        String::new()
    };
    let requires_line = match params.requires_openai_auth {
        Some(true) => "requires_openai_auth = true\n".to_string(),
        Some(false) => String::new(),
        None => String::new(),
    };
    let model_provider_id = params
        .model_provider_id
        .unwrap_or_else(|| "mock_provider".to_string());
    let provider_section = if model_provider_id == "mock_provider" {
        format!(
            r#"[model_providers.mock_provider]
name = "Mock provider for test"
base_url = "{base_url}"
wire_api = "responses"
request_max_retries = 0
stream_max_retries = 0
{requires_line}
"#
        )
    } else {
        params.extra_provider_config.unwrap_or_default()
    };
    let contents = format!(
        r#"
model = "mock-model"
approval_policy = "never"
sandbox_mode = "danger-full-access"
{forced_line}
{forced_workspace_line}

model_provider = "{model_provider_id}"

[features]
shell_snapshot = false

{provider_section}
"#
    );
    std::fs::write(config_toml, contents)
}

async fn read_account(mcp: &mut TestAppServer) -> Result<GetAccountResponse> {
    let request_id = mcp
        .send_get_account_request(GetAccountParams {
            refresh_token: false,
        })
        .await?;
    let response = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(request_id)),
    )
    .await??;
    to_response(response)
}

async fn mock_device_code_usercode(server: &MockServer, interval_seconds: u64) {
    Mock::given(method("POST"))
        .and(path("/api/accounts/deviceauth/usercode"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "device_auth_id": "device-auth-123",
            "user_code": "CODE-12345",
            "interval": interval_seconds.to_string(),
        })))
        .mount(server)
        .await;
}

async fn mock_device_code_usercode_failure(server: &MockServer, status: u16) {
    Mock::given(method("POST"))
        .and(path("/api/accounts/deviceauth/usercode"))
        .respond_with(ResponseTemplate::new(status))
        .mount(server)
        .await;
}

async fn mock_device_code_token_success(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/api/accounts/deviceauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "authorization_code": "poll-code-321",
            "code_challenge": "code-challenge-321",
            "code_verifier": "code-verifier-321",
        })))
        .mount(server)
        .await;
}

async fn mock_device_code_token_failure(server: &MockServer, status: u16) {
    Mock::given(method("POST"))
        .and(path("/api/accounts/deviceauth/token"))
        .respond_with(ResponseTemplate::new(status))
        .mount(server)
        .await;
}

async fn mock_oauth_token(server: &MockServer, id_token: &str) {
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id_token": id_token,
            "access_token": "access-token-123",
            "refresh_token": "refresh-token-123",
        })))
        .mount(server)
        .await;
}
#[tokio::test]
async fn login_account_chatgpt_device_code_returns_error_when_disabled() -> Result<()> {
    let codex_home = TempDir::new()?;
    let mock_server = MockServer::start().await;
    create_config_toml(
        codex_home.path(),
        CreateConfigTomlParams {
            requires_openai_auth: Some(true),
            base_url: Some(format!("{}/v1", mock_server.uri())),
            ..Default::default()
        },
    )?;
    write_models_cache(codex_home.path())?;
    mock_device_code_usercode_failure(&mock_server, /*status*/ 404).await;

    let issuer = mock_server.uri();
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .with_env_overrides(&[
            ("OPENAI_API_KEY", None),
            (LOGIN_ISSUER_ENV_VAR, Some(issuer.as_str())),
        ])
        .build()
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let request_id = mcp.send_login_account_chatgpt_device_code_request().await?;
    let err: JSONRPCError = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await??;
    assert!(
        err.error
            .message
            .contains("device code login is not enabled"),
        "unexpected error: {:?}",
        err.error.message
    );

    let maybe_completed = timeout(
        Duration::from_millis(500),
        mcp.read_stream_until_notification_message("account/login/completed"),
    )
    .await;
    assert!(
        maybe_completed.is_err(),
        "account/login/completed should not be emitted when device code start fails"
    );
    assert!(
        !codex_home.path().join("auth.json").exists(),
        "auth.json should not be created when device code start fails"
    );
    Ok(())
}

#[tokio::test]
async fn login_account_chatgpt_device_code_succeeds_and_notifies() -> Result<()> {
    let codex_home = TempDir::new()?;
    let mock_server = MockServer::start().await;
    create_config_toml(
        codex_home.path(),
        CreateConfigTomlParams {
            requires_openai_auth: Some(true),
            base_url: Some(format!("{}/v1", mock_server.uri())),
            ..Default::default()
        },
    )?;
    write_models_cache(codex_home.path())?;

    mock_device_code_usercode(&mock_server, /*interval_seconds*/ 0).await;
    mock_device_code_token_success(&mock_server).await;
    let id_token = encode_id_token(
        &ChatGptIdTokenClaims::new()
            .email("device@example.com")
            .plan_type("pro")
            .chatgpt_account_id(WORKSPACE_ID_DEVICE),
    )?;
    mock_oauth_token(&mock_server, &id_token).await;

    let issuer = mock_server.uri();
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .with_env_overrides(&[
            ("OPENAI_API_KEY", None),
            (LOGIN_ISSUER_ENV_VAR, Some(issuer.as_str())),
        ])
        .build()
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let request_id = mcp.send_login_account_chatgpt_device_code_request().await?;
    let resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(request_id)),
    )
    .await??;
    let login: LoginAccountResponse = to_response(resp)?;
    let LoginAccountResponse::ChatgptDeviceCode {
        login_id,
        verification_url,
        user_code,
    } = login
    else {
        bail!("unexpected login response: {login:?}");
    };
    assert_eq!(verification_url, format!("{issuer}/codex/device"));
    assert_eq!(user_code, "CODE-12345");

    let note = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("account/login/completed"),
    )
    .await??;
    let parsed: ServerNotification = note.try_into()?;
    let ServerNotification::AccountLoginCompleted(payload) = parsed else {
        bail!("unexpected notification: {parsed:?}");
    };
    assert_eq!(payload.login_id, Some(login_id));
    assert_eq!(payload.success, true);
    assert_eq!(payload.error, None);

    let note = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("account/updated"),
    )
    .await??;
    let parsed: ServerNotification = note.try_into()?;
    let ServerNotification::AccountUpdated(payload) = parsed else {
        bail!("unexpected notification: {parsed:?}");
    };
    assert_eq!(
        payload,
        AccountUpdatedNotification {
            auth_mode: Some(AuthMode::Chatgpt),
            plan_type: Some(AccountPlanType::Pro),
            account_email: Some("device@example.com".to_string()),
        }
    );
    assert!(
        codex_home.path().join("auth.json").exists(),
        "auth.json should be created when device code login succeeds"
    );
    let accounts = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        /*chatgpt_base_url*/ None,
    )
    .await?
    .list_accounts()
    .await?;
    assert_eq!(
        accounts
            .iter()
            .map(|account| account.workspace_account_id.as_str())
            .collect::<Vec<_>>(),
        vec![WORKSPACE_ID_DEVICE]
    );
    Ok(())
}

#[tokio::test]
async fn login_account_chatgpt_device_code_failure_notifies_without_account_update() -> Result<()> {
    let codex_home = TempDir::new()?;
    let mock_server = MockServer::start().await;
    create_config_toml(
        codex_home.path(),
        CreateConfigTomlParams {
            requires_openai_auth: Some(true),
            base_url: Some(format!("{}/v1", mock_server.uri())),
            ..Default::default()
        },
    )?;
    write_models_cache(codex_home.path())?;

    mock_device_code_usercode(&mock_server, /*interval_seconds*/ 0).await;
    mock_device_code_token_failure(&mock_server, /*status*/ 500).await;

    let issuer = mock_server.uri();
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .with_env_overrides(&[
            ("OPENAI_API_KEY", None),
            (LOGIN_ISSUER_ENV_VAR, Some(issuer.as_str())),
        ])
        .build()
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let request_id = mcp.send_login_account_chatgpt_device_code_request().await?;
    let resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(request_id)),
    )
    .await??;
    let login: LoginAccountResponse = to_response(resp)?;
    let LoginAccountResponse::ChatgptDeviceCode { login_id, .. } = login else {
        bail!("unexpected login response: {login:?}");
    };

    let note = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("account/login/completed"),
    )
    .await??;
    let parsed: ServerNotification = note.try_into()?;
    let ServerNotification::AccountLoginCompleted(payload) = parsed else {
        bail!("unexpected notification: {parsed:?}");
    };
    assert_eq!(payload.login_id, Some(login_id));
    assert_eq!(payload.success, false);
    assert!(
        payload
            .error
            .as_deref()
            .is_some_and(|error| error.contains("device auth failed with status")),
        "unexpected error: {:?}",
        payload.error
    );

    let maybe_updated = timeout(
        Duration::from_millis(500),
        mcp.read_stream_until_notification_message("account/updated"),
    )
    .await;
    assert!(
        maybe_updated.is_err(),
        "account/updated should not be emitted when device code login fails"
    );
    assert!(
        !codex_home.path().join("auth.json").exists(),
        "auth.json should not be created when device code login fails"
    );
    Ok(())
}

#[tokio::test]
async fn login_account_chatgpt_device_code_can_be_cancelled() -> Result<()> {
    let codex_home = TempDir::new()?;
    let mock_server = MockServer::start().await;
    create_config_toml(
        codex_home.path(),
        CreateConfigTomlParams {
            requires_openai_auth: Some(true),
            base_url: Some(format!("{}/v1", mock_server.uri())),
            ..Default::default()
        },
    )?;
    write_models_cache(codex_home.path())?;

    mock_device_code_usercode(&mock_server, /*interval_seconds*/ 1).await;
    mock_device_code_token_failure(&mock_server, /*status*/ 404).await;

    let issuer = mock_server.uri();
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .with_env_overrides(&[
            ("OPENAI_API_KEY", None),
            (LOGIN_ISSUER_ENV_VAR, Some(issuer.as_str())),
        ])
        .build()
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let request_id = mcp.send_login_account_chatgpt_device_code_request().await?;
    let resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(request_id)),
    )
    .await??;
    let login: LoginAccountResponse = to_response(resp)?;
    let LoginAccountResponse::ChatgptDeviceCode { login_id, .. } = login else {
        bail!("unexpected login response: {login:?}");
    };

    let cancel_id = mcp
        .send_cancel_login_account_request(CancelLoginAccountParams {
            login_id: login_id.clone(),
        })
        .await?;
    let cancel_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(cancel_id)),
    )
    .await??;
    let cancel: CancelLoginAccountResponse = to_response(cancel_resp)?;
    assert_eq!(cancel.status, CancelLoginAccountStatus::Canceled);

    let note = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("account/login/completed"),
    )
    .await??;
    let parsed: ServerNotification = note.try_into()?;
    let ServerNotification::AccountLoginCompleted(payload) = parsed else {
        bail!("unexpected notification: {parsed:?}");
    };
    assert_eq!(payload.login_id, Some(login_id));
    assert_eq!(payload.success, false);
    assert!(
        payload.error.is_some(),
        "expected a non-empty error on device code cancel"
    );

    let maybe_updated = timeout(
        Duration::from_millis(500),
        mcp.read_stream_until_notification_message("account/updated"),
    )
    .await;
    assert!(
        maybe_updated.is_err(),
        "account/updated should not be emitted when device code login is cancelled"
    );
    assert!(
        !codex_home.path().join("auth.json").exists(),
        "auth.json should not be created when device code login is cancelled"
    );
    Ok(())
}

#[tokio::test]
// Serialize tests that launch the login server since it binds to a fixed port.
#[serial(login_port)]
async fn login_account_chatgpt_start_can_be_cancelled() -> Result<()> {
    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path(), CreateConfigTomlParams::default())?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build()
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let request_id = mcp.send_login_account_chatgpt_request().await?;
    let resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(request_id)),
    )
    .await??;

    let login: LoginAccountResponse = to_response(resp)?;
    let LoginAccountResponse::Chatgpt { login_id, auth_url } = login else {
        bail!("unexpected login response: {login:?}");
    };
    assert!(
        auth_url.contains("redirect_uri=http%3A%2F%2Flocalhost"),
        "auth_url should contain a redirect_uri to localhost"
    );

    let cancel_id = mcp
        .send_cancel_login_account_request(CancelLoginAccountParams {
            login_id: login_id.clone(),
        })
        .await?;
    let cancel_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(cancel_id)),
    )
    .await??;
    let _ok: CancelLoginAccountResponse = to_response(cancel_resp)?;

    let note = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("account/login/completed"),
    )
    .await??;
    let parsed: ServerNotification = note.try_into()?;
    let ServerNotification::AccountLoginCompleted(payload) = parsed else {
        bail!("unexpected notification: {parsed:?}");
    };
    pretty_assertions::assert_eq!(payload.login_id, Some(login_id));
    pretty_assertions::assert_eq!(payload.success, false);
    assert!(
        payload.error.is_some(),
        "expected a non-empty error on cancel"
    );

    let maybe_updated = timeout(
        Duration::from_millis(500),
        mcp.read_stream_until_notification_message("account/updated"),
    )
    .await;
    assert!(
        maybe_updated.is_err(),
        "account/updated should not be emitted when login is cancelled"
    );
    Ok(())
}

#[tokio::test]
// Serialize tests that launch the login server since it binds to a fixed port.
#[serial(login_port)]
async fn login_account_chatgpt_uses_debug_oauth_overrides() -> Result<()> {
    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path(), CreateConfigTomlParams::default())?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .with_env_overrides(&[
            (CLIENT_ID_OVERRIDE_ENV_VAR, Some("staging-client")),
            (LOGIN_ISSUER_ENV_VAR, Some("https://auth.example.com")),
        ])
        .build()
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let request_id = mcp.send_login_account_chatgpt_request().await?;
    let resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(request_id)),
    )
    .await??;

    let login: LoginAccountResponse = to_response(resp)?;
    let LoginAccountResponse::Chatgpt { login_id, auth_url } = login else {
        bail!("unexpected login response: {login:?}");
    };
    let auth_url = Url::parse(&auth_url)?;
    assert_eq!(
        auth_url.origin().ascii_serialization(),
        "https://auth.example.com"
    );
    assert_eq!(
        auth_url
            .query_pairs()
            .find_map(|(key, value)| (key == "client_id").then_some(value.into_owned())),
        Some("staging-client".to_string())
    );

    let cancel_id = mcp
        .send_cancel_login_account_request(CancelLoginAccountParams { login_id })
        .await?;
    let cancel_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(cancel_id)),
    )
    .await??;
    let _: CancelLoginAccountResponse = to_response(cancel_resp)?;
    Ok(())
}

#[tokio::test]
// Serialize tests that launch the login server since it binds to a fixed port.
#[serial(login_port)]
async fn login_account_chatgpt_redirects_to_hosted_success_page() -> Result<()> {
    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path(), CreateConfigTomlParams::default())?;
    let mock_server = MockServer::start().await;
    let id_token = encode_id_token(
        &ChatGptIdTokenClaims::new()
            .email("hosted@example.com")
            .plan_type("pro")
            .chatgpt_account_id(WORKSPACE_ID_EMBEDDED),
    )?;
    mock_oauth_token(&mock_server, &id_token).await;
    let issuer = mock_server.uri();

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .with_env_overrides(&[
            (LOGIN_ISSUER_ENV_VAR, Some(issuer.as_str())),
            (
                LOGIN_OPEN_APP_URL_ENV_VAR,
                Some("http://localhost:3000/codex/open-app"),
            ),
        ])
        .build()
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let request_id = mcp
        .send_login_account_request(json!({
            "type": "chatgpt",
            "appBrand": "chatgpt",
            "useHostedLoginSuccessPage": true,
        }))
        .await?;
    let resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(request_id)),
    )
    .await??;
    let login: LoginAccountResponse = to_response(resp)?;
    let LoginAccountResponse::Chatgpt { auth_url, .. } = login else {
        bail!("unexpected login response: {login:?}");
    };
    let auth_url = Url::parse(&auth_url)?;
    let callback_url = auth_url
        .query_pairs()
        .find_map(|(key, value)| (key == "redirect_uri").then(|| value.into_owned()))
        .ok_or_else(|| anyhow::anyhow!("missing redirect_uri"))?;
    let state = auth_url
        .query_pairs()
        .find_map(|(key, value)| (key == "state").then(|| value.into_owned()))
        .ok_or_else(|| anyhow::anyhow!("missing state"))?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?;

    let response = client
        .get(format!("{callback_url}?code=test-code&state={state}"))
        .send()
        .await?;

    assert_eq!(response.status(), 302);
    assert_eq!(
        response.headers()["location"].to_str()?,
        "http://localhost:3000/codex/open-app?source=login&app_brand=chatgpt"
    );
    Ok(())
}

#[tokio::test]
// Serialize tests that launch the login server since it binds to a fixed port.
#[serial(login_port)]
async fn login_account_chatgpt_includes_forced_workspace_query_param() -> Result<()> {
    let codex_home = TempDir::new()?;
    create_config_toml(
        codex_home.path(),
        CreateConfigTomlParams {
            forced_workspace_id: Some(WORKSPACE_ID_ALLOWED.to_string()),
            ..Default::default()
        },
    )?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build()
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let request_id = mcp.send_login_account_chatgpt_request().await?;
    let resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(request_id)),
    )
    .await??;

    let login: LoginAccountResponse = to_response(resp)?;
    let LoginAccountResponse::Chatgpt { auth_url, .. } = login else {
        bail!("unexpected login response: {login:?}");
    };
    assert!(
        auth_url.contains(&format!("allowed_workspace_id={WORKSPACE_ID_ALLOWED}")),
        "auth URL should include forced workspace"
    );
    Ok(())
}

#[tokio::test]
// Serialize tests that launch the login server since it binds to a fixed port.
#[serial(login_port)]
async fn login_account_chatgpt_includes_forced_workspace_allowlist_query_param() -> Result<()> {
    let codex_home = TempDir::new()?;
    create_config_toml(
        codex_home.path(),
        CreateConfigTomlParams {
            forced_workspace_ids: Some(vec![
                WORKSPACE_ID_ALLOWED.to_string(),
                WORKSPACE_ID_SECOND_ALLOWED.to_string(),
            ]),
            ..Default::default()
        },
    )?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build()
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let request_id = mcp.send_login_account_chatgpt_request().await?;
    let resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(request_id)),
    )
    .await??;

    let login: LoginAccountResponse = to_response(resp)?;
    let LoginAccountResponse::Chatgpt { auth_url, .. } = login else {
        bail!("unexpected login response: {login:?}");
    };
    let auth_url = Url::parse(&auth_url)?;
    let allowed_workspace_ids = auth_url
        .query_pairs()
        .filter_map(|(key, value)| (key == "allowed_workspace_id").then(|| value.into_owned()))
        .collect::<Vec<_>>();
    assert_eq!(
        allowed_workspace_ids,
        vec![format!(
            "{WORKSPACE_ID_ALLOWED},{WORKSPACE_ID_SECOND_ALLOWED}"
        )]
    );
    Ok(())
}

#[tokio::test]
async fn get_account_no_auth() -> Result<()> {
    let codex_home = TempDir::new()?;
    create_config_toml(
        codex_home.path(),
        CreateConfigTomlParams {
            requires_openai_auth: Some(true),
            ..Default::default()
        },
    )?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .with_env_overrides(&[("OPENAI_API_KEY", None)])
        .build()
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let params = GetAccountParams {
        refresh_token: false,
    };
    let request_id = mcp.send_get_account_request(params).await?;

    let resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(request_id)),
    )
    .await??;
    let account: GetAccountResponse = to_response(resp)?;

    assert_eq!(account.account, None, "expected no account");
    assert_eq!(account.requires_openai_auth, true);
    Ok(())
}

#[tokio::test]
async fn get_account_when_auth_not_required() -> Result<()> {
    let codex_home = TempDir::new()?;
    create_config_toml(
        codex_home.path(),
        CreateConfigTomlParams {
            requires_openai_auth: Some(false),
            ..Default::default()
        },
    )?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build()
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let params = GetAccountParams {
        refresh_token: false,
    };
    let request_id = mcp.send_get_account_request(params).await?;

    let resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(request_id)),
    )
    .await??;
    let received: GetAccountResponse = to_response(resp)?;

    let expected = GetAccountResponse {
        account: None,
        requires_openai_auth: false,
    };
    assert_eq!(received, expected);
    Ok(())
}
#[tokio::test]
async fn account_reads_use_startup_config_when_config_reload_fails() -> Result<()> {
    let codex_home = TempDir::new()?;
    create_config_toml(
        codex_home.path(),
        CreateConfigTomlParams {
            model_provider_id: Some("amazon-bedrock".to_string()),
            extra_provider_config: Some(
                r#"[model_providers.amazon-bedrock.aws]
profile = "codex-bedrock"
region = "us-west-2"
"#
                .to_string(),
            ),
            ..Default::default()
        },
    )?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build()
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    std::fs::write(codex_home.path().join("config.toml"), "invalid = [")?;

    assert_eq!(
        read_account(&mut mcp).await?,
        GetAccountResponse {
            account: None,
            requires_openai_auth: false,
        }
    );

    let request_id = mcp
        .send_get_auth_status_request(GetAuthStatusParams {
            include_token: Some(false),
            refresh_token: Some(false),
        })
        .await?;
    let response = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(request_id)),
    )
    .await??;
    assert_eq!(
        to_response::<GetAuthStatusResponse>(response)?,
        GetAuthStatusResponse {
            auth_method: None,
            auth_token: None,
            requires_openai_auth: Some(false),
        }
    );

    Ok(())
}

#[tokio::test]
async fn get_account_with_chatgpt() -> Result<()> {
    let codex_home = TempDir::new()?;
    create_config_toml(
        codex_home.path(),
        CreateConfigTomlParams {
            requires_openai_auth: Some(true),
            ..Default::default()
        },
    )?;
    write_chatgpt_auth(
        codex_home.path(),
        ChatGptAuthFixture::new("access-chatgpt")
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

    let params = GetAccountParams {
        refresh_token: false,
    };
    let request_id = mcp.send_get_account_request(params).await?;

    let resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(request_id)),
    )
    .await??;
    let received: GetAccountResponse = to_response(resp)?;

    let expected = GetAccountResponse {
        account: Some(Account::Chatgpt {
            email: Some("user@example.com".to_string()),
            plan_type: AccountPlanType::Pro,
        }),
        requires_openai_auth: true,
    };
    assert_eq!(received, expected);
    Ok(())
}

#[tokio::test]
async fn get_account_with_chatgpt_without_email() -> Result<()> {
    let codex_home = TempDir::new()?;
    create_config_toml(
        codex_home.path(),
        CreateConfigTomlParams {
            requires_openai_auth: Some(true),
            ..Default::default()
        },
    )?;
    write_chatgpt_auth(
        codex_home.path(),
        ChatGptAuthFixture::new("access-chatgpt").plan_type("pro"),
        AuthCredentialsStoreMode::File,
    )?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .with_env_overrides(&[("OPENAI_API_KEY", None)])
        .build()
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let request_id = mcp
        .send_get_account_request(GetAccountParams {
            refresh_token: false,
        })
        .await?;
    let response: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(request_id)),
    )
    .await??;
    let received: GetAccountResponse = to_response(response)?;

    assert_eq!(
        received,
        GetAccountResponse {
            account: Some(Account::Chatgpt {
                email: None,
                plan_type: AccountPlanType::Pro,
            }),
            requires_openai_auth: true,
        }
    );
    Ok(())
}

#[tokio::test]
async fn get_account_omits_chatgpt_after_permanent_refresh_failure() -> Result<()> {
    let codex_home = TempDir::new()?;
    create_config_toml(
        codex_home.path(),
        CreateConfigTomlParams {
            requires_openai_auth: Some(true),
            ..Default::default()
        },
    )?;
    write_chatgpt_auth(
        codex_home.path(),
        ChatGptAuthFixture::new("stale-access-token")
            .refresh_token("stale-refresh-token")
            .account_id(WORKSPACE_ID_STALE)
            .email("user@example.com")
            .plan_type("pro")
            .last_refresh(Some(Utc::now() - ChronoDuration::days(9))),
        AuthCredentialsStoreMode::File,
    )?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .with_env_overrides(&[("OPENAI_API_KEY", None)])
        .build()
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;
    app_test_support::mark_pool_auth_failed(codex_home.path(), WORKSPACE_ID_STALE).await?;

    let auth_status_request_id = mcp
        .send_get_auth_status_request(GetAuthStatusParams {
            include_token: Some(true),
            refresh_token: Some(true),
        })
        .await?;
    let auth_status_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(auth_status_request_id)),
    )
    .await??;
    let _: GetAuthStatusResponse = to_response(auth_status_resp)?;

    let request_id = mcp
        .send_get_account_request(GetAccountParams {
            refresh_token: false,
        })
        .await?;

    let resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(request_id)),
    )
    .await??;
    let received: GetAccountResponse = to_response(resp)?;

    assert_eq!(
        received,
        GetAccountResponse {
            account: None,
            requires_openai_auth: true,
        }
    );
    Ok(())
}

#[tokio::test]
async fn get_account_with_chatgpt_missing_plan_claim_returns_unknown() -> Result<()> {
    let codex_home = TempDir::new()?;
    create_config_toml(
        codex_home.path(),
        CreateConfigTomlParams {
            requires_openai_auth: Some(true),
            ..Default::default()
        },
    )?;
    write_chatgpt_auth(
        codex_home.path(),
        ChatGptAuthFixture::new("access-chatgpt").email("user@example.com"),
        AuthCredentialsStoreMode::File,
    )?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .with_env_overrides(&[("OPENAI_API_KEY", None)])
        .build()
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let params = GetAccountParams {
        refresh_token: false,
    };
    let request_id = mcp.send_get_account_request(params).await?;

    let resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(request_id)),
    )
    .await??;
    let received: GetAccountResponse = to_response(resp)?;

    let expected = GetAccountResponse {
        account: Some(Account::Chatgpt {
            email: Some("user@example.com".to_string()),
            plan_type: AccountPlanType::Unknown,
        }),
        requires_openai_auth: true,
    };
    assert_eq!(received, expected);
    Ok(())
}
