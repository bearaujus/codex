use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

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
use codex_login::account_pool_db_path;
use codex_login::token_data::TokenData;
use codex_login::token_data::parse_chatgpt_jwt_claims;
use codex_protocol::auth::AuthMode;
use codex_protocol::auth::RefreshTokenFailedError;
use codex_protocol::auth::RefreshTokenFailedReason;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::RateLimitSnapshot;
use codex_protocol::protocol::RateLimitWindow;
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
async fn auxiliary_limit_failsover_when_cached_codex_window_is_exhausted() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = MockServer::start().await;
    let home = Arc::new(TempDir::new()?);
    let chatgpt_base_url = format!("{}/backend-api", server.uri());
    let auth_manager = seed_two_account_pool(home.path(), Some(chatgpt_base_url.as_str())).await?;
    let reset_at = Utc::now().timestamp() + 3_600;
    seed_cached_codex_exhaustion(home.path(), "workspace-1", reset_at).await?;

    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(AuxiliaryLimitThenSuccess {
            call: AtomicUsize::new(0),
            success_body: sse(vec![
                ev_response_created("resp-auxiliary-failover"),
                ev_assistant_message("msg-1", "switched accounts"),
                ev_completed("resp-auxiliary-failover"),
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
        "an exhausted Codex window plus rejected auxiliary overflow should fail over"
    );
    assert_eq!(received_responses_requests(&server).await.len(), 2);

    let pool = auth_manager
        .chatgpt_account_pool()
        .context("account pool should exist")?;
    let accounts = pool.list_accounts().await?;
    let exhausted = accounts
        .iter()
        .find(|account| account.account_id == "workspace-1")
        .context("workspace-1 should remain in the pool")?;
    assert_eq!(exhausted.cooldown_until, Some(reset_at));
    let limits = pool.list_rate_limits().await?;
    let exhausted_limits = limits
        .iter()
        .find(|entry| entry.account_id == "workspace-1")
        .context("workspace-1 rate limits should remain available")?;
    assert!(
        exhausted_limits.rate_limits.contains_key("premium"),
        "the rejected auxiliary limit should still be persisted for visibility"
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
async fn manual_compaction_usage_limit_preselects_and_failsover_for_the_next_turn() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = MockServer::start().await;
    let home = Arc::new(TempDir::new()?);
    let chatgpt_base_url = format!("{}/backend-api", server.uri());
    let auth_manager = seed_two_account_pool(home.path(), Some(chatgpt_base_url.as_str())).await?;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(SuccessThenUsageLimit {
            call: AtomicUsize::new(0),
            success_body: sse(vec![
                ev_response_created("resp-before-compact"),
                ev_assistant_message("msg-before-compact", "before compact"),
                ev_completed("resp-before-compact"),
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
                text: "seed history".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_event(&test.codex, |msg| matches!(msg, EventMsg::TurnComplete(_))).await;

    test.codex.submit(Op::Compact).await?;
    wait_for_event(&test.codex, |msg| matches!(msg, EventMsg::Error(_))).await;
    wait_for_event(&test.codex, |msg| matches!(msg, EventMsg::TurnComplete(_))).await;

    assert_eq!(
        auth_manager
            .auth_cached()
            .and_then(|auth| auth.get_pool_account_id())
            .as_deref(),
        Some("workspace-2"),
        "manual compaction must leave the next turn on an eligible account"
    );
    let accounts = auth_manager
        .chatgpt_account_pool()
        .context("account pool should exist")?
        .list_accounts()
        .await?;
    let exhausted = accounts
        .iter()
        .find(|account| account.account_id == "workspace-1")
        .context("first account should exist")?;
    assert!(
        exhausted.cooldown_until.is_some(),
        "the account serving the failed compaction request must receive the cooldown"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failure_is_attributed_to_auth_attached_after_in_request_pool_failover() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = MockServer::start().await;
    let home = Arc::new(TempDir::new()?);
    let chatgpt_base_url = format!("{}/backend-api", server.uri());
    let auth_manager = seed_two_account_pool(home.path(), Some(chatgpt_base_url.as_str())).await?;
    let first_request_seen = Arc::new(AtomicBool::new(false));
    let second_request_seen = Arc::new(AtomicBool::new(false));
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(UnauthorizedThenUsageLimit {
            call: AtomicUsize::new(0),
            first_request_seen: Arc::clone(&first_request_seen),
            second_request_seen: Arc::clone(&second_request_seen),
        })
        .expect(2)
        .mount(&server)
        .await;

    let db_path = account_pool_db_path(home.path());
    let mutation_db_path = db_path.clone();
    let mutation = tokio::spawn(async move {
        while !first_request_seen.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let mut connection = codex_state::open_existing_sqlite_connection(&mutation_db_path)
            .await
            .expect("pool DB should open");
        sqlx::query("UPDATE accounts SET access_token = NULL WHERE account_id = 'workspace-1'")
            .execute(&mut connection)
            .await
            .expect("first account secret should be removed");
    });
    let activity_probe = tokio::spawn(async move {
        while !second_request_seen.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let mut connection = codex_state::open_existing_sqlite_connection(&db_path)
            .await
            .expect("pool DB should open for activity probe");
        sqlx::query_scalar::<_, String>(
            "SELECT account_id FROM account_activity ORDER BY account_id",
        )
        .fetch_all(&mut connection)
        .await
        .expect("in-flight activity rows should be readable")
    });

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

    wait_for_event(&test.codex, |msg| matches!(msg, EventMsg::Error(_))).await;
    mutation.await?;
    let active_request_accounts = activity_probe.await?;
    assert_eq!(
        active_request_accounts,
        vec!["workspace-2".to_string()],
        "the request-level lease must move before the recovered credential is sent"
    );
    let accounts = auth_manager
        .chatgpt_account_pool()
        .context("account pool should exist")?
        .list_accounts()
        .await?;
    let first = accounts
        .iter()
        .find(|account| account.account_id == "workspace-1")
        .context("first account should exist")?;
    let second = accounts
        .iter()
        .find(|account| account.account_id == "workspace-2")
        .context("second account should exist")?;
    assert_eq!(
        first.auth_status,
        codex_login::ChatgptAccountPoolAuthStatus::MissingSecret
    );
    assert!(
        second.cooldown_until.is_some(),
        "the 429 belongs to the second account whose auth was attached after 401 recovery"
    );
    assert_eq!(
        first.cooldown_until, None,
        "the pre-request account snapshot must not receive the second request's usage limit"
    );

    let requests = received_responses_requests(&server).await;
    assert_eq!(requests.len(), 2);
    assert_ne!(
        requests[0].headers.get("authorization"),
        requests[1].headers.get("authorization"),
        "401 recovery should have attached the failover account to the second request"
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

struct AuxiliaryLimitThenSuccess {
    call: AtomicUsize,
    success_body: String,
}

struct SuccessThenUsageLimit {
    call: AtomicUsize,
    success_body: String,
}

struct UnauthorizedThenUsageLimit {
    call: AtomicUsize,
    first_request_seen: Arc<AtomicBool>,
    second_request_seen: Arc<AtomicBool>,
}

impl Respond for UnauthorizedThenUsageLimit {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        if self.call.fetch_add(1, Ordering::SeqCst) == 0 {
            self.first_request_seen.store(true, Ordering::SeqCst);
            ResponseTemplate::new(401)
                .set_delay(Duration::from_millis(500))
                .set_body_json(json!({
                    "error": {
                        "type": "invalid_request_error",
                        "code": "invalid_token",
                        "message": "synthetic rejected token"
                    }
                }))
        } else {
            self.second_request_seen.store(true, Ordering::SeqCst);
            usage_limit_response().set_delay(Duration::from_millis(500))
        }
    }
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

impl Respond for AuxiliaryLimitThenSuccess {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        let call = self.call.fetch_add(1, Ordering::SeqCst);
        if call == 0 {
            auxiliary_usage_limit_response()
        } else {
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(self.success_body.clone())
        }
    }
}

impl Respond for SuccessThenUsageLimit {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        if self.call.fetch_add(1, Ordering::SeqCst) == 0 {
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(self.success_body.clone())
        } else {
            usage_limit_response()
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

fn auxiliary_usage_limit_response() -> ResponseTemplate {
    ResponseTemplate::new(429)
        .insert_header("x-codex-active-limit", "premium")
        .insert_header("x-codex-credits-has-credits", "false")
        .insert_header("x-codex-credits-unlimited", "false")
        .insert_header("x-codex-credits-balance", "0")
        .insert_header(
            "x-codex-rate-limit-reached-type",
            "workspace_owner_credits_depleted",
        )
        .set_body_json(json!({
            "error": {
                "type": "usage_limit_reached",
                "message": "limit reached",
                "resets_at": Utc::now().timestamp() + 3600,
                "plan_type": "team"
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

async fn seed_cached_codex_exhaustion(
    codex_home: &Path,
    account_id: &str,
    reset_at: i64,
) -> Result<()> {
    let snapshot = RateLimitSnapshot {
        limit_id: Some("codex".to_string()),
        limit_name: Some("Codex".to_string()),
        primary: Some(RateLimitWindow {
            used_percent: 100.0,
            window_minutes: Some(10_080),
            resets_at: Some(reset_at),
        }),
        secondary: None,
        credits: None,
        individual_limit: None,
        spend_control_reached: None,
        plan_type: None,
        rate_limit_reached_type: None,
    };
    let mut connection =
        codex_state::open_existing_sqlite_connection(&account_pool_db_path(codex_home)).await?;
    sqlx::query(
        r#"
        INSERT INTO account_rate_limits (account_id, limit_id, snapshot_json, fetched_at)
        VALUES (?, 'codex', ?, ?)
        ON CONFLICT(account_id, limit_id) DO UPDATE SET
            snapshot_json = excluded.snapshot_json,
            fetched_at = excluded.fetched_at
        "#,
    )
    .bind(account_id)
    .bind(serde_json::to_string(&snapshot)?)
    .bind(Utc::now().timestamp())
    .execute(&mut connection)
    .await?;
    Ok(())
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
        codex_login::test_support::transport_default_auth_route_config(),
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
        codex_login::test_support::transport_default_auth_route_config(),
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
