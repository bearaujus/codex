use super::*;
use chrono::Utc;
use codex_config::types::AuthCredentialsStoreMode;
use codex_login::AuthDotJson;
use codex_login::AuthKeyringBackendKind;
use codex_login::ChatgptAccountPool;
use codex_login::account_pool_db_path;
use codex_login::token_data::TokenData;
use codex_protocol::auth::AuthMode;
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
        codex_login::test_support::transport_default_auth_route_config(),
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
    heartbeat.switch_to_current_account().await;
    assert_eq!(
        auth_manager
            .auth_cached()
            .and_then(|auth| auth.get_pool_account_id())
            .as_deref(),
        Some("activity-b")
    );

    heartbeat.shutdown().await;

    let mut connection =
        codex_state::open_existing_sqlite_connection(&account_pool_db_path(codex_home.path()))
            .await
            .expect("activity database should open");
    let active_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM account_activity")
        .fetch_one(&mut connection)
        .await
        .expect("activity rows should be readable");
    assert_eq!(active_rows, 0);
}

#[tokio::test]
async fn failover_in_one_turn_preserves_an_overlapping_turns_original_lease() {
    let codex_home = tempdir().expect("temp codex home");
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/backend-api/wham/usage"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let account_a = unique_account("overlap-a");
    let account_b = unique_account("overlap-b");
    let pool = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        Some(format!("{}/backend-api", server.uri())),
    )
    .await
    .expect("account pool should open");
    for account_id in [&account_a, &account_b] {
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
        codex_login::test_support::transport_default_auth_route_config(),
    )
    .await;
    let first_turn =
        AccountPoolActivityHeartbeat::start(Arc::clone(&auth_manager), &CancellationToken::new())
            .await;

    assert!(
        !auth_manager
            .handle_chatgpt_account_pool_usage_limit(
                Some(&account_a),
                /*safe_to_retry*/ false,
                /*snapshot*/ None,
                Some(Utc::now() + chrono::Duration::hours(1)),
            )
            .await
            .expect("failover should succeed")
    );
    let second_turn =
        AccountPoolActivityHeartbeat::start(Arc::clone(&auth_manager), &CancellationToken::new())
            .await;

    let mut connection =
        codex_state::open_existing_sqlite_connection(&account_pool_db_path(codex_home.path()))
            .await
            .expect("activity database should open");
    let leased_accounts: Vec<String> =
        sqlx::query_scalar("SELECT account_id FROM account_activity ORDER BY account_id")
            .fetch_all(&mut connection)
            .await
            .expect("activity rows should be readable");
    let mut expected = vec![account_a.clone(), account_b.clone()];
    expected.sort();
    assert_eq!(
        leased_accounts, expected,
        "the newer turn must not erase the still-running older turn's lease"
    );

    second_turn.shutdown().await;
    let remaining_accounts: Vec<String> =
        sqlx::query_scalar("SELECT account_id FROM account_activity ORDER BY account_id")
            .fetch_all(&mut connection)
            .await
            .expect("remaining activity should be readable");
    assert_eq!(remaining_accounts, vec![account_a]);

    first_turn.shutdown().await;
    let active_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM account_activity")
        .fetch_one(&mut connection)
        .await
        .expect("final activity count should load");
    assert_eq!(active_rows, 0);
}

#[tokio::test]
async fn concurrent_lease_handoff_never_clears_the_new_holder() {
    let codex_home = tempdir().expect("temp codex home");
    let account_id = unique_account("handoff");
    let pool = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        /*chatgpt_base_url*/ None,
    )
    .await
    .expect("account pool should open");
    pool.register_account(&AuthDotJson {
        auth_mode: Some(AuthMode::Chatgpt),
        tokens: Some(TokenData {
            access_token: "access-handoff".to_string(),
            refresh_token: "refresh-handoff".to_string(),
            account_id: Some(account_id.clone()),
            ..TokenData::default()
        }),
        pool_account_id: Some(account_id.clone()),
        last_refresh: Some(Utc::now()),
        agent_identity: None,
    })
    .await
    .expect("account should register");
    drop(pool);

    let auth_manager = AuthManager::shared(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        /*forced_chatgpt_workspace_id*/ None,
        /*chatgpt_base_url*/ None,
        AuthKeyringBackendKind::default(),
        codex_login::test_support::transport_default_auth_route_config(),
    )
    .await;
    let mut heartbeat =
        AccountPoolActivityHeartbeat::start(Arc::clone(&auth_manager), &CancellationToken::new())
            .await;
    let mut connection =
        codex_state::open_existing_sqlite_connection(&account_pool_db_path(codex_home.path()))
            .await
            .expect("activity database should open");

    for iteration in 0..8 {
        let next_turn_token = CancellationToken::new();
        let ((), next_heartbeat) = tokio::join!(
            heartbeat.shutdown(),
            AccountPoolActivityHeartbeat::start(Arc::clone(&auth_manager), &next_turn_token),
        );
        heartbeat = next_heartbeat;

        let active_rows: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM account_activity WHERE account_id = ? AND expires_at > ?",
        )
        .bind(&account_id)
        .bind(Utc::now().timestamp())
        .fetch_one(&mut connection)
        .await
        .expect("activity row should be readable");
        assert_eq!(
            active_rows, 1,
            "handoff iteration {iteration} erased the new turn's live lease"
        );
    }

    heartbeat.shutdown().await;
    let active_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM account_activity")
        .fetch_one(&mut connection)
        .await
        .expect("final activity count should load");
    assert_eq!(active_rows, 0);
}
