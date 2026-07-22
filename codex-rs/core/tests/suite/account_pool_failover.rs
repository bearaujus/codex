use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use anyhow::Context;
use anyhow::Result;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::Utc;
use codex_config::types::AuthCredentialsStoreMode;
use codex_login::AuthDotJson;
use codex_login::AuthKeyringBackendKind;
use codex_login::AuthManager;
use codex_login::ChatgptAccountPool;
use codex_login::token_data::TokenData;
use codex_login::token_data::parse_chatgpt_jwt_claims;
use codex_protocol::auth::AuthMode;
use codex_protocol::auth::RefreshTokenFailedError;
use codex_protocol::auth::RefreshTokenFailedReason;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_image_generation_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::sse;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use tempfile::TempDir;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::Request;
use wiremock::Respond;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn usage_limit_failsover_to_second_pool_account_and_retries() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = MockServer::start().await;
    let home = Arc::new(TempDir::new()?);
    let chatgpt_base_url = format!("{}/backend-api", server.uri());
    let auth_manager = seed_two_account_pool(home.path(), Some(chatgpt_base_url.as_str())).await?;

    assert_eq!(
        auth_manager
            .auth_cached()
            .and_then(|auth| auth.get_pool_account_id()),
        Some("workspace-1".to_string())
    );
    let first_token = auth_manager
        .auth_cached()
        .and_then(|auth| auth.get_token().ok())
        .context("first account token")?;

    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(UsageLimitThenSuccess {
            call: AtomicUsize::new(0),
            success_body: sse(vec![
                ev_response_created("resp-failover"),
                ev_assistant_message("msg-1", "switched accounts"),
                ev_completed("resp-failover"),
            ]),
        })
        .expect(2)
        .mount(&server)
        .await;

    let mut builder = test_codex()
        .with_home(Arc::clone(&home))
        .with_auth_manager(Arc::clone(&auth_manager));
    let test = builder.build_with_auto_env(&server).await?;

    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "hello".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;

    wait_for_event(&test.codex, |msg| matches!(msg, EventMsg::TurnComplete(_))).await;

    assert_eq!(
        auth_manager
            .auth_cached()
            .and_then(|auth| auth.get_pool_account_id()),
        Some("workspace-2".to_string()),
        "usage-limit failover should activate the second pool account"
    );
    let second_token = auth_manager
        .auth_cached()
        .and_then(|auth| auth.get_token().ok())
        .context("failover account token")?;
    assert_ne!(first_token, second_token);

    let response_requests = received_responses_requests(&server).await;
    assert_eq!(
        response_requests.len(),
        2,
        "expected usage-limit retry within the turn; got paths: {:?}",
        response_requests
            .iter()
            .map(|request| request.url.path().to_string())
            .collect::<Vec<_>>()
    );
    assert!(
        response_requests[0]
            .headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.contains(&first_token)),
        "first request should use the exhausted account token"
    );
    assert!(
        response_requests[1]
            .headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.contains(&second_token)),
        "retry should use the failover account token"
    );

    let accounts = auth_manager
        .chatgpt_account_pool()
        .context("account pool should exist")?
        .list_accounts()
        .await?;
    let cooled = accounts
        .iter()
        .find(|account| account.account_id == "workspace-1")
        .context("workspace-1 should remain in the pool")?;
    assert!(
        cooled.cooldown_until.is_some(),
        "exhausted account should be marked with a cooldown"
    );
    server.verify().await;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn usage_limit_without_eligible_failover_emits_error() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = MockServer::start().await;
    let home = Arc::new(TempDir::new()?);
    let chatgpt_base_url = format!("{}/backend-api", server.uri());
    let auth_manager =
        seed_single_account_pool(home.path(), Some(chatgpt_base_url.as_str())).await?;

    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(usage_limit_response())
        .expect(1)
        .mount(&server)
        .await;

    let mut builder = test_codex()
        .with_home(Arc::clone(&home))
        .with_auth_manager(Arc::clone(&auth_manager));
    let test = builder.build_with_auto_env(&server).await?;

    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "hello".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;

    let error = wait_for_event(&test.codex, |msg| matches!(msg, EventMsg::Error(_))).await;
    let EventMsg::Error(error) = error else {
        unreachable!();
    };
    assert!(
        error.message.contains("spend cap")
            || error.message.to_lowercase().contains("limit")
            || error.message.to_lowercase().contains("usage"),
        "expected a usage-limit error message, got: {}",
        error.message
    );

    assert_eq!(
        auth_manager
            .auth_cached()
            .and_then(|auth| auth.get_pool_account_id()),
        Some("workspace-1".to_string()),
        "without a fallback account the active pool account should stay selected"
    );

    let accounts = auth_manager
        .chatgpt_account_pool()
        .context("account pool should exist")?
        .list_accounts()
        .await?;
    let cooled = accounts
        .iter()
        .find(|account| account.account_id == "workspace-1")
        .context("workspace-1 should remain in the pool")?;
    assert!(
        cooled.cooldown_until.is_some(),
        "exhausted sole account should still receive a cooldown marker"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn done_only_assistant_output_is_not_replayed_after_stream_failure() -> Result<()> {
    skip_if_no_network!(Ok(()));

    assert_done_output_is_not_replayed(ev_assistant_message("msg-partial", "already delivered"))
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn done_only_image_generation_is_not_replayed_after_stream_failure() -> Result<()> {
    skip_if_no_network!(Ok(()));

    assert_done_output_is_not_replayed(ev_image_generation_call(
        "ig-partial",
        "completed",
        "already generated",
        &"a".repeat(20_000),
    ))
    .await
}

async fn assert_done_output_is_not_replayed(output_event: Value) -> Result<()> {
    let server = MockServer::start().await;
    let home = Arc::new(TempDir::new()?);
    let chatgpt_base_url = format!("{}/backend-api", server.uri());
    let auth_manager = seed_two_account_pool(home.path(), Some(chatgpt_base_url.as_str())).await?;
    let response_body = sse(vec![
        ev_response_created("resp-partial"),
        output_event,
        json!({
            "type": "response.failed",
            "response": {
                "id": "resp-partial",
                "error": {
                    "code": "server_error",
                    "message": "synthetic retryable stream failure"
                }
            }
        }),
    ]);
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(response_body),
        )
        .expect(1)
        .mount(&server)
        .await;

    let mut builder = test_codex()
        .with_home(Arc::clone(&home))
        .with_auth_manager(auth_manager);
    let test = builder.build_with_auto_env(&server).await?;

    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "hello".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;

    wait_for_event(&test.codex, |msg| matches!(msg, EventMsg::Error(_))).await;
    assert_eq!(received_responses_requests(&server).await.len(), 1);

    Ok(())
}

#[tokio::test]
async fn late_auth_failure_does_not_poison_the_active_failover_account() -> Result<()> {
    let home = TempDir::new()?;
    let auth_manager = seed_two_account_pool(home.path(), None).await?;

    assert!(
        !auth_manager
            .handle_chatgpt_account_pool_usage_limit(
                Some("workspace-1"),
                /*safe_to_retry*/ false,
                /*snapshot*/ None,
                Some(Utc::now() + chrono::Duration::hours(1)),
            )
            .await?
    );
    let failover_auth = auth_manager.auth_cached().context("failover auth")?;
    assert_eq!(
        failover_auth.get_pool_account_id().as_deref(),
        Some("workspace-2")
    );

    let error = RefreshTokenFailedError::new(
        RefreshTokenFailedReason::Exhausted,
        "late failure from workspace-1",
    );
    assert!(
        !auth_manager
            .handle_chatgpt_account_pool_auth_failure(
                Some("workspace-1"),
                /*safe_to_retry*/ false,
                &error,
            )
            .await?
    );

    assert_eq!(
        auth_manager.refresh_failure_for_auth(&failover_auth),
        None,
        "a late failure from the previous account must not poison active auth"
    );
    assert_eq!(
        auth_manager
            .auth_cached()
            .and_then(|auth| auth.get_pool_account_id())
            .as_deref(),
        Some("workspace-2")
    );

    Ok(())
}

struct UsageLimitThenSuccess {
    call: AtomicUsize,
    success_body: String,
}

impl Respond for UsageLimitThenSuccess {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        let call = self.call.fetch_add(1, Ordering::SeqCst);
        if call == 0 {
            usage_limit_response()
        } else {
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(self.success_body.clone())
        }
    }
}

fn usage_limit_response() -> ResponseTemplate {
    ResponseTemplate::new(429)
        .insert_header("x-codex-primary-used-percent", "100.0")
        .insert_header("x-codex-secondary-used-percent", "100.0")
        .insert_header("x-codex-primary-window-minutes", "15")
        .insert_header("x-codex-secondary-window-minutes", "60")
        .insert_header(
            "x-codex-rate-limit-reached-type",
            "workspace_member_usage_limit_reached",
        )
        .set_body_json(json!({
            "error": {
                "type": "usage_limit_reached",
                "message": "limit reached",
                "resets_at": Utc::now().timestamp() + 3600,
                "plan_type": "pro"
            }
        }))
}

async fn received_responses_requests(server: &MockServer) -> Vec<wiremock::Request> {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|request| request.url.path() == "/v1/responses")
        .collect()
}

async fn seed_two_account_pool(
    codex_home: &Path,
    chatgpt_base_url: Option<&str>,
) -> Result<Arc<AuthManager>> {
    let pool = ChatgptAccountPool::open(
        codex_home.to_path_buf(),
        AuthCredentialsStoreMode::File,
        chatgpt_base_url.map(str::to_string),
    )
    .await?;
    pool.register_account(&chatgpt_auth("one@example.com", "workspace-1", "pro"))
        .await?;
    pool.register_account(&chatgpt_auth("two@example.com", "workspace-2", "pro"))
        .await?;
    drop(pool);

    Ok(AuthManager::shared(
        codex_home.to_path_buf(),
        AuthCredentialsStoreMode::File,
        /*forced_chatgpt_workspace_id*/ None,
        chatgpt_base_url.map(str::to_string),
        AuthKeyringBackendKind::default(),
        /*auth_route_config*/ None,
    )
    .await)
}

async fn seed_single_account_pool(
    codex_home: &Path,
    chatgpt_base_url: Option<&str>,
) -> Result<Arc<AuthManager>> {
    let pool = ChatgptAccountPool::open(
        codex_home.to_path_buf(),
        AuthCredentialsStoreMode::File,
        chatgpt_base_url.map(str::to_string),
    )
    .await?;
    pool.register_account(&chatgpt_auth("one@example.com", "workspace-1", "pro"))
        .await?;
    drop(pool);

    Ok(AuthManager::shared(
        codex_home.to_path_buf(),
        AuthCredentialsStoreMode::File,
        /*forced_chatgpt_workspace_id*/ None,
        chatgpt_base_url.map(str::to_string),
        AuthKeyringBackendKind::default(),
        /*auth_route_config*/ None,
    )
    .await)
}

fn chatgpt_auth(email: &str, account_id: &str, plan_type: &str) -> AuthDotJson {
    let id_token = fake_jwt(email, account_id, plan_type);
    AuthDotJson {
        auth_mode: Some(AuthMode::Chatgpt),
        tokens: Some(TokenData {
            id_token: parse_chatgpt_jwt_claims(&id_token).expect("id token should parse"),
            access_token: access_token_for(account_id),
            refresh_token: format!("refresh-{account_id}"),
            account_id: Some(account_id.to_string()),
        }),
        pool_account_id: Some(account_id.to_string()),
        last_refresh: Some(Utc::now()),
        agent_identity: None,
    }
}

fn access_token_for(account_id: &str) -> String {
    fake_unsigned_jwt(json!({
        "exp": Utc::now().timestamp() + 3600,
        "https://api.openai.com/auth": {
            "chatgpt_account_id": account_id,
        },
    }))
}

fn fake_jwt(email: &str, account_id: &str, plan_type: &str) -> String {
    fake_unsigned_jwt(json!({
        "email": email,
        "https://api.openai.com/auth": {
            "chatgpt_account_id": account_id,
            "chatgpt_plan_type": plan_type,
        },
    }))
}

fn fake_unsigned_jwt(payload: serde_json::Value) -> String {
    #[derive(Serialize)]
    struct Header {
        alg: &'static str,
        typ: &'static str,
    }

    let header = Header {
        alg: "none",
        typ: "JWT",
    };
    let header_b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header).expect("header"));
    let payload_b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).expect("payload"));
    let signature_b64 = URL_SAFE_NO_PAD.encode(b"sig");
    format!("{header_b64}.{payload_b64}.{signature_b64}")
}
