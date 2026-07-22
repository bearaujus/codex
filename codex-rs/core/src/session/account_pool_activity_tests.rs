use super::*;
use chrono::Utc;
use codex_config::types::AuthCredentialsStoreMode;
use codex_login::AuthDotJson;
use codex_login::AuthKeyringBackendKind;
use codex_login::ChatgptAccountPool;
use codex_login::account_pool_db_path;
use codex_login::token_data::TokenData;
use codex_protocol::auth::AuthMode;
use sqlx::Connection;
use sqlx::SqliteConnection;
use sqlx::sqlite::SqliteConnectOptions;
use tempfile::tempdir;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

/// A unique account id per test so the process-global refcount map cannot leak
/// state between parallel tests.
fn unique_account(tag: &str) -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("acct-{tag}-{n}")
}

#[test]
fn single_holder_clears_on_release() {
    let account = unique_account("single");
    acquire_activity(&account);
    assert!(
        release_activity(&account),
        "the only holder releasing should clear the DB row"
    );
}

#[test]
fn overlapping_holders_keep_marker_until_last_release() {
    let account = unique_account("overlap");
    acquire_activity(&account);
    acquire_activity(&account);

    assert!(
        !release_activity(&account),
        "the earlier turn tearing down must not clear the marker the later turn still needs"
    );
    assert!(
        release_activity(&account),
        "the last remaining holder releasing should finally clear the marker"
    );
}

#[test]
fn release_without_acquire_defaults_to_clearing() {
    let account = unique_account("untracked");
    assert!(
        release_activity(&account),
        "an untracked account should default to clearing rather than leaking a row"
    );
}

#[tokio::test]
async fn shutdown_clears_activity_for_account_activated_during_failover() {
    let codex_home = tempdir().expect("temp codex home");
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/backend-api/wham/usage"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let pool = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        Some(format!("{}/backend-api", server.uri())),
    )
    .await
    .expect("account pool should open");
    for account_id in ["activity-a", "activity-b"] {
        pool.register_account(&AuthDotJson {
            auth_mode: Some(AuthMode::Chatgpt),
            tokens: Some(TokenData {
                access_token: format!("access-{account_id}"),
                refresh_token: format!("refresh-{account_id}"),
                account_id: Some(account_id.to_string()),
                ..TokenData::default()
            }),
            pool_account_id: Some(account_id.to_string()),
            last_refresh: Some(Utc::now()),
            agent_identity: None,
        })
        .await
        .expect("account should register");
    }
    drop(pool);
    let auth_manager = AuthManager::shared(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        /*forced_chatgpt_workspace_id*/ None,
        Some(format!("{}/backend-api", server.uri())),
        AuthKeyringBackendKind::default(),
        /*auth_route_config*/ None,
    )
    .await;
    let heartbeat =
        AccountPoolActivityHeartbeat::start(Arc::clone(&auth_manager), &CancellationToken::new())
            .await;

    assert!(
        !auth_manager
            .handle_chatgpt_account_pool_usage_limit(
                Some("activity-a"),
                /*safe_to_retry*/ false,
                /*snapshot*/ None,
                Some(Utc::now() + chrono::Duration::hours(1)),
            )
            .await
            .expect("failover should succeed")
    );
    auth_manager.record_account_pool_activity().await;
    assert_eq!(
        auth_manager
            .auth_cached()
            .and_then(|auth| auth.get_pool_account_id())
            .as_deref(),
        Some("activity-b")
    );

    heartbeat.shutdown().await;

    let mut connection = SqliteConnection::connect_with(
        &SqliteConnectOptions::new().filename(account_pool_db_path(codex_home.path())),
    )
    .await
    .expect("activity database should open");
    let active_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM account_activity")
        .fetch_one(&mut connection)
        .await
        .expect("activity rows should be readable");
    assert_eq!(active_rows, 0);
}
