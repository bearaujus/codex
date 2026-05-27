use super::activity::ACCOUNT_ACTIVITY_TTL_SECONDS;
use super::token_refresh::ACCOUNT_TOKEN_REFRESH_LOCK_TTL_SECONDS;
use super::*;

use std::collections::BTreeMap;

use base64::Engine;
use chrono::TimeZone;
use chrono::Utc;
use pretty_assertions::assert_eq;
use serde::Serialize;
use serde_json::json;
use sqlx::Row;
use tempfile::TempDir;

use crate::token_data::TokenData;
use crate::token_data::parse_chatgpt_jwt_claims;

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
    let encode = |bytes: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let header_b64 = encode(&serde_json::to_vec(&header).expect("serialize header"));
    let payload_b64 = encode(&serde_json::to_vec(&payload).expect("serialize payload"));
    let signature_b64 = encode(b"sig");
    format!("{header_b64}.{payload_b64}.{signature_b64}")
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

fn fake_access_token(account_id: &str, exp: i64) -> String {
    fake_unsigned_jwt(json!({
        "exp": exp,
        "https://api.openai.com/auth": {
            "chatgpt_account_id": account_id,
        },
    }))
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

#[derive(Debug, PartialEq, Eq)]
struct ActivityRow {
    account_id: String,
    owner_pid: i64,
    host: String,
    started_at: i64,
    heartbeat_at: i64,
    expires_at: i64,
}

async fn activity_rows(pool: &ChatgptAccountPool) -> Vec<ActivityRow> {
    sqlx::query(
        r#"
        SELECT account_id, owner_pid, host, started_at, heartbeat_at, expires_at
        FROM account_activity
        ORDER BY account_id, owner_pid, host
        "#,
    )
    .fetch_all(&pool.pool)
    .await
    .expect("activity rows should load")
    .into_iter()
    .map(|row| ActivityRow {
        account_id: row.get("account_id"),
        owner_pid: row.get("owner_pid"),
        host: row.get("host"),
        started_at: row.get("started_at"),
        heartbeat_at: row.get("heartbeat_at"),
        expires_at: row.get("expires_at"),
    })
    .collect()
}

#[tokio::test]
async fn open_records_schema_version_in_pool_state() {
    let codex_home = TempDir::new().expect("tempdir");
    let pool = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        None,
    )
    .await
    .expect("pool should open");

    let schema_version: String =
        sqlx::query_scalar("SELECT value FROM pool_state WHERE key = 'schema_version'")
            .fetch_one(&pool.pool)
            .await
            .expect("schema_version should be recorded");
    assert_eq!(schema_version, ACCOUNT_POOL_SCHEMA_VERSION);
}

#[tokio::test]
async fn record_account_activity_creates_live_row() {
    let codex_home = TempDir::new().expect("tempdir");
    let pool = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        None,
    )
    .await
    .expect("pool should open");
    pool.register_account(&chatgpt_auth(
        "activity@example.com",
        "workspace-activity",
        "pro",
    ))
    .await
    .expect("account should register");

    pool.record_account_activity_for_owner_at("workspace-activity", 1001, "host-a", 1_000)
        .await
        .expect("activity should record");

    let rows = activity_rows(&pool).await;
    assert_eq!(
        rows,
        vec![ActivityRow {
            account_id: "workspace-activity".to_string(),
            owner_pid: 1001,
            host: "host-a".to_string(),
            started_at: 1_000,
            heartbeat_at: 1_000,
            expires_at: 1_000 + ACCOUNT_ACTIVITY_TTL_SECONDS,
        }]
    );
    assert!(rows[0].expires_at > 1_000);
}

#[tokio::test]
async fn record_account_activity_refreshes_owner_without_duplicate() {
    let codex_home = TempDir::new().expect("tempdir");
    let pool = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        None,
    )
    .await
    .expect("pool should open");
    pool.register_account(&chatgpt_auth(
        "activity@example.com",
        "workspace-activity",
        "pro",
    ))
    .await
    .expect("account should register");

    pool.record_account_activity_for_owner_at("workspace-activity", 1001, "host-a", 1_000)
        .await
        .expect("initial activity should record");
    pool.record_account_activity_for_owner_at("workspace-activity", 1001, "host-a", 1_025)
        .await
        .expect("activity should refresh");

    assert_eq!(
        activity_rows(&pool).await,
        vec![ActivityRow {
            account_id: "workspace-activity".to_string(),
            owner_pid: 1001,
            host: "host-a".to_string(),
            started_at: 1_000,
            heartbeat_at: 1_025,
            expires_at: 1_025 + ACCOUNT_ACTIVITY_TTL_SECONDS,
        }]
    );
}

#[tokio::test]
async fn record_account_activity_allows_distinct_owners_for_one_account() {
    let codex_home = TempDir::new().expect("tempdir");
    let pool = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        None,
    )
    .await
    .expect("pool should open");
    pool.register_account(&chatgpt_auth(
        "activity@example.com",
        "workspace-activity",
        "pro",
    ))
    .await
    .expect("account should register");

    pool.record_account_activity_for_owner_at("workspace-activity", 1001, "host-a", 1_000)
        .await
        .expect("first owner activity should record");
    pool.record_account_activity_for_owner_at("workspace-activity", 1002, "host-a", 1_005)
        .await
        .expect("second owner activity should record");

    assert_eq!(
        activity_rows(&pool).await,
        vec![
            ActivityRow {
                account_id: "workspace-activity".to_string(),
                owner_pid: 1001,
                host: "host-a".to_string(),
                started_at: 1_000,
                heartbeat_at: 1_000,
                expires_at: 1_000 + ACCOUNT_ACTIVITY_TTL_SECONDS,
            },
            ActivityRow {
                account_id: "workspace-activity".to_string(),
                owner_pid: 1002,
                host: "host-a".to_string(),
                started_at: 1_005,
                heartbeat_at: 1_005,
                expires_at: 1_005 + ACCOUNT_ACTIVITY_TTL_SECONDS,
            },
        ]
    );
}

#[tokio::test]
async fn record_account_activity_garbage_collects_expired_rows() {
    let codex_home = TempDir::new().expect("tempdir");
    let pool = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        None,
    )
    .await
    .expect("pool should open");
    pool.register_account(&chatgpt_auth(
        "activity@example.com",
        "workspace-activity",
        "pro",
    ))
    .await
    .expect("account should register");
    sqlx::query(
        r#"
        INSERT INTO account_activity (
            account_id,
            owner_pid,
            host,
            started_at,
            heartbeat_at,
            expires_at
        )
        VALUES (?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind("workspace-activity")
    .bind(9999)
    .bind("expired-host")
    .bind(900)
    .bind(900)
    .bind(999)
    .execute(&pool.pool)
    .await
    .expect("expired activity should insert");

    pool.record_account_activity_for_owner_at("workspace-activity", 1001, "host-a", 1_000)
        .await
        .expect("activity should record and gc");

    assert_eq!(
        activity_rows(&pool).await,
        vec![ActivityRow {
            account_id: "workspace-activity".to_string(),
            owner_pid: 1001,
            host: "host-a".to_string(),
            started_at: 1_000,
            heartbeat_at: 1_000,
            expires_at: 1_000 + ACCOUNT_ACTIVITY_TTL_SECONDS,
        }]
    );
}

#[tokio::test]
async fn resolve_turn_selection_ignores_live_account_activity() {
    let codex_home = TempDir::new().expect("tempdir");
    let pool = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        None,
    )
    .await
    .expect("pool should open");
    let first_auth = chatgpt_auth("one@example.com", "workspace-1", "pro");
    pool.register_account(&first_auth)
        .await
        .expect("first account should register");
    pool.register_account(&chatgpt_auth("two@example.com", "workspace-2", "pro"))
        .await
        .expect("second account should register");
    pool.record_account_activity_for_owner_at("workspace-1", 1001, "host-a", 1_000)
        .await
        .expect("activity should record");

    let selection = pool
        .resolve_turn_selection(None, false)
        .await
        .expect("selection should succeed");

    assert_eq!(
        selection,
        ChatgptAccountPoolSelectionOutcome::Activated {
            account_id: "workspace-1".to_string(),
            auth: first_auth,
            failover: false,
        }
    );
}

#[tokio::test]
async fn open_creates_external_service_contract_tables() {
    let codex_home = TempDir::new().expect("tempdir");
    let pool = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        None,
    )
    .await
    .expect("pool should open");

    let tables: Vec<String> = sqlx::query_scalar(
        r#"
        SELECT name
        FROM sqlite_master
        WHERE type = 'table'
            AND name IN ('account_activity', 'account_token_locks', 'account_usage_history')
        ORDER BY name
        "#,
    )
    .fetch_all(&pool.pool)
    .await
    .expect("contract tables should be queryable");
    assert_eq!(
        tables,
        vec![
            "account_activity".to_string(),
            "account_token_locks".to_string(),
            "account_usage_history".to_string(),
        ]
    );

    let indexes: Vec<String> = sqlx::query_scalar(
        r#"
        SELECT name
        FROM sqlite_master
        WHERE type = 'index'
            AND name = 'idx_usage_history_acct_time'
        ORDER BY name
        "#,
    )
    .fetch_all(&pool.pool)
    .await
    .expect("usage history index should be queryable");
    assert_eq!(indexes, vec!["idx_usage_history_acct_time".to_string()]);
}

#[tokio::test]
async fn open_migrates_pre_phase_one_db_non_destructively() {
    let codex_home = TempDir::new().expect("tempdir");
    std::fs::create_dir_all(pool_root_dir(codex_home.path())).expect("pool dir should create");
    let legacy_pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            SqliteConnectOptions::new()
                .filename(pool_db_path(codex_home.path()))
                .create_if_missing(true),
        )
        .await
        .expect("legacy pool should open");
    for statement in [
        r#"
        CREATE TABLE accounts (
            account_id TEXT PRIMARY KEY,
            email TEXT,
            plan_type TEXT,
            enabled INTEGER NOT NULL,
            is_default INTEGER NOT NULL,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL,
            last_used_at INTEGER NULL,
            last_auth_refresh_at INTEGER NULL,
            auth_status TEXT NOT NULL,
            cooldown_until INTEGER NULL,
            cooldown_reason TEXT NULL
        )
        "#,
        r#"
        CREATE TABLE account_rate_limits (
            account_id TEXT NOT NULL,
            limit_id TEXT NOT NULL,
            snapshot_json TEXT NOT NULL,
            fetched_at INTEGER NOT NULL,
            PRIMARY KEY (account_id, limit_id)
        )
        "#,
        r#"
        CREATE TABLE account_events (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            account_id TEXT NULL,
            event_type TEXT NOT NULL,
            message TEXT NOT NULL,
            created_at INTEGER NOT NULL
        )
        "#,
        r#"
        CREATE TABLE pool_state (
            key TEXT PRIMARY KEY,
            value TEXT NULL
        )
        "#,
    ] {
        sqlx::query(statement)
            .execute(&legacy_pool)
            .await
            .expect("legacy schema statement should execute");
    }
    sqlx::query(
        r#"
        INSERT INTO accounts (
            account_id,
            email,
            plan_type,
            enabled,
            is_default,
            created_at,
            updated_at,
            last_used_at,
            last_auth_refresh_at,
            auth_status,
            cooldown_until,
            cooldown_reason
        )
        VALUES ('workspace-pre-phase', 'pre@example.com', 'pro', 1, 1, 100, 110, NULL, NULL, 'valid', NULL, NULL)
        "#,
    )
    .execute(&legacy_pool)
    .await
    .expect("legacy account should insert");
    sqlx::query(
        "INSERT INTO pool_state (key, value) VALUES ('selected_account_id', 'workspace-pre-phase')",
    )
    .execute(&legacy_pool)
    .await
    .expect("legacy selected account should insert");
    legacy_pool.close().await;

    let pool = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        None,
    )
    .await
    .expect("pool should open");
    let accounts = pool.list_accounts().await.expect("accounts should list");
    assert_eq!(
        accounts,
        vec![ChatgptAccountPoolAccount {
            account_id: "workspace-pre-phase".to_string(),
            email: Some("pre@example.com".to_string()),
            plan_type: Some("pro".to_string()),
            enabled: true,
            is_default: true,
            is_selected: true,
            created_at: 100,
            updated_at: 110,
            last_used_at: None,
            last_auth_refresh_at: None,
            auth_status: ChatgptAccountPoolAuthStatus::Valid,
            cooldown_until: None,
            cooldown_reason: None,
            rate_limits: BTreeMap::new(),
        }]
    );

    let schema_version: String =
        sqlx::query_scalar("SELECT value FROM pool_state WHERE key = 'schema_version'")
            .fetch_one(&pool.pool)
            .await
            .expect("schema_version should be recorded");
    assert_eq!(schema_version, ACCOUNT_POOL_SCHEMA_VERSION);
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
async fn resolve_turn_selection_reports_no_eligible_accounts_for_stale_current_account() {
    let codex_home = TempDir::new().expect("tempdir");
    let pool = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        None,
    )
    .await
    .expect("pool should open");

    let selection = pool
        .resolve_turn_selection(Some("workspace-stale"), false)
        .await
        .expect("selection should succeed");
    assert_eq!(
        selection,
        ChatgptAccountPoolSelectionOutcome::NoEligibleAccounts
    );
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

#[tokio::test]
async fn token_refresh_lock_is_single_flight_and_releasable() {
    let codex_home = TempDir::new().expect("tempdir");
    let pool = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        None,
    )
    .await
    .expect("pool should open");
    pool.register_account(&chatgpt_auth("lock@example.com", "workspace-lock", "pro"))
        .await
        .expect("account should register");

    assert!(
        pool.try_acquire_token_refresh_lock_at(
            "workspace-lock",
            "host-a:1001",
            ChatgptAccountPool::token_refresh_lock_ttl(),
            1_000,
        )
        .await
        .expect("first lock acquisition should succeed")
    );
    assert!(
        !pool
            .try_acquire_token_refresh_lock_at(
                "workspace-lock",
                "host-b:1002",
                ChatgptAccountPool::token_refresh_lock_ttl(),
                1_001,
            )
            .await
            .expect("second owner should be blocked while the lock is live")
    );
    assert!(
        pool.try_acquire_token_refresh_lock_at(
            "workspace-lock",
            "host-c:1003",
            ChatgptAccountPool::token_refresh_lock_ttl(),
            1_000 + ACCOUNT_TOKEN_REFRESH_LOCK_TTL_SECONDS + 1,
        )
        .await
        .expect("expired lock should be stealable")
    );

    pool.release_token_refresh_lock("workspace-lock", "host-c:1003")
        .await
        .expect("releasing the active lock should succeed");

    assert!(
        pool.try_acquire_token_refresh_lock_at(
            "workspace-lock",
            "host-b:1002",
            ChatgptAccountPool::token_refresh_lock_ttl(),
            1_100,
        )
        .await
        .expect("released lock should be acquirable again")
    );
}

#[tokio::test]
async fn persist_refreshed_account_auth_updates_pool_secret_and_ack() {
    let codex_home = TempDir::new().expect("tempdir");
    let pool = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        None,
    )
    .await
    .expect("pool should open");
    pool.register_account(&chatgpt_auth(
        "refresh@example.com",
        "workspace-refresh",
        "pro",
    ))
    .await
    .expect("account should register");
    sqlx::query(
        "UPDATE accounts SET last_auth_refresh_at = ?, updated_at = ? WHERE account_id = ?",
    )
    .bind(10_i64)
    .bind(10_i64)
    .bind("workspace-refresh")
    .execute(&pool.pool)
    .await
    .expect("seed account timestamps should update");

    let mut rotated_auth = chatgpt_auth("refresh@example.com", "workspace-refresh", "pro");
    let rotated_tokens = rotated_auth.tokens.as_mut().expect("tokens should exist");
    rotated_tokens.access_token = fake_access_token("workspace-refresh", 4_000);
    rotated_tokens.refresh_token = "refresh-rotated".to_string();
    rotated_auth.last_refresh = Utc.timestamp_opt(2_000, 0).single();

    pool.persist_refreshed_account_auth("workspace-refresh", &rotated_auth)
        .await
        .expect("refreshed auth should persist");

    let stored_pool_auth = load_auth_dot_json(
        &account_pool_secret_dir(codex_home.path(), "workspace-refresh"),
        AuthCredentialsStoreMode::File,
    )
    .expect("pool auth should load")
    .expect("pool auth should exist");
    assert_eq!(stored_pool_auth, rotated_auth);

    let last_auth_refresh_at = pool
        .account_last_auth_refresh_at("workspace-refresh")
        .await
        .expect("last_auth_refresh_at should load")
        .expect("last_auth_refresh_at should be populated");
    assert!(last_auth_refresh_at > 10);

    let updated_at: i64 = sqlx::query_scalar(
        "SELECT updated_at FROM accounts WHERE account_id = 'workspace-refresh'",
    )
    .fetch_one(&pool.pool)
    .await
    .expect("updated_at should load");
    assert_eq!(updated_at, last_auth_refresh_at);

    let event = sqlx::query(
        r#"
        SELECT account_id, event_type
        FROM account_events
        ORDER BY id DESC
        LIMIT 1
        "#,
    )
    .fetch_one(&pool.pool)
    .await
    .expect("refresh event should load");
    assert_eq!(
        event.get::<Option<String>, _>("account_id"),
        Some("workspace-refresh".to_string())
    );
    assert_eq!(
        event.get::<String, _>("event_type"),
        "account_auth_refreshed".to_string()
    );
}

#[test]
fn account_auth_needs_token_refresh_respects_access_token_expiration() {
    let now = Utc
        .timestamp_opt(1_900, 0)
        .single()
        .expect("valid timestamp");

    let mut future_auth = chatgpt_auth("future@example.com", "workspace-exp", "pro");
    future_auth
        .tokens
        .as_mut()
        .expect("tokens should exist")
        .access_token = fake_access_token("workspace-exp", 2_000);
    assert_eq!(
        ChatgptAccountPool::account_auth_needs_token_refresh(&future_auth, now),
        false,
    );

    let mut expired_auth = chatgpt_auth("expired@example.com", "workspace-exp", "pro");
    expired_auth
        .tokens
        .as_mut()
        .expect("tokens should exist")
        .access_token = fake_access_token("workspace-exp", 1_800);
    assert_eq!(
        ChatgptAccountPool::account_auth_needs_token_refresh(&expired_auth, now),
        true,
    );
}
