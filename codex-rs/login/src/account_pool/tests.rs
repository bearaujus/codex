use super::activity::ACCOUNT_ACTIVITY_TTL_SECONDS;
use super::token_refresh::ACCOUNT_TOKEN_REFRESH_LOCK_TTL_SECONDS;
use super::*;

use std::collections::BTreeMap;
use std::collections::HashSet;

use base64::Engine;
use chrono::TimeZone;
use chrono::Utc;
use pretty_assertions::assert_eq;
use serde::Serialize;
use serde_json::json;
use sqlx::Row;
use tempfile::TempDir;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

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
        pool_account_id: Some(account_id.to_string()),
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

#[derive(Debug, PartialEq)]
struct UsageHistoryRow {
    account_id: String,
    limit_id: String,
    fetched_at: i64,
    snapshot: RateLimitSnapshot,
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

async fn usage_history_rows(pool: &ChatgptAccountPool) -> Vec<UsageHistoryRow> {
    sqlx::query(
        r#"
        SELECT account_id, limit_id, fetched_at, snapshot_json
        FROM account_usage_history
        ORDER BY id
        "#,
    )
    .fetch_all(&pool.pool)
    .await
    .expect("usage history rows should load")
    .into_iter()
    .map(|row| UsageHistoryRow {
        account_id: row.get("account_id"),
        limit_id: row.get("limit_id"),
        fetched_at: row.get("fetched_at"),
        snapshot: serde_json::from_str(&row.get::<String, _>("snapshot_json"))
            .expect("usage history snapshot should decode"),
    })
    .collect()
}

fn codex_snapshot(used_percent: f64) -> RateLimitSnapshot {
    RateLimitSnapshot {
        limit_id: Some("codex".to_string()),
        limit_name: None,
        primary: Some(RateLimitWindow {
            used_percent,
            window_minutes: Some(300),
            resets_at: Some(3_600),
        }),
        secondary: Some(RateLimitWindow {
            used_percent: used_percent / 2.0,
            window_minutes: Some(10_080),
            resets_at: Some(7_200),
        }),
        credits: None,
        plan_type: Some(AccountPlanType::Pro),
        rate_limit_reached_type: None,
    }
}

fn premium_snapshot(balance: &str) -> RateLimitSnapshot {
    RateLimitSnapshot {
        limit_id: Some("premium".to_string()),
        limit_name: Some("premium".to_string()),
        primary: None,
        secondary: None,
        credits: Some(codex_protocol::protocol::CreditsSnapshot {
            has_credits: balance != "0",
            unlimited: false,
            balance: Some(balance.to_string()),
        }),
        plan_type: Some(AccountPlanType::Pro),
        rate_limit_reached_type: None,
    }
}

fn metered_feature_snapshot(
    limit_id: &str,
    used_percent: f64,
    resets_at: i64,
) -> RateLimitSnapshot {
    RateLimitSnapshot {
        limit_id: Some(limit_id.to_string()),
        limit_name: Some(limit_id.to_string()),
        primary: Some(RateLimitWindow {
            used_percent,
            window_minutes: Some(60),
            resets_at: Some(resets_at),
        }),
        secondary: None,
        credits: None,
        plan_type: Some(AccountPlanType::Pro),
        rate_limit_reached_type: None,
    }
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
async fn record_account_activity_moves_same_owner_between_accounts() {
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
        .expect("first account should register");
    pool.register_account(&chatgpt_auth("two@example.com", "workspace-2", "pro"))
        .await
        .expect("second account should register");

    pool.record_account_activity_for_owner_at("workspace-1", 1001, "host-a", 1_000)
        .await
        .expect("first owner activity should record");
    pool.record_account_activity_for_owner_at("workspace-2", 1001, "host-a", 1_025)
        .await
        .expect("owner activity should move");

    assert_eq!(
        activity_rows(&pool).await,
        vec![ActivityRow {
            account_id: "workspace-2".to_string(),
            owner_pid: 1001,
            host: "host-a".to_string(),
            started_at: 1_025,
            heartbeat_at: 1_025,
            expires_at: 1_025 + ACCOUNT_ACTIVITY_TTL_SECONDS,
        }]
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
async fn resolve_turn_selection_falls_back_to_busy_account_when_no_idle_account_exists() {
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
        .expect("first activity should record");
    pool.record_account_activity_for_owner_at("workspace-2", 1002, "host-b", 1_005)
        .await
        .expect("second activity should record");

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
async fn register_account_sets_selected() {
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
    assert!(registered.is_selected);

    let selected = pool
        .selected_account_auth()
        .await
        .expect("selected auth lookup should succeed")
        .expect("selected auth should exist");
    assert_eq!(selected.0, "workspace-1");
}

#[tokio::test]
async fn record_fetched_rate_limits_replaces_latest_snapshot_set_and_appends_history() {
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
        .expect("account should register");

    let first_codex = codex_snapshot(42.0);
    let first_premium = premium_snapshot("3");
    pool.record_fetched_rate_limits("workspace-1", &[first_codex.clone(), first_premium.clone()])
        .await
        .expect("first fetch should persist");

    let second_codex = codex_snapshot(18.0);
    let entry = pool
        .record_fetched_rate_limits("workspace-1", std::slice::from_ref(&second_codex))
        .await
        .expect("second fetch should persist");

    assert_eq!(entry.account_id, "workspace-1");
    assert!(entry.fetched_at.is_some());
    assert_eq!(
        entry.rate_limits,
        BTreeMap::from([("codex".to_string(), second_codex.clone())])
    );

    let latest = pool
        .list_rate_limits()
        .await
        .expect("rate limits should load");
    assert_eq!(latest.len(), 1);
    assert_eq!(latest[0].account_id, "workspace-1");
    assert!(latest[0].fetched_at.is_some());
    assert_eq!(
        latest[0].rate_limits,
        BTreeMap::from([("codex".to_string(), second_codex.clone())])
    );

    let history = usage_history_rows(&pool).await;
    assert_eq!(history.len(), 3);
    assert_eq!(history[0].snapshot, first_codex);
    assert_eq!(history[1].snapshot, first_premium);
    assert_eq!(history[2].snapshot, second_codex);
}

#[tokio::test]
async fn record_rate_limit_snapshot_preserves_other_latest_buckets_and_appends_history() {
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
        .expect("account should register");

    let first_codex = codex_snapshot(42.0);
    let first_premium = premium_snapshot("3");
    pool.record_fetched_rate_limits("workspace-1", &[first_codex.clone(), first_premium.clone()])
        .await
        .expect("initial fetch should persist");

    let updated_codex = codex_snapshot(26.0);
    pool.record_rate_limit_snapshot("workspace-1", &updated_codex)
        .await
        .expect("single snapshot observation should persist");

    let latest = pool
        .list_rate_limits()
        .await
        .expect("rate limits should load");
    assert_eq!(latest.len(), 1);
    assert_eq!(latest[0].account_id, "workspace-1");
    assert_eq!(
        latest[0].rate_limits,
        BTreeMap::from([
            ("codex".to_string(), updated_codex.clone()),
            ("premium".to_string(), first_premium.clone()),
        ])
    );

    let history = usage_history_rows(&pool).await;
    assert_eq!(history.len(), 3);
    assert_eq!(history[0].snapshot, first_codex);
    assert_eq!(history[1].snapshot, first_premium);
    assert_eq!(history[2].snapshot, updated_codex);
}

#[tokio::test]
async fn record_fetched_rate_limits_empty_refresh_clears_stale_latest_rows_and_cooldown() {
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
        .expect("account should register");

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

    let entry = pool
        .record_fetched_rate_limits("workspace-1", &[])
        .await
        .expect("empty fetch should persist");

    assert_eq!(entry.account_id, "workspace-1");
    assert!(entry.fetched_at.is_some());
    assert!(entry.rate_limits.is_empty());

    let latest = pool
        .list_rate_limits()
        .await
        .expect("rate limits should load");
    assert!(latest.is_empty());

    let account = pool
        .list_accounts()
        .await
        .expect("accounts should load")
        .into_iter()
        .find(|account| account.account_id == "workspace-1")
        .expect("workspace-1 should remain in pool");
    assert_eq!(account.cooldown_until, None);
    assert_eq!(account.cooldown_reason, None);
}

#[tokio::test]
async fn record_fetched_rate_limits_ignores_auxiliary_limit_exhaustion_for_account_cooldown() {
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
        .expect("account should register");

    let codex_snapshot = codex_snapshot(18.0);
    let overdrive_snapshot = metered_feature_snapshot("overdrive", 100.0, now_ts() + 3_600);
    pool.record_fetched_rate_limits(
        "workspace-1",
        &[codex_snapshot.clone(), overdrive_snapshot.clone()],
    )
    .await
    .expect("fetch should persist");

    let account = pool
        .list_accounts()
        .await
        .expect("accounts should load")
        .into_iter()
        .find(|account| account.account_id == "workspace-1")
        .expect("workspace-1 should remain in pool");
    assert_eq!(account.cooldown_until, None);
    assert_eq!(account.cooldown_reason, None);
    assert_eq!(
        account.rate_limits,
        BTreeMap::from([
            ("codex".to_string(), codex_snapshot),
            ("overdrive".to_string(), overdrive_snapshot),
        ])
    );
}

#[tokio::test]
async fn record_fetched_rate_limits_uses_only_exhausted_window_reset_for_cooldown() {
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
        .expect("account should register");

    let primary_reset_at = now_ts() + 1_800;
    let secondary_reset_at = now_ts() + 86_400;
    let snapshot = RateLimitSnapshot {
        limit_id: Some("codex".to_string()),
        limit_name: None,
        primary: Some(RateLimitWindow {
            used_percent: 100.0,
            window_minutes: Some(300),
            resets_at: Some(primary_reset_at),
        }),
        secondary: Some(RateLimitWindow {
            used_percent: 32.0,
            window_minutes: Some(10_080),
            resets_at: Some(secondary_reset_at),
        }),
        credits: None,
        plan_type: Some(AccountPlanType::Pro),
        rate_limit_reached_type: Some(RateLimitReachedType::RateLimitReached),
    };

    pool.record_fetched_rate_limits("workspace-1", &[snapshot])
        .await
        .expect("fetch should persist");

    let account = pool
        .list_accounts()
        .await
        .expect("accounts should load")
        .into_iter()
        .find(|account| account.account_id == "workspace-1")
        .expect("workspace-1 should remain in pool");
    assert_eq!(account.cooldown_until, Some(primary_reset_at));
    assert_eq!(
        account.cooldown_reason,
        Some("rate_limits_refreshed".to_string())
    );
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
async fn resolve_turn_selection_skips_selected_account_with_missing_secret() {
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

    std::fs::remove_file(
        account_pool_secret_dir(codex_home.path(), "workspace-1").join("auth.json"),
    )
    .expect("selected account secret should be removable");

    let selection = pool
        .resolve_turn_selection(None, false)
        .await
        .expect("selection should succeed");
    let ChatgptAccountPoolSelectionOutcome::Activated { account_id, .. } = selection else {
        panic!("expected activation");
    };
    assert_eq!(account_id, "workspace-2");

    let accounts = pool.list_accounts().await.expect("accounts should list");
    assert_eq!(
        accounts
            .into_iter()
            .find(|account| account.account_id == "workspace-1")
            .expect("workspace-1 should remain in pool")
            .auth_status,
        ChatgptAccountPoolAuthStatus::MissingSecret,
    );
}

#[tokio::test]
async fn resolve_turn_selection_marks_switch_from_current_to_selected_account_as_failover() {
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
    pool.select_account("workspace-2")
        .await
        .expect("selection should succeed");

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
        panic!("expected activation");
    };
    assert_eq!(account_id, "workspace-2");
    assert!(failover);
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
async fn resolve_turn_selection_skips_best_fallback_account_with_missing_secret() {
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
    pool.register_account(&chatgpt_auth("three@example.com", "workspace-3", "pro"))
        .await
        .expect("third account");
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
    std::fs::remove_file(
        account_pool_secret_dir(codex_home.path(), "workspace-2").join("auth.json"),
    )
    .expect("best fallback secret should be removable");

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
    assert_eq!(account_id, "workspace-3");
    assert!(failover);

    let accounts = pool.list_accounts().await.expect("accounts should list");
    assert_eq!(
        accounts
            .into_iter()
            .find(|account| account.account_id == "workspace-2")
            .expect("workspace-2 should remain in pool")
            .auth_status,
        ChatgptAccountPoolAuthStatus::MissingSecret,
    );
}

#[test]
fn capacity_score_treats_missing_rate_limits_as_unvalidated() {
    let account = ChatgptAccountPoolAccount {
        account_id: "workspace-activity".to_string(),
        workspace_account_id: "workspace-activity".to_string(),
        member_identity_key: None,
        chatgpt_user_id: None,
        subject: None,
        email: Some("activity@example.com".to_string()),
        plan_type: Some("pro".to_string()),
        enabled: true,
        is_selected: false,
        created_at: 1,
        updated_at: 1,
        last_used_at: None,
        last_auth_refresh_at: None,
        auth_status: ChatgptAccountPoolAuthStatus::Valid,
        cooldown_until: None,
        cooldown_reason: None,
        rate_limits: BTreeMap::new(),
    };

    assert_eq!(capacity_score(&account, 1_000), (false, 100));
}

#[tokio::test]
async fn resolve_turn_selection_prefers_validated_fallback_over_unvalidated_account() {
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
    pool.register_account(&chatgpt_auth("three@example.com", "workspace-3", "pro"))
        .await
        .expect("third account");
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
    pool.record_fetched_rate_limits(
        "workspace-2",
        &[RateLimitSnapshot {
            limit_id: Some("codex".to_string()),
            limit_name: None,
            primary: Some(RateLimitWindow {
                used_percent: 25.0,
                window_minutes: Some(300),
                resets_at: Some(now_ts() + 1800),
            }),
            secondary: None,
            credits: None,
            plan_type: None,
            rate_limit_reached_type: None,
        }],
    )
    .await
    .expect("validated fallback should record rate limits");

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

fn pending_account(
    account_id: &str,
    enabled: bool,
    cooldown_until: Option<i64>,
) -> ChatgptAccountPoolAccount {
    ChatgptAccountPoolAccount {
        account_id: account_id.to_string(),
        workspace_account_id: account_id.to_string(),
        member_identity_key: None,
        chatgpt_user_id: None,
        subject: None,
        email: None,
        plan_type: Some("pro".to_string()),
        enabled,
        is_selected: false,
        created_at: 1,
        updated_at: 1,
        last_used_at: None,
        last_auth_refresh_at: None,
        auth_status: ChatgptAccountPoolAuthStatus::PendingValidation,
        cooldown_until,
        cooldown_reason: None,
        rate_limits: BTreeMap::new(),
    }
}

#[test]
fn capacity_score_treats_pending_validation_as_full_capacity() {
    // A pending account scores above an idle valid account ((false, 100)) so the
    // scorer prefers bringing fresh capacity online.
    let account = pending_account("pending", true, None);
    assert_eq!(capacity_score(&account, 1_000), (true, 100));
}

#[test]
fn select_best_candidate_prefers_pending_over_idle_valid_and_skips_ineligible() {
    let now = 1_000;
    // An idle valid account with no usage data scores (false, 100).
    let mut idle_valid = pending_account("idle-valid", true, None);
    idle_valid.auth_status = ChatgptAccountPoolAuthStatus::Valid;
    let accounts = vec![
        idle_valid,
        pending_account("pending-cooldown", true, Some(now + 600)),
        pending_account("pending-disabled", false, None),
        pending_account("pending-probed", true, None),
        pending_account("pending-ok", true, None),
    ];

    let mut probed = HashSet::new();
    probed.insert("pending-probed".to_string());
    assert_eq!(
        select_best_candidate(&accounts, now, &probed),
        Some("pending-ok"),
        "a usable pending account outranks an idle valid one; ineligible pending \
         accounts (cooled down / disabled / already probed) are skipped"
    );

    probed.insert("pending-ok".to_string());
    assert_eq!(
        select_best_candidate(&accounts, now, &probed),
        Some("idle-valid"),
        "once no usable pending account remains, selection falls back to the valid one"
    );
}

async fn set_pending_validation(pool: &ChatgptAccountPool, account_id: &str) {
    sqlx::query("UPDATE accounts SET auth_status = 'pending_validation' WHERE account_id = ?")
        .bind(account_id)
        .execute(&pool.pool)
        .await
        .expect("account should move to pending_validation");
}

fn chatgpt_auth_with_live_token(email: &str, account_id: &str) -> AuthDotJson {
    let mut auth = chatgpt_auth(email, account_id, "pro");
    auth.tokens
        .as_mut()
        .expect("tokens should exist")
        .access_token = fake_access_token(account_id, Utc::now().timestamp() + 3600);
    auth
}

fn chatgpt_auth_with_stale_token(email: &str, account_id: &str) -> AuthDotJson {
    let mut auth = chatgpt_auth(email, account_id, "pro");
    auth.tokens
        .as_mut()
        .expect("tokens should exist")
        .access_token = fake_access_token(account_id, Utc::now().timestamp() - 3600);
    auth
}

/// Restores a process env var on drop so serial tests never leak the refresh
/// URL override into other tests.
struct EnvGuard {
    key: &'static str,
    original: Option<std::ffi::OsString>,
}

impl EnvGuard {
    fn set(key: &'static str, value: String) -> Self {
        let original = std::env::var_os(key);
        // SAFETY: tests sharing this override run serially (see #[serial]).
        unsafe { std::env::set_var(key, &value) };
        Self { key, original }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: restore the prior value before any other test observes it.
        unsafe {
            match &self.original {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }
}

#[serial_test::serial(account_pool_refresh_url)]
#[tokio::test]
async fn resolve_turn_selection_refreshes_stale_pending_token_before_probe() {
    let server = MockServer::start().await;
    // A stale pending token is refreshed first; the rotated tokens come back here.
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id_token": fake_jwt("one@example.com", "workspace-1", "pro"),
            "access_token": fake_access_token("workspace-1", Utc::now().timestamp() + 3600),
            "refresh_token": "refresh-workspace-1-rotated",
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/backend-api/wham/usage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "plan_type": "pro" })))
        .mount(&server)
        .await;
    let _env_guard = EnvGuard::set(
        crate::REFRESH_TOKEN_URL_OVERRIDE_ENV_VAR,
        format!("{}/oauth/token", server.uri()),
    );
    let base_url = format!("{}/backend-api", server.uri());

    let codex_home = TempDir::new().expect("tempdir");
    let pool = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        Some(base_url),
    )
    .await
    .expect("pool should open");
    pool.register_account(&chatgpt_auth_with_stale_token(
        "one@example.com",
        "workspace-1",
    ))
    .await
    .expect("account should register");
    set_pending_validation(&pool, "workspace-1").await;

    let selection = pool
        .resolve_turn_selection(None, false)
        .await
        .expect("selection should succeed");
    let ChatgptAccountPoolSelectionOutcome::Activated { account_id, .. } = selection else {
        panic!("expected activation after refresh-then-probe, got {selection:?}");
    };
    assert_eq!(account_id, "workspace-1");

    let accounts = pool.list_accounts().await.expect("accounts should list");
    assert_eq!(
        accounts
            .iter()
            .find(|account| account.account_id == "workspace-1")
            .expect("account should remain")
            .auth_status,
        ChatgptAccountPoolAuthStatus::Valid,
        "a stale token is refreshed and the probe then promotes the account to valid"
    );
    let persisted = pool
        .load_account_secret("workspace-1")
        .expect("secret should load")
        .expect("secret should exist");
    assert_eq!(
        persisted.tokens.expect("tokens should exist").refresh_token,
        "refresh-workspace-1-rotated",
        "the rotated refresh token must be persisted so the next pickup does not reuse a spent one"
    );
}

#[serial_test::serial(account_pool_refresh_url)]
#[tokio::test]
async fn resolve_turn_selection_leaves_pending_account_pending_when_refresh_fails_transiently() {
    let server = MockServer::start().await;
    // Transient (5xx) refresh failure: the probe must NOT run and the account
    // must stay pending rather than being condemned.
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/backend-api/wham/usage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "plan_type": "pro" })))
        .mount(&server)
        .await;
    let _env_guard = EnvGuard::set(
        crate::REFRESH_TOKEN_URL_OVERRIDE_ENV_VAR,
        format!("{}/oauth/token", server.uri()),
    );
    let base_url = format!("{}/backend-api", server.uri());

    let codex_home = TempDir::new().expect("tempdir");
    let pool = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        Some(base_url),
    )
    .await
    .expect("pool should open");
    pool.register_account(&chatgpt_auth_with_stale_token(
        "one@example.com",
        "workspace-1",
    ))
    .await
    .expect("account should register");
    set_pending_validation(&pool, "workspace-1").await;

    let selection = pool
        .resolve_turn_selection(None, false)
        .await
        .expect("selection should succeed");
    assert!(
        matches!(
            selection,
            ChatgptAccountPoolSelectionOutcome::NoEligibleAccounts
        ),
        "a transient refresh failure leaves no usable account, got {selection:?}"
    );

    let accounts = pool.list_accounts().await.expect("accounts should list");
    assert_eq!(
        accounts
            .iter()
            .find(|account| account.account_id == "workspace-1")
            .expect("account should remain")
            .auth_status,
        ChatgptAccountPoolAuthStatus::PendingValidation,
        "a transient refresh failure must not condemn the account; it stays pending"
    );
}

#[serial_test::serial(account_pool_refresh_url)]
#[tokio::test]
async fn resolve_turn_selection_marks_pending_account_invalid_when_refresh_token_rejected() {
    let server = MockServer::start().await;
    // Authoritative refresh rejection (401): the refresh token is dead, so the
    // account is genuinely unusable and should be marked invalid.
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    let _env_guard = EnvGuard::set(
        crate::REFRESH_TOKEN_URL_OVERRIDE_ENV_VAR,
        format!("{}/oauth/token", server.uri()),
    );
    let base_url = format!("{}/backend-api", server.uri());

    let codex_home = TempDir::new().expect("tempdir");
    let pool = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        Some(base_url),
    )
    .await
    .expect("pool should open");
    pool.register_account(&chatgpt_auth_with_stale_token(
        "one@example.com",
        "workspace-1",
    ))
    .await
    .expect("account should register");
    set_pending_validation(&pool, "workspace-1").await;

    let selection = pool
        .resolve_turn_selection(None, false)
        .await
        .expect("selection should succeed");
    assert!(
        matches!(
            selection,
            ChatgptAccountPoolSelectionOutcome::NoEligibleAccounts
        ),
        "a rejected refresh token leaves no usable account, got {selection:?}"
    );

    let accounts = pool.list_accounts().await.expect("accounts should list");
    assert_eq!(
        accounts
            .iter()
            .find(|account| account.account_id == "workspace-1")
            .expect("account should remain")
            .auth_status,
        ChatgptAccountPoolAuthStatus::Invalid,
        "an authoritative refresh-token rejection marks the account invalid"
    );
}

#[tokio::test]
async fn resolve_turn_selection_validates_pending_account_on_pickup() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/backend-api/wham/usage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "plan_type": "pro" })))
        .mount(&server)
        .await;
    let base_url = format!("{}/backend-api", server.uri());

    let codex_home = TempDir::new().expect("tempdir");
    let pool = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        Some(base_url),
    )
    .await
    .expect("pool should open");
    pool.register_account(&chatgpt_auth_with_live_token(
        "one@example.com",
        "workspace-1",
    ))
    .await
    .expect("account should register");
    set_pending_validation(&pool, "workspace-1").await;

    let selection = pool
        .resolve_turn_selection(None, false)
        .await
        .expect("selection should succeed");
    let ChatgptAccountPoolSelectionOutcome::Activated { account_id, .. } = selection else {
        panic!("expected activation after validate-on-pickup, got {selection:?}");
    };
    assert_eq!(account_id, "workspace-1");

    let accounts = pool.list_accounts().await.expect("accounts should list");
    assert_eq!(
        accounts
            .iter()
            .find(|account| account.account_id == "workspace-1")
            .expect("account should remain")
            .auth_status,
        ChatgptAccountPoolAuthStatus::Valid,
        "a successful pickup probe promotes the account to valid"
    );
    assert!(
        usage_history_rows(&pool)
            .await
            .iter()
            .any(|row| row.account_id == "workspace-1"),
        "the usage snapshot fetched during validation should be stored"
    );
}

#[tokio::test]
async fn resolve_turn_selection_marks_pending_account_invalid_on_probe_401() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/backend-api/wham/usage"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    let base_url = format!("{}/backend-api", server.uri());

    let codex_home = TempDir::new().expect("tempdir");
    let pool = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        Some(base_url),
    )
    .await
    .expect("pool should open");
    pool.register_account(&chatgpt_auth_with_live_token(
        "one@example.com",
        "workspace-1",
    ))
    .await
    .expect("account should register");
    set_pending_validation(&pool, "workspace-1").await;

    let selection = pool
        .resolve_turn_selection(None, false)
        .await
        .expect("selection should succeed");
    assert!(
        matches!(
            selection,
            ChatgptAccountPoolSelectionOutcome::NoEligibleAccounts
        ),
        "a 401 during validation leaves no usable account, got {selection:?}"
    );

    let accounts = pool.list_accounts().await.expect("accounts should list");
    assert_eq!(
        accounts
            .iter()
            .find(|account| account.account_id == "workspace-1")
            .expect("account should remain")
            .auth_status,
        ChatgptAccountPoolAuthStatus::Invalid,
        "an authoritative 401 marks the pending account invalid"
    );
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
            1_000 + ACCOUNT_TOKEN_REFRESH_LOCK_TTL_SECONDS,
        )
        .await
        .expect("lock should be stealable at the expiry boundary")
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

#[tokio::test]
async fn persist_refreshed_account_auth_restores_valid_auth_status() {
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
    pool.mark_account_auth_failed("workspace-refresh", "refresh token expired")
        .await
        .expect("auth status should update");

    let mut rotated_auth = chatgpt_auth("refresh@example.com", "workspace-refresh", "pro");
    rotated_auth
        .tokens
        .as_mut()
        .expect("tokens should exist")
        .access_token = fake_access_token("workspace-refresh", 4_000);
    rotated_auth.last_refresh = Utc.timestamp_opt(2_000, 0).single();

    pool.persist_refreshed_account_auth("workspace-refresh", &rotated_auth)
        .await
        .expect("refreshed auth should persist");

    let account = pool
        .list_accounts()
        .await
        .expect("accounts should load")
        .into_iter()
        .find(|account| account.account_id == "workspace-refresh")
        .expect("workspace-refresh should remain in pool");
    assert_eq!(account.auth_status, ChatgptAccountPoolAuthStatus::Valid);
}

#[tokio::test]
async fn persist_refreshed_account_auth_uses_pool_credentials_store_mode() {
    let codex_home = TempDir::new().expect("tempdir");
    let pool = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::Ephemeral,
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

    let mut rotated_auth = chatgpt_auth("refresh@example.com", "workspace-refresh", "pro");
    rotated_auth
        .tokens
        .as_mut()
        .expect("tokens should exist")
        .access_token = fake_access_token("workspace-refresh", 4_000);

    pool.persist_refreshed_account_auth("workspace-refresh", &rotated_auth)
        .await
        .expect("refreshed auth should persist");

    let stored_pool_auth = load_auth_dot_json(
        &account_pool_secret_dir(codex_home.path(), "workspace-refresh"),
        AuthCredentialsStoreMode::Ephemeral,
    )
    .expect("pool auth should load")
    .expect("pool auth should exist");
    assert_eq!(stored_pool_auth, rotated_auth);
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
