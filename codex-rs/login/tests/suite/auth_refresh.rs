use anyhow::Context;
use anyhow::Result;
use base64::Engine;
use chrono::Duration;
use chrono::Utc;
use codex_config::types::AuthCredentialsStoreMode;
use codex_login::AuthDotJson;
use codex_login::AuthKeyringBackendKind;
use codex_login::AuthManager;
use codex_login::ChatgptAccountPool;
use codex_login::REFRESH_TOKEN_URL_OVERRIDE_ENV_VAR;
use codex_login::RefreshTokenError;
use codex_login::load_auth_dot_json;
use codex_login::save_auth;
use codex_login::token_data::TokenData;
use codex_login::token_data::parse_chatgpt_jwt_claims;
use codex_protocol::auth::AuthMode;
use codex_protocol::auth::RefreshTokenFailedReason;
use core_test_support::skip_if_no_network;
use pretty_assertions::assert_eq;
use serde::Serialize;
use serde_json::json;
use std::ffi::OsString;
use std::sync::Arc;
use tempfile::TempDir;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

const INITIAL_ACCESS_TOKEN: &str = "initial-access-token";
const INITIAL_REFRESH_TOKEN: &str = "initial-refresh-token";

#[serial_test::serial(auth_env)]
#[tokio::test]
async fn refresh_token_succeeds_updates_storage() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "unexpected-access-token",
            "refresh_token": "unexpected-refresh-token"
        })))
        .expect(0)
        .mount(&server)
        .await;

    let ctx = RefreshTokenTestContext::new(&server).await?;
    let initial_last_refresh = Utc::now() - Duration::days(1);
    let initial_tokens = build_tokens(INITIAL_ACCESS_TOKEN, INITIAL_REFRESH_TOKEN);
    let initial_auth = AuthDotJson {
        auth_mode: Some(AuthMode::Chatgpt),
        tokens: Some(initial_tokens.clone()),
        pool_account_id: Some("account-id".to_string()),
        last_refresh: Some(initial_last_refresh),
        agent_identity: None,
    };
    ctx.write_auth(&initial_auth).await?;

    let refreshed_tokens = TokenData {
        access_token: live_access_token(),
        refresh_token: "new-refresh-token".to_string(),
        ..initial_tokens.clone()
    };
    let refreshed_auth = AuthDotJson {
        tokens: Some(refreshed_tokens.clone()),
        last_refresh: Some(initial_last_refresh + Duration::minutes(1)),
        ..initial_auth.clone()
    };
    // Simulate codex-accounts writing a rotated secret into the pool.
    ctx.persist_pool_auth(&refreshed_auth).await?;

    ctx.auth_manager
        .refresh_token_from_authority()
        .await
        .context("refresh should reload pool secret")?;

    let stored = ctx.load_auth().await?;
    let tokens = stored.tokens.as_ref().context("tokens should exist")?;
    assert_eq!(tokens, &refreshed_tokens);
    let refreshed_at = stored
        .last_refresh
        .as_ref()
        .context("last_refresh should be recorded")?;
    assert!(
        *refreshed_at >= initial_last_refresh,
        "last_refresh should advance"
    );

    let cached_auth = ctx
        .auth_manager
        .auth()
        .await
        .context("auth should be cached")?;
    let cached = cached_auth
        .get_token_data()
        .context("token data should be cached")?;
    assert_eq!(cached, refreshed_tokens);

    server.verify().await;
    Ok(())
}

#[serial_test::serial(auth_env)]
#[tokio::test]
async fn refresh_token_refreshes_when_auth_is_unchanged() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "unexpected-access-token",
            "refresh_token": "unexpected-refresh-token"
        })))
        .expect(0)
        .mount(&server)
        .await;

    let ctx = RefreshTokenTestContext::new(&server).await?;
    let initial_last_refresh = Utc::now() - Duration::days(1);
    let initial_tokens = build_tokens(INITIAL_ACCESS_TOKEN, INITIAL_REFRESH_TOKEN);
    let initial_auth = AuthDotJson {
        auth_mode: Some(AuthMode::Chatgpt),
        tokens: Some(initial_tokens.clone()),
        pool_account_id: Some("account-id".to_string()),
        last_refresh: Some(initial_last_refresh),
        agent_identity: None,
    };
    ctx.write_auth(&initial_auth).await?;

    let refreshed_tokens = TokenData {
        access_token: live_access_token(),
        refresh_token: "new-refresh-token".to_string(),
        ..initial_tokens.clone()
    };
    let refreshed_auth = AuthDotJson {
        tokens: Some(refreshed_tokens.clone()),
        last_refresh: Some(initial_last_refresh + Duration::minutes(1)),
        ..initial_auth.clone()
    };
    ctx.persist_pool_auth(&refreshed_auth).await?;

    ctx.auth_manager
        .refresh_token()
        .await
        .context("refresh should reload pool secret")?;

    let stored = ctx.load_auth().await?;
    let tokens = stored.tokens.as_ref().context("tokens should exist")?;
    assert_eq!(tokens, &refreshed_tokens);
    let refreshed_at = stored
        .last_refresh
        .as_ref()
        .context("last_refresh should be recorded")?;
    assert!(
        *refreshed_at >= initial_last_refresh,
        "last_refresh should advance"
    );

    let cached_auth = ctx
        .auth_manager
        .auth()
        .await
        .context("auth should be cached")?;
    let cached = cached_auth
        .get_token_data()
        .context("token data should be cached")?;
    assert_eq!(cached, refreshed_tokens);

    server.verify().await;
    Ok(())
}

#[serial_test::serial(auth_env)]
#[tokio::test]
async fn auth_keeps_pool_managed_access_token_when_it_is_only_near_expiry() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "new-access-token",
            "refresh_token": "new-refresh-token"
        })))
        .mount(&server)
        .await;

    let ctx = RefreshTokenTestContext::new(&server).await?;
    let initial_last_refresh = Utc::now() - Duration::days(1);
    let near_expiry_access_token = access_token_with_expiration(Utc::now() + Duration::minutes(4));
    let initial_tokens = build_tokens(&near_expiry_access_token, INITIAL_REFRESH_TOKEN);
    let initial_auth = AuthDotJson {
        auth_mode: Some(AuthMode::Chatgpt),
        tokens: Some(initial_tokens.clone()),
        pool_account_id: Some("account-id".to_string()),
        last_refresh: Some(initial_last_refresh),
        agent_identity: None,
    };
    ctx.write_auth(&initial_auth).await?;

    let cached_auth = ctx
        .auth_manager
        .auth()
        .await
        .context("auth should be cached")?;
    let cached = cached_auth
        .get_token_data()
        .context("token data should remain cached")?;
    assert_eq!(cached, initial_tokens);
    assert_eq!(
        ctx.load_auth().await?.tokens.as_ref(),
        Some(&initial_tokens)
    );
    let requests = server.received_requests().await.unwrap_or_default();
    assert!(
        requests.is_empty(),
        "expected no refresh token requests for near-expiry pool-managed auth"
    );

    Ok(())
}

#[serial_test::serial(auth_env)]
#[tokio::test]
async fn auth_skips_access_token_outside_refresh_window() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = MockServer::start().await;
    let ctx = RefreshTokenTestContext::new(&server).await?;
    let initial_last_refresh = Utc::now();
    let fresh_access_token = access_token_with_expiration(Utc::now() + Duration::minutes(6));
    let initial_tokens = build_tokens(&fresh_access_token, INITIAL_REFRESH_TOKEN);
    let initial_auth = AuthDotJson {
        auth_mode: Some(AuthMode::Chatgpt),
        tokens: Some(initial_tokens.clone()),
        pool_account_id: Some("account-id".to_string()),
        last_refresh: Some(initial_last_refresh),
        agent_identity: None,
    };
    ctx.write_auth(&initial_auth).await?;

    let cached_auth = ctx
        .auth_manager
        .auth()
        .await
        .context("auth should be cached")?;

    let cached = cached_auth
        .get_token_data()
        .context("token data should remain cached")?;
    assert_eq!(cached, initial_tokens);
    assert_eq!(
        ctx.load_auth().await?.tokens.as_ref(),
        Some(&initial_tokens)
    );
    let requests = server.received_requests().await.unwrap_or_default();
    assert!(requests.is_empty(), "expected no refresh token requests");

    Ok(())
}

#[serial_test::serial(auth_env)]
#[tokio::test]
async fn refresh_token_skips_refresh_when_auth_changed() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = MockServer::start().await;
    let ctx = RefreshTokenTestContext::new(&server).await?;

    let initial_last_refresh = Utc::now() - Duration::days(1);
    let initial_tokens = build_tokens(INITIAL_ACCESS_TOKEN, INITIAL_REFRESH_TOKEN);
    let initial_auth = AuthDotJson {
        auth_mode: Some(AuthMode::Chatgpt),
        tokens: Some(initial_tokens),
        pool_account_id: Some("account-id".to_string()),
        last_refresh: Some(initial_last_refresh),
        agent_identity: None,
    };
    ctx.write_auth(&initial_auth).await?;

    let disk_tokens = TokenData {
        access_token: live_access_token(),
        refresh_token: "disk-refresh-token".to_string(),
        ..build_tokens(INITIAL_ACCESS_TOKEN, INITIAL_REFRESH_TOKEN)
    };
    let disk_auth = AuthDotJson {
        auth_mode: Some(AuthMode::Chatgpt),
        tokens: Some(disk_tokens.clone()),
        pool_account_id: Some("account-id".to_string()),
        last_refresh: Some(initial_last_refresh + Duration::minutes(1)),
        agent_identity: None,
    };
    // Pool is the authority — a newer pool secret should be adopted without OAuth.
    ctx.persist_pool_auth_without_reload(&disk_auth).await?;

    ctx.auth_manager
        .refresh_token()
        .await
        .context("refresh should reload changed pool auth")?;

    let stored = ctx.load_auth().await?;
    assert_eq!(stored.tokens.as_ref(), disk_auth.tokens.as_ref());

    let cached_auth = ctx
        .auth_manager
        .auth_cached()
        .context("auth should be cached")?;
    let cached_tokens = cached_auth
        .get_token_data()
        .context("token data should be cached")?;
    assert_eq!(cached_tokens, disk_tokens);

    let requests = server.received_requests().await.unwrap_or_default();
    assert!(requests.is_empty(), "expected no refresh token requests");

    Ok(())
}

#[serial_test::serial(auth_env)]
#[tokio::test]
async fn refresh_token_reloads_pool_secret_when_local_pool_copy_is_stale() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "unexpected-access-token",
            "refresh_token": "unexpected-refresh-token"
        })))
        .expect(0)
        .mount(&server)
        .await;

    let initial_last_refresh = Utc::now() - Duration::days(1);
    let initial_tokens = build_tokens(INITIAL_ACCESS_TOKEN, INITIAL_REFRESH_TOKEN);
    let initial_auth = AuthDotJson {
        auth_mode: Some(AuthMode::Chatgpt),
        tokens: Some(initial_tokens.clone()),
        pool_account_id: Some("account-id".to_string()),
        last_refresh: Some(initial_last_refresh),
        agent_identity: None,
    };
    let ctx = RefreshTokenTestContext::new_with_initial_auth(&server, &initial_auth).await?;
    let pool = ctx
        .auth_manager
        .chatgpt_account_pool()
        .context("pool should exist")?;

    let reloaded_tokens = TokenData {
        access_token: access_token_with_expiration(Utc::now() + Duration::hours(1)),
        refresh_token: "pool-refresh-token".to_string(),
        ..initial_tokens
    };
    let reloaded_pool_auth = AuthDotJson {
        auth_mode: Some(AuthMode::Chatgpt),
        tokens: Some(reloaded_tokens.clone()),
        pool_account_id: Some("account-id".to_string()),
        last_refresh: Some(initial_last_refresh + Duration::hours(1)),
        agent_identity: None,
    };
    pool.persist_refreshed_account_auth("account-id", &reloaded_pool_auth)
        .await?;

    ctx.auth_manager
        .refresh_token()
        .await
        .context("refresh should reload the shared pool secret")?;

    let cached = ctx
        .auth_manager
        .auth_cached()
        .context("auth should remain cached")?
        .get_token_data()
        .context("cached token data should exist")?;
    assert_eq!(cached, reloaded_tokens);
    assert_eq!(
        ctx.auth_manager
            .auth_cached()
            .and_then(|auth| auth.get_pool_account_id()),
        reloaded_pool_auth.pool_account_id
    );

    server.verify().await;
    Ok(())
}

#[serial_test::serial(auth_refresh)]
#[tokio::test]
async fn refresh_token_skips_oauth_when_pool_db_copy_is_already_fresh() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "new-access-token",
            "refresh_token": "new-refresh-token"
        })))
        .expect(0)
        .mount(&server)
        .await;

    let initial_last_refresh = Utc::now() - Duration::days(1);
    let initial_tokens = build_tokens(INITIAL_ACCESS_TOKEN, INITIAL_REFRESH_TOKEN);
    let initial_auth = AuthDotJson {
        auth_mode: Some(AuthMode::Chatgpt),
        tokens: Some(initial_tokens.clone()),
        pool_account_id: Some("account-id".to_string()),
        last_refresh: Some(initial_last_refresh),
        agent_identity: None,
    };
    let ctx = RefreshTokenTestContext::new_with_initial_auth(&server, &initial_auth).await?;
    let pool = ctx
        .auth_manager
        .chatgpt_account_pool()
        .context("pool should exist")?;

    let pool_auth = AuthDotJson {
        auth_mode: Some(AuthMode::Chatgpt),
        tokens: Some(TokenData {
            access_token: access_token_with_expiration(Utc::now() + Duration::hours(1)),
            refresh_token: "pool-refresh-token".to_string(),
            ..initial_tokens.clone()
        }),
        pool_account_id: Some("account-id".to_string()),
        last_refresh: Some(initial_last_refresh + Duration::minutes(5)),
        agent_identity: None,
    };
    pool.persist_refreshed_account_auth("account-id", &pool_auth)
        .await?;

    ctx.auth_manager
        .refresh_token()
        .await
        .context("refresh should use the shared pool secret")?;

    let requests = server.received_requests().await.unwrap_or_default();
    assert_eq!(
        requests
            .iter()
            .filter(|request| {
                request.method.as_str() == "POST" && request.url.path() == "/oauth/token"
            })
            .count(),
        0
    );

    let cached = ctx
        .auth_manager
        .auth_cached()
        .context("auth should remain cached")?
        .get_token_data()
        .context("cached token data should exist")?;
    assert_eq!(
        cached,
        pool_auth
            .tokens
            .clone()
            .context("pool tokens should exist")?
    );

    server.verify().await;
    Ok(())
}

#[serial_test::serial(auth_refresh)]
#[tokio::test]
async fn refresh_token_ignores_disk_only_account_swap() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "recovered-access-token",
            "refresh_token": "recovered-refresh-token"
        })))
        .expect(0)
        .mount(&server)
        .await;

    let ctx = RefreshTokenTestContext::new(&server).await?;
    let initial_last_refresh = Utc::now() - Duration::days(1);
    let initial_tokens = TokenData {
        access_token: live_access_token(),
        refresh_token: INITIAL_REFRESH_TOKEN.to_string(),
        ..build_tokens(INITIAL_ACCESS_TOKEN, INITIAL_REFRESH_TOKEN)
    };
    let initial_auth = AuthDotJson {
        auth_mode: Some(AuthMode::Chatgpt),
        tokens: Some(initial_tokens.clone()),
        pool_account_id: Some("account-id".to_string()),
        last_refresh: Some(initial_last_refresh),
        agent_identity: None,
    };
    ctx.write_auth(&initial_auth).await?;

    let mut disk_tokens = build_tokens("disk-access-token", "disk-refresh-token");
    disk_tokens.account_id = Some("other-account".to_string());
    let disk_auth = AuthDotJson {
        auth_mode: Some(AuthMode::Chatgpt),
        tokens: Some(disk_tokens),
        pool_account_id: Some("other-account".to_string()),
        last_refresh: Some(initial_last_refresh),
        agent_identity: None,
    };
    // Disk-only swap must not override pool-managed credentials.
    save_auth(
        ctx.codex_home.path(),
        &disk_auth,
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
    )?;

    ctx.auth_manager
        .refresh_token()
        .await
        .context("refresh should keep the pool-managed account")?;

    let stored = ctx.load_auth().await?;
    assert_eq!(stored.tokens.as_ref(), Some(&initial_tokens));
    assert_eq!(stored.pool_account_id.as_deref(), Some("account-id"));

    let requests = server.received_requests().await.unwrap_or_default();
    assert!(requests.is_empty(), "expected no refresh token requests");

    let cached_after = ctx
        .auth_manager
        .auth_cached()
        .context("auth should be cached after refresh")?;
    let cached_after_tokens = cached_after
        .get_token_data()
        .context("token data should remain cached")?;
    assert_eq!(cached_after_tokens, initial_tokens);

    server.verify().await;
    Ok(())
}

#[serial_test::serial(auth_env)]
#[tokio::test]
async fn returns_fresh_tokens_as_is() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "new-access-token",
            "refresh_token": "new-refresh-token"
        })))
        .mount(&server)
        .await;

    let ctx = RefreshTokenTestContext::new(&server).await?;
    let stale_refresh = Utc::now() - Duration::days(9);
    let fresh_access_token = access_token_with_expiration(Utc::now() + Duration::hours(1));
    let initial_tokens = build_tokens(&fresh_access_token, INITIAL_REFRESH_TOKEN);
    let initial_auth = AuthDotJson {
        auth_mode: Some(AuthMode::Chatgpt),
        tokens: Some(initial_tokens.clone()),
        pool_account_id: Some("account-id".to_string()),
        last_refresh: Some(stale_refresh),
        agent_identity: None,
    };
    ctx.write_auth(&initial_auth).await?;

    let cached_auth = ctx
        .auth_manager
        .auth()
        .await
        .context("auth should be cached")?;
    let cached = cached_auth
        .get_token_data()
        .context("token data should remain cached")?;
    assert_eq!(cached, initial_tokens);

    let stored = ctx.load_auth().await?;
    assert_eq!(stored.tokens.as_ref(), Some(&initial_tokens));

    let requests = server.received_requests().await.unwrap_or_default();
    assert!(requests.is_empty(), "expected no refresh token requests");

    Ok(())
}

#[serial_test::serial(auth_env)]
#[tokio::test]
async fn refreshes_token_when_access_token_is_expired() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "unexpected-access-token",
            "refresh_token": "unexpected-refresh-token"
        })))
        .expect(0)
        .mount(&server)
        .await;

    let ctx = RefreshTokenTestContext::new(&server).await?;
    let fresh_refresh = Utc::now() - Duration::days(1);
    let expired_access_token = access_token_with_expiration(Utc::now() - Duration::hours(1));
    let initial_tokens = build_tokens(&expired_access_token, INITIAL_REFRESH_TOKEN);
    let initial_auth = AuthDotJson {
        auth_mode: Some(AuthMode::Chatgpt),
        tokens: Some(initial_tokens.clone()),
        pool_account_id: Some("account-id".to_string()),
        last_refresh: Some(fresh_refresh),
        agent_identity: None,
    };
    ctx.write_auth(&initial_auth).await?;

    let refreshed_tokens = TokenData {
        access_token: access_token_with_expiration(Utc::now() + Duration::hours(1)),
        refresh_token: "new-refresh-token".to_string(),
        ..initial_tokens.clone()
    };
    let refreshed_auth = AuthDotJson {
        tokens: Some(refreshed_tokens.clone()),
        last_refresh: Some(fresh_refresh + Duration::minutes(1)),
        ..initial_auth.clone()
    };
    // codex-accounts refreshed the pool secret; auth() should pick it up without OAuth.
    ctx.persist_pool_auth(&refreshed_auth).await?;

    let cached_auth = ctx
        .auth_manager
        .auth()
        .await
        .context("auth should be cached")?;
    let cached = cached_auth
        .get_token_data()
        .context("token data should refresh from pool")?;
    assert_eq!(cached, refreshed_tokens);

    let stored = ctx.load_auth().await?;
    let tokens = stored.tokens.as_ref().context("tokens should exist")?;
    assert_eq!(tokens, &refreshed_tokens);
    let refreshed_at = stored
        .last_refresh
        .as_ref()
        .context("last_refresh should be recorded")?;
    assert!(
        *refreshed_at >= fresh_refresh,
        "last_refresh should advance"
    );

    server.verify().await;
    Ok(())
}

#[serial_test::serial(auth_env)]
#[tokio::test]
async fn auth_reloads_disk_auth_when_cached_auth_is_stale() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = MockServer::start().await;

    let ctx = RefreshTokenTestContext::new(&server).await?;
    let stale_refresh = Utc::now() - Duration::days(9);
    let initial_tokens = build_tokens(INITIAL_ACCESS_TOKEN, INITIAL_REFRESH_TOKEN);
    let initial_auth = AuthDotJson {
        auth_mode: Some(AuthMode::Chatgpt),
        tokens: Some(initial_tokens),
        pool_account_id: Some("account-id".to_string()),
        last_refresh: Some(stale_refresh),
        agent_identity: None,
    };
    ctx.write_auth(&initial_auth).await?;

    let fresh_refresh = Utc::now() - Duration::days(1);
    let disk_tokens = build_tokens("disk-access-token", "disk-refresh-token");
    let disk_auth = AuthDotJson {
        auth_mode: Some(AuthMode::Chatgpt),
        tokens: Some(disk_tokens.clone()),
        pool_account_id: Some("account-id".to_string()),
        last_refresh: Some(fresh_refresh),
        agent_identity: None,
    };
    // Pool is the source of truth; auth() reloads from the pool DB copy.
    ctx.persist_pool_auth(&disk_auth).await?;

    let cached_auth = ctx
        .auth_manager
        .auth()
        .await
        .context("auth should reload from pool")?;
    let cached = cached_auth
        .get_token_data()
        .context("token data should reload from pool")?;
    assert_eq!(cached, disk_tokens);

    let stored = ctx.load_auth().await?;
    assert_eq!(stored.tokens.as_ref(), disk_auth.tokens.as_ref());

    let requests = server.received_requests().await.unwrap_or_default();
    assert!(requests.is_empty(), "expected no refresh token requests");

    Ok(())
}

#[serial_test::serial(auth_env)]
#[tokio::test]
async fn auth_reloads_disk_auth_without_calling_expired_refresh_token() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "error": {
                "code": "refresh_token_expired"
            }
        })))
        .expect(0)
        .mount(&server)
        .await;

    let ctx = RefreshTokenTestContext::new(&server).await?;
    let stale_refresh = Utc::now() - Duration::days(9);
    let initial_tokens = build_tokens(INITIAL_ACCESS_TOKEN, INITIAL_REFRESH_TOKEN);
    let initial_auth = AuthDotJson {
        auth_mode: Some(AuthMode::Chatgpt),
        tokens: Some(initial_tokens),
        pool_account_id: Some("account-id".to_string()),
        last_refresh: Some(stale_refresh),
        agent_identity: None,
    };
    ctx.write_auth(&initial_auth).await?;

    let fresh_refresh = Utc::now() - Duration::days(1);
    let disk_tokens = build_tokens("disk-access-token", "disk-refresh-token");
    let disk_auth = AuthDotJson {
        auth_mode: Some(AuthMode::Chatgpt),
        tokens: Some(disk_tokens.clone()),
        pool_account_id: Some("account-id".to_string()),
        last_refresh: Some(fresh_refresh),
        agent_identity: None,
    };
    ctx.persist_pool_auth(&disk_auth).await?;

    let cached_auth = ctx
        .auth_manager
        .auth()
        .await
        .context("auth should reload from pool")?;
    let cached = cached_auth
        .get_token_data()
        .context("token data should reload from pool")?;
    assert_eq!(cached, disk_tokens);

    let stored = ctx.load_auth().await?;
    assert_eq!(stored.tokens.as_ref(), disk_auth.tokens.as_ref());

    server.verify().await;
    Ok(())
}

#[serial_test::serial(auth_env)]
#[tokio::test]
async fn refresh_token_returns_permanent_error_for_expired_refresh_token() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "error": {
                "code": "refresh_token_expired"
            }
        })))
        .expect(0)
        .mount(&server)
        .await;

    let ctx = RefreshTokenTestContext::new(&server).await?;
    let initial_last_refresh = Utc::now() - Duration::days(1);
    let initial_tokens = build_tokens(INITIAL_ACCESS_TOKEN, INITIAL_REFRESH_TOKEN);
    let initial_auth = AuthDotJson {
        auth_mode: Some(AuthMode::Chatgpt),
        tokens: Some(initial_tokens.clone()),
        pool_account_id: Some("account-id".to_string()),
        last_refresh: Some(initial_last_refresh),
        agent_identity: None,
    };
    ctx.write_auth(&initial_auth).await?;
    ctx.auth_manager
        .chatgpt_account_pool()
        .context("pool should exist")?
        .mark_account_auth_failed("account-id", "refresh_token_expired")
        .await?;

    let err = ctx
        .auth_manager
        .refresh_token_from_authority()
        .await
        .err()
        .context("refresh should fail")?;
    assert!(matches!(err, RefreshTokenError::Permanent(_)));
    assert_eq!(err.failed_reason(), Some(RefreshTokenFailedReason::Other));

    let stored = ctx.load_auth().await?;
    assert_eq!(stored.tokens.as_ref(), Some(&initial_tokens));
    let cached_auth = ctx
        .auth_manager
        .auth_cached()
        .context("auth should remain cached")?;
    let cached = cached_auth
        .get_token_data()
        .context("token data should remain cached")?;
    assert_eq!(cached, initial_tokens);

    server.verify().await;
    Ok(())
}

#[serial_test::serial(auth_env)]
#[tokio::test]
async fn refresh_token_does_not_retry_after_permanent_failure() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "error": {
                "code": "refresh_token_reused"
            }
        })))
        .expect(0)
        .mount(&server)
        .await;

    let ctx = RefreshTokenTestContext::new(&server).await?;
    let initial_last_refresh = Utc::now() - Duration::days(1);
    let initial_tokens = build_tokens(INITIAL_ACCESS_TOKEN, INITIAL_REFRESH_TOKEN);
    let initial_auth = AuthDotJson {
        auth_mode: Some(AuthMode::Chatgpt),
        tokens: Some(initial_tokens.clone()),
        pool_account_id: Some("account-id".to_string()),
        last_refresh: Some(initial_last_refresh),
        agent_identity: None,
    };
    ctx.write_auth(&initial_auth).await?;
    ctx.auth_manager
        .chatgpt_account_pool()
        .context("pool should exist")?
        .mark_account_auth_failed("account-id", "refresh_token_reused")
        .await?;

    let first_err = ctx
        .auth_manager
        .refresh_token()
        .await
        .err()
        .context("first refresh should fail")?;
    assert!(matches!(first_err, RefreshTokenError::Permanent(_)));
    assert_eq!(
        first_err.failed_reason(),
        Some(RefreshTokenFailedReason::Other)
    );

    let second_err = ctx
        .auth_manager
        .refresh_token()
        .await
        .err()
        .context("second refresh should fail without retrying")?;
    assert!(matches!(second_err, RefreshTokenError::Permanent(_)));
    assert_eq!(
        second_err.failed_reason(),
        Some(RefreshTokenFailedReason::Other)
    );

    let stored = ctx.load_auth().await?;
    assert_eq!(stored.tokens.as_ref(), Some(&initial_tokens));
    let cached_auth = ctx
        .auth_manager
        .auth()
        .await
        .context("auth should remain cached")?;
    let cached = cached_auth
        .get_token_data()
        .context("token data should remain cached")?;
    assert_eq!(cached, initial_tokens);

    server.verify().await;
    Ok(())
}

#[serial_test::serial(auth_env)]
#[tokio::test]
async fn refresh_token_does_not_retry_after_bad_request_reused_failure() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": {
                "code": "refresh_token_reused"
            }
        })))
        .expect(0)
        .mount(&server)
        .await;

    let ctx = RefreshTokenTestContext::new(&server).await?;
    let initial_last_refresh = Utc::now() - Duration::days(1);
    let initial_tokens = build_tokens(INITIAL_ACCESS_TOKEN, INITIAL_REFRESH_TOKEN);
    let initial_auth = AuthDotJson {
        auth_mode: Some(AuthMode::Chatgpt),
        tokens: Some(initial_tokens.clone()),
        pool_account_id: Some("account-id".to_string()),
        last_refresh: Some(initial_last_refresh),
        agent_identity: None,
    };
    ctx.write_auth(&initial_auth).await?;
    ctx.auth_manager
        .chatgpt_account_pool()
        .context("pool should exist")?
        .mark_account_auth_failed("account-id", "refresh_token_reused")
        .await?;

    let first_err = ctx
        .auth_manager
        .refresh_token()
        .await
        .err()
        .context("first refresh should fail")?;
    assert_eq!(
        first_err.failed_reason(),
        Some(RefreshTokenFailedReason::Other)
    );

    let second_err = ctx
        .auth_manager
        .refresh_token()
        .await
        .err()
        .context("second refresh should fail without retrying")?;
    assert_eq!(
        second_err.failed_reason(),
        Some(RefreshTokenFailedReason::Other)
    );

    let stored = ctx.load_auth().await?;
    assert_eq!(stored.tokens.as_ref(), Some(&initial_tokens));
    let cached_auth = ctx
        .auth_manager
        .auth()
        .await
        .context("auth should remain cached")?;
    let cached = cached_auth
        .get_token_data()
        .context("token data should remain cached")?;
    assert_eq!(cached, initial_tokens);

    server.verify().await;
    Ok(())
}

#[serial_test::serial(auth_env)]
#[tokio::test]
async fn refresh_token_reloads_changed_auth_after_permanent_failure() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "error": {
                "code": "refresh_token_reused"
            }
        })))
        .expect(0)
        .mount(&server)
        .await;

    let ctx = RefreshTokenTestContext::new(&server).await?;
    let initial_last_refresh = Utc::now() - Duration::days(1);
    let initial_tokens = build_tokens(INITIAL_ACCESS_TOKEN, INITIAL_REFRESH_TOKEN);
    let initial_auth = AuthDotJson {
        auth_mode: Some(AuthMode::Chatgpt),
        tokens: Some(initial_tokens.clone()),
        pool_account_id: Some("account-id".to_string()),
        last_refresh: Some(initial_last_refresh),
        agent_identity: None,
    };
    ctx.write_auth(&initial_auth).await?;
    ctx.auth_manager
        .chatgpt_account_pool()
        .context("pool should exist")?
        .mark_account_auth_failed("account-id", "refresh_token_reused")
        .await?;

    let first_err = ctx
        .auth_manager
        .refresh_token()
        .await
        .err()
        .context("first refresh should fail")?;
    assert_eq!(
        first_err.failed_reason(),
        Some(RefreshTokenFailedReason::Other)
    );

    let fresh_refresh = Utc::now() - Duration::hours(1);
    let disk_tokens = TokenData {
        access_token: live_access_token(),
        refresh_token: "disk-refresh-token".to_string(),
        ..build_tokens(INITIAL_ACCESS_TOKEN, INITIAL_REFRESH_TOKEN)
    };
    let disk_auth = AuthDotJson {
        auth_mode: Some(AuthMode::Chatgpt),
        tokens: Some(disk_tokens.clone()),
        pool_account_id: Some("account-id".to_string()),
        last_refresh: Some(fresh_refresh),
        agent_identity: None,
    };
    ctx.persist_pool_auth(&disk_auth).await?;

    ctx.auth_manager
        .refresh_token()
        .await
        .context("refresh should reload changed pool auth without retrying")?;

    let stored = ctx.load_auth().await?;
    assert_eq!(stored.tokens.as_ref(), disk_auth.tokens.as_ref());

    let cached_auth = ctx
        .auth_manager
        .auth_cached()
        .context("auth should be cached")?;
    let cached = cached_auth
        .get_token_data()
        .context("token data should reload from pool")?;
    assert_eq!(cached, disk_tokens);

    let requests = server.received_requests().await.unwrap_or_default();
    assert!(requests.is_empty(), "expected no oauth refresh attempts");

    server.verify().await;
    Ok(())
}

#[serial_test::serial(auth_env)]
#[tokio::test]
async fn refresh_token_returns_transient_error_on_server_failure() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({
            "error": "temporary-failure"
        })))
        .expect(0)
        .mount(&server)
        .await;

    let ctx = RefreshTokenTestContext::new(&server).await?;
    let initial_last_refresh = Utc::now() - Duration::days(1);
    let expired_access_token = access_token_with_expiration(Utc::now() - Duration::hours(1));
    let initial_tokens = build_tokens(&expired_access_token, INITIAL_REFRESH_TOKEN);
    let initial_auth = AuthDotJson {
        auth_mode: Some(AuthMode::Chatgpt),
        tokens: Some(initial_tokens.clone()),
        pool_account_id: Some("account-id".to_string()),
        last_refresh: Some(initial_last_refresh),
        agent_identity: None,
    };
    ctx.write_auth(&initial_auth).await?;

    // Forced refresh via unauthorized recovery with non-terminal pool status
    // fails over immediately (no OAuth; no 90s wait).
    let mut recovery = ctx.auth_manager.unauthorized_recovery();
    recovery
        .next()
        .await
        .context("reload step should succeed")?;
    let err = recovery
        .next()
        .await
        .err()
        .context("forced refresh should fail transiently")?;
    assert!(matches!(err, RefreshTokenError::Transient(_)));
    assert_eq!(err.failed_reason(), None);

    let stored = ctx.load_auth().await?;
    assert_eq!(stored.tokens.as_ref(), Some(&initial_tokens));
    let cached_auth = ctx
        .auth_manager
        .auth()
        .await
        .context("auth should remain cached")?;
    let cached = cached_auth
        .get_token_data()
        .context("token data should remain cached")?;
    assert_eq!(cached, initial_tokens);
    let requests = server.received_requests().await.unwrap_or_default();
    assert!(requests.is_empty(), "expected no oauth refresh attempts");

    server.verify().await;
    Ok(())
}

#[serial_test::serial(auth_env)]
#[tokio::test]
async fn unauthorized_recovery_reloads_then_refreshes_tokens() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "unexpected-access-token",
            "refresh_token": "unexpected-refresh-token"
        })))
        .expect(0)
        .mount(&server)
        .await;

    let ctx = RefreshTokenTestContext::new(&server).await?;
    let initial_last_refresh = Utc::now() - Duration::days(1);
    let initial_tokens = build_tokens(INITIAL_ACCESS_TOKEN, INITIAL_REFRESH_TOKEN);
    let initial_auth = AuthDotJson {
        auth_mode: Some(AuthMode::Chatgpt),
        tokens: Some(initial_tokens.clone()),
        pool_account_id: Some("account-id".to_string()),
        last_refresh: Some(initial_last_refresh),
        agent_identity: None,
    };
    ctx.write_auth(&initial_auth).await?;

    let disk_tokens = TokenData {
        access_token: live_access_token(),
        refresh_token: "disk-refresh-token".to_string(),
        ..build_tokens(INITIAL_ACCESS_TOKEN, INITIAL_REFRESH_TOKEN)
    };
    let disk_auth = AuthDotJson {
        auth_mode: Some(AuthMode::Chatgpt),
        tokens: Some(disk_tokens.clone()),
        pool_account_id: Some("account-id".to_string()),
        last_refresh: Some(initial_last_refresh),
        agent_identity: None,
    };
    ctx.persist_pool_auth_without_reload(&disk_auth).await?;

    let cached_before = ctx
        .auth_manager
        .auth_cached()
        .expect("auth should be cached");
    let cached_before_tokens = cached_before
        .get_token_data()
        .context("token data should be cached")?;
    assert_eq!(cached_before_tokens, initial_tokens);

    let mut recovery = ctx.auth_manager.unauthorized_recovery();
    assert!(recovery.has_next());

    recovery.next().await?;

    let cached_after = ctx
        .auth_manager
        .auth_cached()
        .expect("auth should be cached after reload");
    let cached_after_tokens = cached_after
        .get_token_data()
        .context("token data should reload")?;
    assert_eq!(cached_after_tokens, disk_tokens);

    let requests = server.received_requests().await.unwrap_or_default();
    assert!(requests.is_empty(), "expected no refresh token requests");

    let refreshed_tokens = TokenData {
        access_token: live_access_token(),
        refresh_token: "recovered-refresh-token".to_string(),
        ..disk_tokens.clone()
    };
    let refreshed_auth = AuthDotJson {
        tokens: Some(refreshed_tokens.clone()),
        last_refresh: Some(initial_last_refresh + Duration::minutes(1)),
        ..disk_auth.clone()
    };
    // Simulate codex-accounts rotating the secret before the forced refresh step.
    ctx.persist_pool_auth_without_reload(&refreshed_auth)
        .await?;

    recovery.next().await?;

    let stored = ctx.load_auth().await?;
    let tokens = stored.tokens.as_ref().context("tokens should exist")?;
    assert_eq!(tokens, &refreshed_tokens);

    let cached_auth = ctx
        .auth_manager
        .auth()
        .await
        .expect("auth should be cached");
    let cached_tokens = cached_auth
        .get_token_data()
        .context("token data should be cached")?;
    assert_eq!(cached_tokens, refreshed_tokens);
    assert!(!recovery.has_next());

    server.verify().await;
    Ok(())
}

#[serial_test::serial(auth_env)]
#[tokio::test]
async fn unauthorized_recovery_ignores_disk_only_account_swap() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "recovered-access-token",
            "refresh_token": "recovered-refresh-token"
        })))
        .expect(0)
        .mount(&server)
        .await;

    let ctx = RefreshTokenTestContext::new(&server).await?;
    let initial_last_refresh = Utc::now() - Duration::days(1);
    let initial_tokens = TokenData {
        access_token: live_access_token(),
        refresh_token: INITIAL_REFRESH_TOKEN.to_string(),
        ..build_tokens(INITIAL_ACCESS_TOKEN, INITIAL_REFRESH_TOKEN)
    };
    let initial_auth = AuthDotJson {
        auth_mode: Some(AuthMode::Chatgpt),
        tokens: Some(initial_tokens.clone()),
        pool_account_id: Some("account-id".to_string()),
        last_refresh: Some(initial_last_refresh),
        agent_identity: None,
    };
    ctx.write_auth(&initial_auth).await?;

    let mut disk_tokens = build_tokens("disk-access-token", "disk-refresh-token");
    disk_tokens.account_id = Some("other-account".to_string());
    let disk_auth = AuthDotJson {
        auth_mode: Some(AuthMode::Chatgpt),
        tokens: Some(disk_tokens),
        pool_account_id: Some("other-account".to_string()),
        last_refresh: Some(initial_last_refresh),
        agent_identity: None,
    };
    save_auth(
        ctx.codex_home.path(),
        &disk_auth,
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
    )?;

    let cached_before = ctx
        .auth_manager
        .auth_cached()
        .expect("auth should be cached");
    let cached_before_tokens = cached_before
        .get_token_data()
        .context("token data should be cached")?;
    assert_eq!(cached_before_tokens, initial_tokens);

    let mut recovery = ctx.auth_manager.unauthorized_recovery();
    assert!(recovery.has_next());

    recovery
        .next()
        .await
        .context("reload should keep the pool-managed account")?;

    let stored = ctx.load_auth().await?;
    assert_eq!(stored.tokens.as_ref(), Some(&initial_tokens));
    assert_eq!(stored.pool_account_id.as_deref(), Some("account-id"));

    let requests = server.received_requests().await.unwrap_or_default();
    assert!(requests.is_empty(), "expected no refresh token requests");

    let cached_after = ctx
        .auth_manager
        .auth_cached()
        .context("auth should remain cached after reload")?;
    let cached_after_tokens = cached_after
        .get_token_data()
        .context("token data should remain cached")?;
    assert_eq!(cached_after_tokens, initial_tokens);

    server.verify().await;
    Ok(())
}

#[serial_test::serial(auth_env)]
#[tokio::test]
async fn unauthorized_recovery_requires_chatgpt_auth() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = MockServer::start().await;
    let ctx = RefreshTokenTestContext::new(&server).await?;
    let auth = AuthDotJson {
        auth_mode: Some(AuthMode::Headers),
        tokens: None,
        pool_account_id: None,
        last_refresh: None,
        agent_identity: None,
    };
    ctx.write_auth(&auth).await?;

    let mut recovery = ctx.auth_manager.unauthorized_recovery();
    assert!(!recovery.has_next());

    let err = recovery
        .next()
        .await
        .err()
        .context("recovery should fail")?;
    assert_eq!(err.failed_reason(), Some(RefreshTokenFailedReason::Other));

    let requests = server.received_requests().await.unwrap_or_default();
    assert!(requests.is_empty(), "expected no refresh token requests");

    Ok(())
}

struct RefreshTokenTestContext {
    codex_home: TempDir,
    auth_manager: Arc<AuthManager>,
    _env_guard: EnvGuard,
}

impl RefreshTokenTestContext {
    async fn new(server: &MockServer) -> Result<Self> {
        let codex_home = TempDir::new()?;

        let endpoint = format!("{}/oauth/token", server.uri());
        let env_guard = EnvGuard::set(REFRESH_TOKEN_URL_OVERRIDE_ENV_VAR, endpoint);

        let auth_manager = AuthManager::shared(
            codex_home.path().to_path_buf(),
            AuthCredentialsStoreMode::File,
            /*forced_chatgpt_workspace_id*/ None,
            Some(format!("{}/backend-api", server.uri())),
            AuthKeyringBackendKind::default(),
            /*auth_route_config*/ None,
        )
        .await;

        Ok(Self {
            codex_home,
            auth_manager,
            _env_guard: env_guard,
        })
    }

    async fn new_with_initial_auth(
        server: &MockServer,
        auth_dot_json: &AuthDotJson,
    ) -> Result<Self> {
        let codex_home = TempDir::new()?;
        save_auth(
            codex_home.path(),
            auth_dot_json,
            AuthCredentialsStoreMode::File,
            AuthKeyringBackendKind::default(),
        )?;

        let endpoint = format!("{}/oauth/token", server.uri());
        let env_guard = EnvGuard::set(REFRESH_TOKEN_URL_OVERRIDE_ENV_VAR, endpoint);

        let auth_manager = AuthManager::shared(
            codex_home.path().to_path_buf(),
            AuthCredentialsStoreMode::File,
            /*forced_chatgpt_workspace_id*/ None,
            Some(format!("{}/backend-api", server.uri())),
            AuthKeyringBackendKind::default(),
            /*auth_route_config*/ None,
        )
        .await;

        Ok(Self {
            codex_home,
            auth_manager,
            _env_guard: env_guard,
        })
    }

    async fn load_auth(&self) -> Result<AuthDotJson> {
        if let Some(account_id) = self
            .auth_manager
            .auth_cached()
            .and_then(|auth| auth.get_pool_account_id())
            && let Some(account_pool) = self.auth_manager.chatgpt_account_pool()
            && let Some((selected_account_id, pool_auth)) = account_pool
                .selected_account_auth()
                .await
                .context("load selected pool auth")?
            && selected_account_id == account_id
        {
            return Ok(pool_auth);
        }
        load_auth_dot_json(
            self.codex_home.path(),
            AuthCredentialsStoreMode::File,
            AuthKeyringBackendKind::default(),
        )
        .context("load auth.json")?
        .context("auth.json should exist")
    }

    async fn write_auth(&self, auth_dot_json: &AuthDotJson) -> Result<()> {
        save_auth(
            self.codex_home.path(),
            auth_dot_json,
            AuthCredentialsStoreMode::File,
            AuthKeyringBackendKind::default(),
        )?;
        // AuthManager opens the pool once at construction; re-open to run
        // migrate_legacy_auth_if_needed for auth written after startup, then
        // persist so the manager's pool connection sees the latest tokens.
        if auth_dot_json.auth_mode == Some(AuthMode::Chatgpt)
            && let Some(account_id) = auth_dot_json.pool_account_id.as_deref()
        {
            let _ = ChatgptAccountPool::open(
                self.codex_home.path().to_path_buf(),
                AuthCredentialsStoreMode::File,
                /*chatgpt_base_url*/ None,
            )
            .await
            .context("ensure chatgpt account pool row exists")?;
            let pool = self
                .auth_manager
                .chatgpt_account_pool()
                .context("account pool should exist")?;
            pool.persist_refreshed_account_auth(account_id, auth_dot_json)
                .await
                .context("persist auth into account pool")?;
        }
        self.auth_manager.reload().await;
        Ok(())
    }

    async fn persist_pool_auth(&self, auth_dot_json: &AuthDotJson) -> Result<()> {
        self.persist_pool_auth_without_reload(auth_dot_json).await?;
        self.auth_manager.reload().await;
        Ok(())
    }

    async fn persist_pool_auth_without_reload(&self, auth_dot_json: &AuthDotJson) -> Result<()> {
        let account_id = auth_dot_json
            .pool_account_id
            .as_deref()
            .context("pool_account_id required")?;
        let pool = self
            .auth_manager
            .chatgpt_account_pool()
            .context("account pool should exist")?;
        pool.persist_refreshed_account_auth(account_id, auth_dot_json)
            .await
            .context("persist auth into account pool")?;
        Ok(())
    }
}

struct EnvGuard {
    key: &'static str,
    original: Option<OsString>,
}

impl EnvGuard {
    fn set(key: &'static str, value: String) -> Self {
        let original = std::env::var_os(key);
        // SAFETY: these tests execute serially, so updating the process environment is safe.
        unsafe {
            std::env::set_var(key, &value);
        }
        Self { key, original }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: the guard restores the original environment value before other tests run.
        unsafe {
            match &self.original {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }
}

fn jwt_with_payload(payload: serde_json::Value) -> String {
    #[derive(Serialize)]
    struct Header {
        alg: &'static str,
        typ: &'static str,
    }

    let header = Header {
        alg: "none",
        typ: "JWT",
    };

    fn b64(data: &[u8]) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
    }

    let header_bytes = serde_json::to_vec(&header).expect("header should serialize");
    let payload_bytes = serde_json::to_vec(&payload).expect("payload should serialize");
    let header_b64 = b64(&header_bytes);
    let payload_b64 = b64(&payload_bytes);
    let signature_b64 = b64(b"sig");
    format!("{header_b64}.{payload_b64}.{signature_b64}")
}

fn minimal_jwt() -> String {
    jwt_with_payload(json!({ "sub": "user-123" }))
}

fn live_access_token() -> String {
    access_token_with_expiration(Utc::now() + Duration::hours(2))
}

fn access_token_with_expiration(expires_at: chrono::DateTime<Utc>) -> String {
    jwt_with_payload(json!({ "sub": "user-123", "exp": expires_at.timestamp() }))
}

fn build_tokens(access_token: &str, refresh_token: &str) -> TokenData {
    let id_token = match parse_chatgpt_jwt_claims(&minimal_jwt()) {
        Ok(id_token) => id_token,
        Err(err) => panic!("minimal JWT should parse: {err}"),
    };
    TokenData {
        id_token,
        access_token: access_token.to_string(),
        refresh_token: refresh_token.to_string(),
        account_id: Some("account-id".to_string()),
    }
}
