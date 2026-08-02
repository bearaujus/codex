use super::activity::ACCOUNT_ACTIVITY_TTL_SECONDS;
use super::token_refresh::ACCOUNT_TOKEN_REFRESH_LOCK_TTL_SECONDS;
use super::*;

use std::collections::BTreeMap;
use std::collections::HashSet;
use std::path::Path;

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

use crate::save_auth;
use crate::token_data::TokenData;
use crate::token_data::parse_chatgpt_jwt_claims;

async fn open_raw_pool(codex_home: &Path) -> SqlitePool {
    std::fs::create_dir_all(pool_root_dir(codex_home)).expect("pool root should be created");
    codex_state::open_durable_single_connection_sqlite_pool(&pool_db_path(codex_home))
        .await
        .expect("raw SQLite pool should open")
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

fn fake_jwt_with_member(
    email: &str,
    account_id: &str,
    plan_type: &str,
    chatgpt_user_id: &str,
) -> String {
    fake_unsigned_jwt(json!({
        "email": email,
        "sub": format!("subject-{chatgpt_user_id}"),
        "https://api.openai.com/auth": {
            "chatgpt_account_id": account_id,
            "chatgpt_plan_type": plan_type,
            "chatgpt_user_id": chatgpt_user_id,
            "user_id": format!("legacy-{chatgpt_user_id}"),
        },
    }))
}

#[cfg(unix)]
#[tokio::test]
async fn pool_root_directory_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;

    let codex_home = TempDir::new().expect("tempdir");
    ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        /*chatgpt_base_url*/ None,
    )
    .await
    .expect("pool should open");

    let mode = std::fs::metadata(pool_root_dir(codex_home.path()))
        .expect("pool root metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o700);
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
        auth_mode: None,
        tokens: Some(TokenData {
            id_token: parse_chatgpt_jwt_claims(&id_token).expect("id token should parse"),
            access_token: fake_jwt(email, account_id, plan_type),
            refresh_token: format!("refresh-{account_id}"),
            account_id: Some(account_id.to_string()),
        }),
        pool_account_id: Some(account_id.to_string()),
        last_refresh: Some(
            Utc.timestamp_opt(Utc::now().timestamp(), 0)
                .single()
                .expect("valid timestamp"),
        ),
        agent_identity: None,
    }
}

fn chatgpt_auth_with_member(
    email: &str,
    workspace_account_id: &str,
    plan_type: &str,
    chatgpt_user_id: &str,
    pool_account_id: &str,
) -> AuthDotJson {
    let mut auth = chatgpt_auth(email, workspace_account_id, plan_type);
    let id_token = fake_jwt_with_member(email, workspace_account_id, plan_type, chatgpt_user_id);
    auth.tokens.as_mut().expect("tokens").id_token =
        parse_chatgpt_jwt_claims(&id_token).expect("id token should parse");
    auth.pool_account_id = Some(pool_account_id.to_string());
    auth
}

#[test]
fn usage_endpoint_url_matches_official_backend_path_styles() {
    assert_eq!(
        usage_endpoint_url("https://chatgpt.com/backend-api/"),
        "https://chatgpt.com/backend-api/wham/usage"
    );
    assert_eq!(
        usage_endpoint_url("https://proxy.example/api-root"),
        "https://proxy.example/api-root/api/codex/usage"
    );
}

#[tokio::test]
async fn usage_request_headers_match_authenticated_codex_backend_requests() {
    let codex_home = TempDir::new().expect("tempdir");
    let pool = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        /*chatgpt_base_url*/ None,
    )
    .await
    .expect("pool should open");
    let mut auth = chatgpt_auth("one@example.com", "workspace-1", "pro");
    let fedramp_id_token = fake_unsigned_jwt(json!({
        "email": "one@example.com",
        "https://api.openai.com/auth": {
            "chatgpt_account_id": "workspace-1",
            "chatgpt_plan_type": "pro",
            "chatgpt_account_is_fedramp": true,
        },
    }));
    auth.tokens.as_mut().expect("tokens").id_token =
        parse_chatgpt_jwt_claims(&fedramp_id_token).expect("id token should parse");
    pool.register_account(&auth)
        .await
        .expect("account should register");
    let codex_auth = pool
        .load_account_codex_auth("workspace-1")
        .await
        .expect("account auth should load")
        .expect("account auth should exist");

    let headers = usage_request_headers(&codex_auth).expect("headers should build");
    let expected_authorization = format!(
        "Bearer {}",
        fake_jwt("one@example.com", "workspace-1", "pro")
    );
    assert_eq!(
        headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok()),
        Some(expected_authorization.as_str())
    );
    assert_eq!(
        headers
            .get("ChatGPT-Account-Id")
            .and_then(|value| value.to_str().ok()),
        Some("workspace-1")
    );
    assert_eq!(
        headers
            .get("X-OpenAI-Fedramp")
            .and_then(|value| value.to_str().ok()),
        Some("true")
    );
    assert_eq!(
        headers
            .get("originator")
            .and_then(|value| value.to_str().ok()),
        Some(originator().value.as_str())
    );
    assert!(
        headers
            .get(USER_AGENT)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| !value.is_empty())
    );
}

#[tokio::test]
async fn malformed_usage_payload_does_not_expose_response_body() {
    const SENTINEL: &str = "secret-access-token-echo";
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/backend-api/wham/usage"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string(format!(r#"{{"unexpected":"{SENTINEL}""#)),
        )
        .expect(1)
        .mount(&server)
        .await;
    let base_url = format!("{}/backend-api", server.uri());
    let codex_home = TempDir::new().expect("tempdir");
    let pool = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        Some(base_url.clone()),
    )
    .await
    .expect("pool should open");
    let auth = chatgpt_auth("one@example.com", "workspace-1", "pro");
    pool.register_account(&auth)
        .await
        .expect("account should register");
    let codex_auth = pool
        .load_account_codex_auth("workspace-1")
        .await
        .expect("account auth should load")
        .expect("account auth should exist");

    let error = match fetch_usage_snapshots_with_status(&base_url, &codex_auth).await {
        UsageFetchOutcome::Failed(error) => error,
        _ => panic!("malformed usage payload should fail"),
    };
    let rendered = error.to_string();
    assert!(
        !rendered.contains(SENTINEL),
        "usage parser error leaked response body: {rendered}"
    );
    assert!(rendered.contains("failed to decode rate-limit payload"));
}

#[test]
fn account_metadata_falls_back_from_empty_token_account_id_to_the_id_token_claim() {
    let mut auth = chatgpt_auth("one@example.com", "workspace-1", "pro");
    auth.tokens.as_mut().expect("tokens").account_id = Some(String::new());

    let metadata = extract_chatgpt_metadata(&auth).expect("metadata should use the ID token claim");
    assert_eq!(metadata.workspace_account_id, "workspace-1");
}

#[test]
fn account_metadata_rejects_mismatched_workspace_ids() {
    let mut auth = chatgpt_auth("one@example.com", "workspace-claim", "pro");
    auth.tokens.as_mut().expect("tokens").account_id = Some("workspace-response".to_string());

    let err = extract_chatgpt_metadata(&auth).expect_err("mismatched IDs must be rejected");
    assert!(matches!(err, ChatgptAccountPoolError::AccountIdMismatch));
}

#[test]
fn account_metadata_rejects_unsafe_identity_values() {
    let mut auth = chatgpt_auth("one@example.com", "workspace-1", "pro");
    auth.tokens.as_mut().expect("tokens").id_token.email = Some(" one@example.com".to_string());
    let err = extract_chatgpt_metadata(&auth).expect_err("unsafe email must be rejected");
    assert!(matches!(
        err,
        ChatgptAccountPoolError::InvalidMetadata("ID token email")
    ));

    let mut auth = chatgpt_auth("one@example.com", "workspace-1", "pro");
    auth.pool_account_id = Some("pool-account\n".to_string());
    let err = extract_chatgpt_metadata(&auth).expect_err("unsafe pool ID must be rejected");
    assert!(matches!(
        err,
        ChatgptAccountPoolError::InvalidMetadata("pool account id")
    ));
}

#[tokio::test]
async fn register_account_rejects_rebinding_an_existing_pool_identity() {
    let codex_home = TempDir::new().expect("tempdir");
    let pool = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        None,
    )
    .await
    .expect("pool should open");
    let original = chatgpt_auth_with_member(
        "one@example.com",
        "workspace-1",
        "pro",
        "user-1",
        "pool-stable",
    );
    pool.register_account(&original)
        .await
        .expect("original account should register");

    let replacement = chatgpt_auth_with_member(
        "two@example.com",
        "workspace-1",
        "pro",
        "user-2",
        "pool-stable",
    );
    let err = pool
        .register_account(&replacement)
        .await
        .expect_err("a pool row must not be rebound to another member");
    assert!(matches!(
        err,
        ChatgptAccountPoolError::CredentialIdentityMismatch(ref id)
            if id == "pool-stable"
    ));

    let account = pool
        .list_accounts()
        .await
        .expect("accounts should load")
        .into_iter()
        .find(|account| account.account_id == "pool-stable")
        .expect("bound account should remain");
    assert_eq!(account.chatgpt_user_id.as_deref(), Some("user-1"));
    assert_eq!(account.email.as_deref(), Some("one@example.com"));
}

#[tokio::test]
async fn register_account_can_bind_a_preexisting_unbound_legacy_row() {
    let codex_home = TempDir::new().expect("tempdir");
    let pool = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        None,
    )
    .await
    .expect("pool should open");
    let mut legacy = chatgpt_auth("legacy@example.com", "workspace-1", "pro");
    legacy.pool_account_id = Some("pool-legacy".to_string());
    pool.register_account(&legacy)
        .await
        .expect("legacy account should register");

    let bound = chatgpt_auth_with_member(
        "bound@example.com",
        "workspace-1",
        "pro",
        "user-1",
        "pool-legacy",
    );
    pool.register_account(&bound)
        .await
        .expect("an unbound legacy row should accept its first member binding");

    let account = pool
        .list_accounts()
        .await
        .expect("accounts should load")
        .into_iter()
        .find(|account| account.account_id == "pool-legacy")
        .expect("bound account should remain");
    assert_eq!(
        account.member_identity_key.as_deref(),
        Some("chatgpt_user_id:user-1")
    );
    assert_eq!(account.chatgpt_user_id.as_deref(), Some("user-1"));
}

#[tokio::test]
async fn register_account_serializes_token_replacement_and_clears_superseded_recovery() {
    let codex_home = TempDir::new().expect("tempdir");
    let pool = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        None,
    )
    .await
    .expect("pool should open");
    let original = chatgpt_auth("one@example.com", "workspace-1", "pro");
    let original_refresh = original
        .tokens
        .as_ref()
        .expect("tokens")
        .refresh_token
        .clone();
    pool.register_account(&original)
        .await
        .expect("original account should register");

    let service_owner = "codex-accounts:test-refresh";
    assert!(
        pool.try_acquire_token_refresh_lock(
            "workspace-1",
            service_owner,
            ChatgptAccountPool::token_refresh_lock_ttl(),
        )
        .await
        .expect("service lock should be acquired")
    );
    let mut replacement = original.clone();
    replacement.tokens.as_mut().expect("tokens").refresh_token = "refresh-replacement".to_string();
    let digest = format!("{:x}", Sha256::digest(b"workspace-1"));
    let recovery_path = pool_root_dir(codex_home.path())
        .join(TOKEN_RECOVERY_DIRECTORY)
        .join(format!("{}.json", &digest[..16]));
    std::fs::create_dir_all(recovery_path.parent().expect("recovery directory"))
        .expect("recovery directory should be created");
    std::fs::write(&recovery_path, b"superseded pending rotation")
        .expect("test recovery journal should be written");

    let release_pool = pool.clone();
    let release_task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        release_pool
            .release_token_refresh_lock("workspace-1", service_owner)
            .await
    });
    let (_, superseded) = pool
        .register_account_with_superseded_refresh_token(&replacement)
        .await
        .expect("registration should wait for the active refresh lease");
    release_task
        .await
        .expect("release task should join")
        .expect("service lock should release");
    assert_eq!(superseded.as_deref(), Some(original_refresh.as_str()));
    assert!(
        !recovery_path.exists(),
        "a new interactive login must remove the prior rotation journal"
    );
    assert_eq!(
        pool.read_account_tokens("workspace-1")
            .await
            .expect("replacement should load")
            .expect("replacement should exist")
            .tokens
            .expect("replacement tokens")
            .refresh_token,
        "refresh-replacement"
    );
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
        individual_limit: None,
        plan_type: Some(AccountPlanType::Pro),
        rate_limit_reached_type: None,
        spend_control_reached: None,
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
        individual_limit: None,
        plan_type: Some(AccountPlanType::Pro),
        rate_limit_reached_type: None,
        spend_control_reached: None,
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
        individual_limit: None,
        plan_type: Some(AccountPlanType::Pro),
        rate_limit_reached_type: None,
        spend_control_reached: None,
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
async fn open_rejects_newer_schema_before_migrating() {
    let codex_home = TempDir::new().expect("tempdir");
    let pool = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        /*chatgpt_base_url*/ None,
    )
    .await
    .expect("pool should open");
    sqlx::query("UPDATE pool_state SET value = '4' WHERE key = 'schema_version'")
        .execute(&pool.pool)
        .await
        .expect("schema version should update");
    drop(pool);

    let err = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        /*chatgpt_base_url*/ None,
    )
    .await
    .expect_err("newer schema should be rejected");
    assert!(matches!(
        err,
        ChatgptAccountPoolError::IncompatibleSchemaVersion {
            ref found,
            supported: ACCOUNT_POOL_SCHEMA_VERSION,
        } if found == "4"
    ));
}

#[tokio::test]
async fn open_rejects_unknown_required_account_column_before_migrating() {
    let codex_home = TempDir::new().expect("tempdir");
    let raw_pool = open_raw_pool(codex_home.path()).await;
    sqlx::query(
        r#"
        CREATE TABLE pool_state (
            key TEXT PRIMARY KEY,
            value TEXT NULL
        )
        "#,
    )
    .execute(&raw_pool)
    .await
    .expect("pool_state should be created");
    sqlx::query("INSERT INTO pool_state (key, value) VALUES ('schema_version', '3')")
        .execute(&raw_pool)
        .await
        .expect("schema marker should be inserted");
    sqlx::query(
        r#"
        CREATE TABLE accounts (
            account_id TEXT PRIMARY KEY,
            future_required_value TEXT NOT NULL
        )
        "#,
    )
    .execute(&raw_pool)
    .await
    .expect("future accounts table should be created");
    raw_pool.close().await;

    let err = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        None,
    )
    .await
    .expect_err("unknown required column should be rejected");
    assert!(matches!(
        err,
        ChatgptAccountPoolError::IncompatibleAccountsTable { ref columns }
            if columns == "future_required_value"
    ));

    let raw_pool = open_raw_pool(codex_home.path()).await;
    let migrated_table_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'account_rate_limits'",
    )
    .fetch_one(&raw_pool)
    .await
    .expect("sqlite schema should remain readable");
    assert_eq!(
        migrated_table_count, 0,
        "compatibility rejection must happen before migrations"
    );
}

#[tokio::test]
async fn concurrent_open_serializes_legacy_schema_migrations() {
    let codex_home = TempDir::new().expect("tempdir");
    let seed_pool = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        None,
    )
    .await
    .expect("seed pool should open");
    seed_pool.pool.close().await;
    drop(seed_pool);

    let raw_pool = open_raw_pool(codex_home.path()).await;
    for statement in [
        "ALTER TABLE accounts DROP COLUMN agent_identity",
        "ALTER TABLE accounts DROP COLUMN id_token",
        "ALTER TABLE accounts DROP COLUMN refresh_token",
        "ALTER TABLE accounts DROP COLUMN access_token",
    ] {
        sqlx::query(statement)
            .execute(&raw_pool)
            .await
            .unwrap_or_else(|err| panic!("legacy token column should be removable: {err}"));
    }
    sqlx::query("UPDATE pool_state SET value = '2' WHERE key = 'schema_version'")
        .execute(&raw_pool)
        .await
        .expect("schema marker should be downgraded for the migration fixture");
    raw_pool.close().await;

    let home_a = codex_home.path().to_path_buf();
    let home_b = codex_home.path().to_path_buf();
    let home_c = codex_home.path().to_path_buf();
    let (pool_a, pool_b, pool_c) = tokio::join!(
        ChatgptAccountPool::open(home_a, AuthCredentialsStoreMode::File, None),
        ChatgptAccountPool::open(home_b, AuthCredentialsStoreMode::File, None),
        ChatgptAccountPool::open(home_c, AuthCredentialsStoreMode::File, None),
    );
    let pools = [
        pool_a.expect("first concurrent open should migrate"),
        pool_b.expect("second concurrent open should observe the migration"),
        pool_c.expect("third concurrent open should observe the migration"),
    ];

    let token_column_count: i64 = sqlx::query_scalar(
        r#"
        SELECT COUNT(*)
        FROM pragma_table_info('accounts')
        WHERE name IN ('access_token', 'refresh_token', 'id_token', 'agent_identity')
        "#,
    )
    .fetch_one(&pools[0].pool)
    .await
    .expect("migrated columns should be queryable");
    assert_eq!(token_column_count, 4);
}

#[tokio::test]
async fn open_allows_unknown_required_account_column_with_default() {
    let codex_home = TempDir::new().expect("tempdir");
    let seed_pool = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        None,
    )
    .await
    .expect("seed pool should open");
    sqlx::query(
        "ALTER TABLE accounts ADD COLUMN future_defaulted_value TEXT NOT NULL DEFAULT 'safe'",
    )
    .execute(&seed_pool.pool)
    .await
    .expect("forward-compatible column should be added");
    seed_pool.pool.close().await;
    drop(seed_pool);

    let reopened = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        None,
    )
    .await
    .expect("defaulted extension column should remain compatible");
    reopened
        .register_account(&chatgpt_auth(
            "defaulted@example.com",
            "workspace-defaulted",
            "pro",
        ))
        .await
        .expect("account insert should use the extension column default");
}

#[tokio::test]
async fn open_migrates_known_legacy_is_default_column() {
    let codex_home = TempDir::new().expect("tempdir");
    let seed_pool = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        None,
    )
    .await
    .expect("seed pool should open");
    sqlx::query("ALTER TABLE accounts ADD COLUMN is_default INTEGER NOT NULL DEFAULT 0")
        .execute(&seed_pool.pool)
        .await
        .expect("legacy column should be added");
    sqlx::query("UPDATE pool_state SET value = '1' WHERE key = 'schema_version'")
        .execute(&seed_pool.pool)
        .await
        .expect("legacy schema marker should be set");
    seed_pool.pool.close().await;
    drop(seed_pool);

    let reopened = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        None,
    )
    .await
    .expect("known legacy schema should migrate");
    let legacy_column_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pragma_table_info('accounts') WHERE name = 'is_default'",
    )
    .fetch_one(&reopened.pool)
    .await
    .expect("migrated schema should be queryable");
    assert_eq!(legacy_column_count, 0);
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
async fn record_account_activity_preserves_overlapping_turns_for_same_owner() {
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
        .expect("second account activity should record");

    assert_eq!(
        activity_rows(&pool).await,
        vec![
            ActivityRow {
                account_id: "workspace-1".to_string(),
                owner_pid: 1001,
                host: "host-a".to_string(),
                started_at: 1_000,
                heartbeat_at: 1_000,
                expires_at: 1_000 + ACCOUNT_ACTIVITY_TTL_SECONDS,
            },
            ActivityRow {
                account_id: "workspace-2".to_string(),
                owner_pid: 1001,
                host: "host-a".to_string(),
                started_at: 1_025,
                heartbeat_at: 1_025,
                expires_at: 1_025 + ACCOUNT_ACTIVITY_TTL_SECONDS,
            },
        ],
        "overlapping turns in one process must keep both accounts leased"
    );
}

#[tokio::test]
async fn record_account_activity_rolls_back_new_lease_when_account_update_fails() {
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
        .expect("initial owner activity should record");
    sqlx::query(
        r#"
        CREATE TRIGGER fail_second_account_activity_update
        BEFORE UPDATE OF last_used_at ON accounts
        WHEN NEW.account_id = 'workspace-2'
        BEGIN
            SELECT RAISE(ABORT, 'forced account activity failure');
        END
        "#,
    )
    .execute(&pool.pool)
    .await
    .expect("failure trigger should be installed");

    let err = pool
        .record_account_activity_for_owner_at("workspace-2", 1001, "host-a", 1_025)
        .await
        .expect_err("account timestamp failure should reject the new lease");
    assert!(matches!(err, ChatgptAccountPoolError::Sqlx(_)));
    assert_eq!(
        activity_rows(&pool).await,
        vec![ActivityRow {
            account_id: "workspace-1".to_string(),
            owner_pid: 1001,
            host: "host-a".to_string(),
            started_at: 1_000,
            heartbeat_at: 1_000,
            expires_at: 1_000 + ACCOUNT_ACTIVITY_TTL_SECONDS,
        }],
        "the previous lease must survive a failed move"
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
            AND name IN (
                'idx_account_activity_expires_at',
                'idx_account_events_account_time',
                'idx_account_events_type_time',
                'idx_usage_history_acct_time',
                'idx_usage_history_acct_time_id'
            )
        ORDER BY name
        "#,
    )
    .fetch_all(&pool.pool)
    .await
    .expect("usage history index should be queryable");
    assert_eq!(
        indexes,
        vec![
            "idx_account_activity_expires_at".to_string(),
            "idx_account_events_account_time".to_string(),
            "idx_account_events_type_time".to_string(),
            "idx_usage_history_acct_time".to_string(),
            "idx_usage_history_acct_time_id".to_string(),
        ]
    );
    let workspace_default: Option<String> = sqlx::query_scalar(
        "SELECT dflt_value FROM pragma_table_info('accounts') WHERE name = 'workspace_account_id'",
    )
    .fetch_one(&pool.pool)
    .await
    .expect("workspace account column should be queryable");
    assert_eq!(workspace_default.as_deref(), Some("''"));
    let v3_token_columns: i64 = sqlx::query_scalar(
        r#"
        SELECT COUNT(*)
        FROM pragma_table_info('accounts')
        WHERE name IN ('access_token', 'refresh_token', 'id_token', 'agent_identity')
          AND "notnull" = 0
        "#,
    )
    .fetch_one(&pool.pool)
    .await
    .expect("v3 token columns should be queryable");
    assert_eq!(v3_token_columns, 4);
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
async fn concurrent_first_account_registration_preserves_one_selection() {
    let codex_home = TempDir::new().expect("tempdir");
    let pool_a = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        None,
    )
    .await
    .expect("first pool should open");
    let pool_b = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        None,
    )
    .await
    .expect("second pool should open");
    let auth_a = chatgpt_auth("one@example.com", "workspace-1", "pro");
    let auth_b = chatgpt_auth("two@example.com", "workspace-2", "pro");

    let (registered_a, registered_b) = tokio::join!(
        pool_a.register_account(&auth_a),
        pool_b.register_account(&auth_b),
    );
    registered_a.expect("first concurrent registration should succeed");
    registered_b.expect("second concurrent registration should succeed");

    let selected = pool_a
        .selected_account_id()
        .await
        .expect("selection should load")
        .expect("one first account should be selected");
    assert!(
        selected == "workspace-1" || selected == "workspace-2",
        "selection should point at one of the concurrently registered accounts"
    );
    let accounts = pool_a.list_accounts().await.expect("accounts should list");
    assert_eq!(accounts.len(), 2);
    assert_eq!(
        accounts
            .iter()
            .filter(|account| account.is_selected)
            .count(),
        1
    );
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
        entry.fetched_at_by_limit_id,
        BTreeMap::from([(
            "codex".to_string(),
            entry.fetched_at.expect("fetch timestamp should be present"),
        )])
    );
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
    let initial = pool
        .record_fetched_rate_limits("workspace-1", &[first_codex.clone(), first_premium.clone()])
        .await
        .expect("initial fetch should persist");
    let initial_fetched_at = initial
        .fetched_at
        .expect("initial fetch timestamp should be present");
    assert_eq!(
        initial.fetched_at_by_limit_id,
        BTreeMap::from([
            ("codex".to_string(), initial_fetched_at),
            ("premium".to_string(), initial_fetched_at),
        ])
    );

    let updated_codex = codex_snapshot(26.0);
    let updated_fetched_at = initial_fetched_at + 1;
    pool.store_rate_limit_snapshot("workspace-1", &updated_codex, updated_fetched_at)
        .await
        .expect("single snapshot observation should persist");

    let latest = pool
        .list_rate_limits()
        .await
        .expect("rate limits should load");
    assert_eq!(latest.len(), 1);
    assert_eq!(latest[0].account_id, "workspace-1");
    assert_eq!(latest[0].fetched_at, Some(updated_fetched_at));
    assert_eq!(
        latest[0].fetched_at_by_limit_id,
        BTreeMap::from([
            ("codex".to_string(), updated_fetched_at),
            ("premium".to_string(), initial_fetched_at),
        ])
    );
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
async fn record_rate_limit_snapshot_rolls_back_latest_when_history_write_fails() {
    let codex_home = TempDir::new().expect("tempdir");
    let pool = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        None,
    )
    .await
    .expect("pool should open");
    pool.register_account(&chatgpt_auth(
        "rollback@example.com",
        "workspace-rollback",
        "pro",
    ))
    .await
    .expect("account should register");
    sqlx::query(
        r#"
        CREATE TRIGGER fail_usage_history_insert
        BEFORE INSERT ON account_usage_history
        BEGIN
            SELECT RAISE(ABORT, 'forced history failure');
        END
        "#,
    )
    .execute(&pool.pool)
    .await
    .expect("failure trigger should be installed");

    let err = pool
        .record_rate_limit_snapshot("workspace-rollback", &codex_snapshot(100.0))
        .await
        .expect_err("history failure should reject the snapshot");
    assert!(matches!(err, ChatgptAccountPoolError::Sqlx(_)));

    let latest_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM account_rate_limits WHERE account_id = 'workspace-rollback'",
    )
    .fetch_one(&pool.pool)
    .await
    .expect("latest snapshot count should load");
    assert_eq!(
        latest_count, 0,
        "latest and history writes must commit or roll back together"
    );
}

#[tokio::test]
async fn record_fetched_rate_limits_rolls_back_usage_when_cooldown_event_fails() {
    let codex_home = TempDir::new().expect("tempdir");
    let pool = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        None,
    )
    .await
    .expect("pool should open");
    pool.register_account(&chatgpt_auth(
        "rollback@example.com",
        "workspace-rollback",
        "pro",
    ))
    .await
    .expect("account should register");
    sqlx::query(
        r#"
        CREATE TRIGGER fail_cooldown_started_event
        BEFORE INSERT ON account_events
        WHEN NEW.event_type = 'account_cooldown_started'
        BEGIN
            SELECT RAISE(ABORT, 'forced cooldown event failure');
        END
        "#,
    )
    .execute(&pool.pool)
    .await
    .expect("failure trigger should be installed");
    let now = now_ts();
    let exhausted = RateLimitSnapshot {
        primary: Some(RateLimitWindow {
            used_percent: 100.0,
            window_minutes: Some(300),
            resets_at: Some(now + 3_600),
        }),
        secondary: None,
        rate_limit_reached_type: Some(RateLimitReachedType::RateLimitReached),
        ..codex_snapshot(100.0)
    };

    pool.record_fetched_rate_limits("workspace-rollback", &[exhausted])
        .await
        .expect_err("cooldown event failure should reject the usage update");

    let latest_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM account_rate_limits WHERE account_id = 'workspace-rollback'",
    )
    .fetch_one(&pool.pool)
    .await
    .expect("latest count should load");
    let history_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM account_usage_history WHERE account_id = 'workspace-rollback'",
    )
    .fetch_one(&pool.pool)
    .await
    .expect("history count should load");
    let cooldown_until: Option<i64> = sqlx::query_scalar(
        "SELECT cooldown_until FROM accounts WHERE account_id = 'workspace-rollback'",
    )
    .fetch_one(&pool.pool)
    .await
    .expect("cooldown should load");
    assert_eq!(latest_count, 0);
    assert_eq!(history_count, 0);
    assert_eq!(cooldown_until, None);
}

#[tokio::test]
async fn mark_current_account_rate_limited_non_codex_snapshot_does_not_cool_down() {
    // Regression guard for the "innocent active account marked rate-limited" bug:
    // the CLI receives a 429 whose active limit is non-codex (e.g. "premium" credits
    // depleted). The codex quota — the only one the codex CLI consumes — is fine, so
    // the account must NOT be cooled down or marked rate_limited; otherwise a healthy
    // account is pulled out of rotation and failover cascades through the pool. The
    // snapshot is still persisted for visibility, and a non-cooldown event is emitted.
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
    // Promote to valid so the eligibility assertion below is meaningful.
    sqlx::query("UPDATE accounts SET auth_status = 'valid' WHERE account_id = ?")
        .bind("workspace-1")
        .execute(&pool.pool)
        .await
        .expect("set valid");

    pool.mark_current_account_rate_limited(
        "workspace-1",
        Some(&RateLimitSnapshot {
            limit_id: Some("premium".to_string()),
            limit_name: None,
            // No primary/secondary window data — mirrors the real premium snapshot
            // that came back when credits were depleted.
            primary: None,
            secondary: None,
            credits: Some(codex_protocol::protocol::CreditsSnapshot {
                has_credits: false,
                unlimited: false,
                balance: Some("0".to_string()),
            }),
            individual_limit: None,
            plan_type: None,
            rate_limit_reached_type: None,
            spend_control_reached: None,
        }),
        // Even with a non-codex resets_at present, the account must not be cooled down:
        // that reset is for the premium limit, not codex.
        Some(Utc::now() + chrono::Duration::hours(2)),
    )
    .await
    .expect("mark should succeed");

    let account = pool
        .list_accounts()
        .await
        .expect("accounts should list")
        .into_iter()
        .find(|a| a.account_id == "workspace-1")
        .expect("workspace-1 should remain");

    assert_eq!(
        account.cooldown_until, None,
        "a non-codex usage limit must not set any cooldown (codex quota is fine)"
    );
    assert!(
        is_account_eligible(&account, now_ts()),
        "the account must remain eligible for selection after a non-codex usage limit"
    );

    // The snapshot is still stored for visibility, and the event reflects non-cooldown.
    let events = pool.list_events(None).await.expect("events should list");
    assert!(
        events
            .iter()
            .any(|e| e.event_type == "rate_limit_reached_non_codex"),
        "a non-cooldown 'rate_limit_reached_non_codex' event should be recorded"
    );
    assert!(
        !events.iter().any(|e| e.event_type == "rate_limit_reached"),
        "no cooldown 'rate_limit_reached' event should be recorded for a non-codex limit"
    );
}

#[tokio::test]
async fn mark_current_account_rate_limited_codex_snapshot_uses_long_fallback() {
    // Scenario 4: codex limit with no window data and no resets_at → 1-hour fallback.
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

    let before = now_ts();
    pool.mark_current_account_rate_limited(
        "workspace-1",
        Some(&RateLimitSnapshot {
            limit_id: Some("codex".to_string()),
            limit_name: None,
            primary: None,
            secondary: None,
            credits: None,
            individual_limit: None,
            plan_type: None,
            rate_limit_reached_type: None,
            spend_control_reached: None,
        }),
        None,
    )
    .await
    .expect("mark should succeed");

    let account = pool
        .list_accounts()
        .await
        .expect("accounts should list")
        .into_iter()
        .find(|a| a.account_id == "workspace-1")
        .expect("workspace-1 should remain");

    let cooldown = account
        .cooldown_until
        .expect("cooldown_until should be set");
    // Must use the full 1-hour conservative fallback.
    assert!(
        cooldown >= before + 3600 - 5,
        "expected 1-hour fallback for codex rate limit, got cooldown_until={cooldown} before={before}"
    );
}

#[tokio::test]
async fn mark_current_account_rate_limited_no_snapshot_uses_long_fallback() {
    // Scenario 5: no snapshot at all and no resets_at → 1-hour fallback (unknown bucket).
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

    let before = now_ts();
    pool.mark_current_account_rate_limited("workspace-1", None, None)
        .await
        .expect("mark should succeed");

    let account = pool
        .list_accounts()
        .await
        .expect("accounts should list")
        .into_iter()
        .find(|a| a.account_id == "workspace-1")
        .expect("workspace-1 should remain");

    let cooldown = account
        .cooldown_until
        .expect("cooldown_until should be set");
    assert!(
        cooldown >= before + 3600 - 5,
        "expected 1-hour fallback for unknown rate limit bucket, got cooldown_until={cooldown} before={before}"
    );
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
            individual_limit: None,
            plan_type: None,
            rate_limit_reached_type: Some(RateLimitReachedType::RateLimitReached),
            spend_control_reached: None,
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
async fn clear_rate_limit_cooldown_makes_reset_account_eligible() {
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
        /*snapshot*/ None,
        Some(Utc::now() + chrono::Duration::hours(1)),
    )
    .await
    .expect("marking limit should succeed");

    pool.clear_rate_limit_cooldown("workspace-1")
        .await
        .expect("clearing cooldown should succeed");

    let account = pool
        .list_accounts()
        .await
        .expect("accounts should load")
        .into_iter()
        .find(|account| account.account_id == "workspace-1")
        .expect("workspace-1 should remain in pool");
    assert_eq!(account.cooldown_until, None);
    assert_eq!(account.cooldown_reason, None);
    assert!(matches!(
        pool.resolve_turn_selection(
            Some("workspace-1"),
            /*current_refresh_failed_permanently*/ false,
        )
        .await
        .expect("selection should succeed"),
        ChatgptAccountPoolSelectionOutcome::Unchanged
    ));
}

#[tokio::test]
async fn clear_rate_limit_cooldown_rolls_back_when_event_write_fails() {
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
        None,
        Some(Utc::now() + chrono::Duration::hours(1)),
    )
    .await
    .expect("cooldown should be recorded");
    let cooldown_before: i64 =
        sqlx::query_scalar("SELECT cooldown_until FROM accounts WHERE account_id = 'workspace-1'")
            .fetch_one(&pool.pool)
            .await
            .expect("cooldown should load");
    sqlx::query(
        r#"
        CREATE TRIGGER fail_cooldown_cleared_event
        BEFORE INSERT ON account_events
        WHEN NEW.event_type = 'account_cooldown_cleared'
        BEGIN
            SELECT RAISE(ABORT, 'forced cooldown event failure');
        END
        "#,
    )
    .execute(&pool.pool)
    .await
    .expect("failure trigger should be installed");

    pool.clear_rate_limit_cooldown("workspace-1")
        .await
        .expect_err("event failure should reject cooldown clearing");
    let cooldown_after: i64 =
        sqlx::query_scalar("SELECT cooldown_until FROM accounts WHERE account_id = 'workspace-1'")
            .fetch_one(&pool.pool)
            .await
            .expect("cooldown should remain readable");
    assert_eq!(cooldown_after, cooldown_before);
}

#[tokio::test]
async fn disable_all_accounts_rolls_back_when_selection_clear_fails() {
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
    sqlx::query(
        r#"
        CREATE TRIGGER fail_selected_account_clear
        BEFORE UPDATE OF value ON pool_state
        WHEN OLD.key = 'selected_account_id'
        BEGIN
            SELECT RAISE(ABORT, 'forced selection clear failure');
        END
        "#,
    )
    .execute(&pool.pool)
    .await
    .expect("failure trigger should be installed");

    pool.disable_all_accounts_for_logout()
        .await
        .expect_err("selection failure should reject logout persistence");
    let enabled: i64 =
        sqlx::query_scalar("SELECT enabled FROM accounts WHERE account_id = 'workspace-1'")
            .fetch_one(&pool.pool)
            .await
            .expect("enabled flag should load");
    assert_eq!(enabled, 1, "account disabling must roll back");
    assert_eq!(
        pool.selected_account_id()
            .await
            .expect("selection should load")
            .as_deref(),
        Some("workspace-1")
    );
}

#[tokio::test]
async fn logout_revocation_outcome_matches_the_stored_credential_status() {
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
    pool.register_account(&chatgpt_auth("two@example.com", "workspace-2", "plus"))
        .await
        .expect("second account should register");
    pool.disable_all_accounts_for_logout()
        .await
        .expect("logout disable should commit");

    pool.record_logout_credential_revocation("workspace-1", None)
        .await
        .expect("successful revocation should record");
    pool.record_logout_credential_revocation(
        "workspace-2",
        Some("revocation endpoint echoed sentinel-refresh-token"),
    )
    .await
    .expect("failed revocation should record");

    let accounts = pool.list_accounts().await.expect("accounts should load");
    let first = accounts
        .iter()
        .find(|account| account.account_id == "workspace-1")
        .expect("first account should exist");
    let second = accounts
        .iter()
        .find(|account| account.account_id == "workspace-2")
        .expect("second account should exist");
    assert!(!first.enabled && !second.enabled);
    assert_eq!(first.auth_status, ChatgptAccountPoolAuthStatus::Invalid);
    assert_eq!(second.auth_status, ChatgptAccountPoolAuthStatus::Valid);
    let events = pool.list_events(None).await.expect("events should load");
    assert!(events.iter().any(|event| {
        event.account_id.as_deref() == Some("workspace-1")
            && event.event_type == "logout_credential_revoked"
    }));
    assert!(events.iter().any(|event| {
        event.account_id.as_deref() == Some("workspace-2")
            && event.event_type == "logout_credential_revoke_failed"
            && event.message
                == "Pool sign-out disabled this account, but OAuth refresh-token revocation failed"
    }));
    assert!(
        events
            .iter()
            .all(|event| !event.message.contains("sentinel-refresh-token")),
        "revocation event leaked arbitrary upstream error text"
    );
}

#[tokio::test]
async fn superseded_revocation_event_does_not_persist_arbitrary_error_text() {
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

    pool.record_superseded_credential_revocation(
        "workspace-1",
        Some("revocation endpoint echoed sentinel-refresh-token"),
    )
    .await
    .expect("failed revocation should record");

    let events = pool.list_events(None).await.expect("events should load");
    let event = events
        .iter()
        .find(|event| event.event_type == "superseded_credential_revoke_failed")
        .expect("revocation failure event should exist");
    assert_eq!(
        event.message,
        "Replacement credential committed, but superseded OAuth refresh-token revocation failed"
    );
    assert!(!event.message.contains("sentinel-refresh-token"));
}

#[tokio::test]
async fn mark_account_auth_failed_rolls_back_when_event_write_fails() {
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
    sqlx::query(
        r#"
        CREATE TRIGGER fail_permanent_auth_event
        BEFORE INSERT ON account_events
        WHEN NEW.event_type = 'auth_failure_permanent'
        BEGIN
            SELECT RAISE(ABORT, 'forced auth event failure');
        END
        "#,
    )
    .execute(&pool.pool)
    .await
    .expect("failure trigger should be installed");

    pool.mark_account_auth_failed("workspace-1", "forced test failure")
        .await
        .expect_err("event failure should reject the auth transition");
    assert_eq!(
        pool.read_account_auth_status("workspace-1")
            .await
            .expect("status should load"),
        Some(ChatgptAccountPoolAuthStatus::Valid)
    );
}

#[tokio::test]
async fn stale_permanent_auth_failure_does_not_invalidate_rotated_credential() {
    let codex_home = TempDir::new().expect("tempdir");
    let pool = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        None,
    )
    .await
    .expect("pool should open");
    let stale_auth = chatgpt_auth("one@example.com", "workspace-1", "pro");
    pool.register_account(&stale_auth)
        .await
        .expect("account should register");
    let mut repaired_auth = stale_auth.clone();
    let repaired_tokens = repaired_auth.tokens.as_mut().expect("tokens should exist");
    repaired_tokens.access_token = "rotated-access".to_string();
    repaired_tokens.refresh_token = "rotated-refresh".to_string();
    pool.write_account_tokens("workspace-1", &repaired_auth)
        .await
        .expect("rotated credential should persist");

    let applied = pool
        .mark_account_auth_failed_if_credential_matches(
            "workspace-1",
            &stale_auth,
            "stale request failed",
        )
        .await
        .expect("stale failure should be safely ignored");

    assert!(!applied);
    assert_eq!(
        pool.read_account_auth_status("workspace-1")
            .await
            .expect("status should load"),
        Some(ChatgptAccountPoolAuthStatus::Valid)
    );
    assert!(
        pool.list_events(None)
            .await
            .expect("events should load")
            .iter()
            .all(|event| event.event_type != "auth_failure_permanent"),
        "a stale failure must not emit a misleading permanent-failure event"
    );
}

#[tokio::test]
async fn stale_retryable_auth_failure_does_not_cool_down_rotated_credential() {
    let codex_home = TempDir::new().expect("tempdir");
    let pool = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        None,
    )
    .await
    .expect("pool should open");
    let stale_auth = chatgpt_auth("one@example.com", "workspace-1", "pro");
    pool.register_account(&stale_auth)
        .await
        .expect("account should register");
    let mut repaired_auth = stale_auth.clone();
    let repaired_tokens = repaired_auth.tokens.as_mut().expect("tokens should exist");
    repaired_tokens.access_token = "rotated-access".to_string();
    repaired_tokens.refresh_token = "rotated-refresh".to_string();
    pool.write_account_tokens("workspace-1", &repaired_auth)
        .await
        .expect("rotated credential should persist");

    let applied = pool
        .mark_account_auth_retryable_if_credential_matches(
            "workspace-1",
            &stale_auth,
            "stale request failed",
        )
        .await
        .expect("stale failure should be safely ignored");

    assert!(!applied);
    let account = pool
        .list_accounts()
        .await
        .expect("accounts should load")
        .into_iter()
        .find(|account| account.account_id == "workspace-1")
        .expect("account should exist");
    assert_eq!(account.auth_status, ChatgptAccountPoolAuthStatus::Valid);
    assert_eq!(account.cooldown_until, None);
    assert_eq!(account.cooldown_reason, None);
}

#[tokio::test]
async fn record_validated_rate_limits_promotes_without_cooldown_for_exhausted_fresh_account() {
    // Regression guard for the "fresh failover account also marked rate-limited" bug:
    // bringing a pending account online during failover probes its /usage. Even if
    // that probe's codex window comes back exhausted, validate-on-pickup must NOT cool
    // the account — it is the account we are switching *to*, not the one that hit the
    // limit. The snapshot is still persisted (so the scorer can deprioritize it) and
    // the account is promoted to valid; a real 429 on a turn is what cools it.
    let codex_home = TempDir::new().expect("tempdir");
    let pool = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        None,
    )
    .await
    .expect("pool should open");
    pool.register_account(&chatgpt_auth("fresh@example.com", "workspace-fresh", "pro"))
        .await
        .expect("account should register");
    set_pending_validation(&pool, "workspace-fresh").await;

    let exhausted = RateLimitSnapshot {
        limit_id: Some("codex".to_string()),
        limit_name: None,
        primary: Some(RateLimitWindow {
            used_percent: 100.0,
            window_minutes: Some(300),
            resets_at: Some(now_ts() + 3_600),
        }),
        secondary: None,
        credits: None,
        individual_limit: None,
        plan_type: Some(AccountPlanType::Pro),
        rate_limit_reached_type: Some(RateLimitReachedType::RateLimitReached),
        spend_control_reached: None,
    };
    pool.record_validated_rate_limits("workspace-fresh", std::slice::from_ref(&exhausted))
        .await
        .expect("validation snapshot should persist");

    let account = pool
        .list_accounts()
        .await
        .expect("accounts should load")
        .into_iter()
        .find(|account| account.account_id == "workspace-fresh")
        .expect("workspace-fresh should remain in pool");
    assert_eq!(
        account.cooldown_until, None,
        "validate-on-pickup must not cool the fresh failover candidate"
    );
    assert_eq!(account.cooldown_reason, None);
    assert_eq!(
        account.auth_status,
        ChatgptAccountPoolAuthStatus::Valid,
        "validate-on-pickup should still promote pending_validation -> valid"
    );
    assert_eq!(
        account.rate_limits,
        BTreeMap::from([("codex".to_string(), exhausted)]),
        "the usage snapshot should still be stored for the capacity scorer"
    );
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
async fn record_fetched_rate_limits_does_not_cool_an_allowed_hundred_percent_window() {
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

    // `/usage` mapping supplies a reached type whenever `allowed` is false or
    // `limit_reached` is true. Its absence therefore means a percentage alone
    // must not make background polling remove an otherwise usable account.
    let snapshot = RateLimitSnapshot {
        primary: Some(RateLimitWindow {
            used_percent: 100.0,
            window_minutes: Some(300),
            resets_at: Some(now_ts() + 3_600),
        }),
        secondary: None,
        rate_limit_reached_type: None,
        ..codex_snapshot(100.0)
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
    assert_eq!(account.cooldown_until, None);
    assert_eq!(account.cooldown_reason, None);
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
        individual_limit: None,
        plan_type: Some(AccountPlanType::Pro),
        rate_limit_reached_type: Some(RateLimitReachedType::RateLimitReached),
        spend_control_reached: None,
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
async fn record_fetched_rate_limits_uses_spend_control_billing_reset_for_cooldown() {
    let codex_home = TempDir::new().expect("tempdir");
    let pool = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        None,
    )
    .await
    .expect("pool should open");
    pool.register_account(&chatgpt_auth(
        "business@example.com",
        "workspace-business",
        "business",
    ))
    .await
    .expect("account should register");

    let billing_reset_at = now_ts() + 30 * 24 * 60 * 60;
    let snapshot = RateLimitSnapshot {
        limit_id: Some("codex".to_string()),
        limit_name: None,
        primary: None,
        secondary: None,
        credits: None,
        individual_limit: Some(SpendControlLimitSnapshot {
            limit: "25000".to_string(),
            used: "25000".to_string(),
            remaining_percent: 0,
            resets_at: billing_reset_at,
        }),
        plan_type: Some(AccountPlanType::Business),
        rate_limit_reached_type: Some(RateLimitReachedType::WorkspaceMemberUsageLimitReached),
        spend_control_reached: Some(true),
    };

    pool.record_fetched_rate_limits("workspace-business", &[snapshot])
        .await
        .expect("fetch should persist");

    let account = pool
        .list_accounts()
        .await
        .expect("accounts should load")
        .into_iter()
        .find(|account| account.account_id == "workspace-business")
        .expect("workspace-business should remain in pool");
    assert_eq!(account.cooldown_until, Some(billing_reset_at));
    assert_eq!(
        account.cooldown_reason.as_deref(),
        Some("rate_limits_refreshed")
    );
}

#[tokio::test]
async fn migrate_legacy_auth_into_pool_on_open() {
    let codex_home = TempDir::new().expect("tempdir");
    save_auth(
        codex_home.path(),
        &chatgpt_auth("legacy@example.com", "workspace-legacy", "plus"),
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
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
async fn migrate_legacy_auth_repairs_compatible_incomplete_pool_copy() {
    let codex_home = TempDir::new().expect("tempdir");
    let legacy_auth = chatgpt_auth("legacy@example.com", "workspace-legacy", "plus");
    save_auth(
        codex_home.path(),
        &legacy_auth,
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
    )
    .expect("legacy auth should save");
    let pool = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        None,
    )
    .await
    .expect("pool should open");
    sqlx::query(
        "UPDATE accounts SET access_token = NULL, refresh_token = NULL WHERE account_id = ?",
    )
    .bind("workspace-legacy")
    .execute(&pool.pool)
    .await
    .expect("pool credential should be made incomplete");
    drop(pool);

    let repaired = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        None,
    )
    .await
    .expect("pool should repair from the preserved source");
    let repaired_auth = repaired
        .read_account_tokens("workspace-legacy")
        .await
        .expect("repaired credential should load")
        .expect("repaired credential should exist");
    assert_eq!(repaired_auth.tokens, legacy_auth.tokens);
}

#[tokio::test]
async fn migrate_legacy_auth_rejects_rebinding_an_incomplete_pool_row() {
    let codex_home = TempDir::new().expect("tempdir");
    let pool = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        None,
    )
    .await
    .expect("pool should open");
    pool.register_account(&chatgpt_auth_with_member(
        "bound@example.com",
        "workspace-legacy",
        "plus",
        "user-1",
        "pool-stable",
    ))
    .await
    .expect("bound account should register");
    sqlx::query(
        "UPDATE accounts SET access_token = NULL, refresh_token = NULL WHERE account_id = ?",
    )
    .bind("pool-stable")
    .execute(&pool.pool)
    .await
    .expect("pool credential should be made incomplete");
    drop(pool);

    save_auth(
        codex_home.path(),
        &chatgpt_auth_with_member(
            "other@example.com",
            "workspace-legacy",
            "plus",
            "user-2",
            "pool-stable",
        ),
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
    )
    .expect("conflicting legacy auth should save");

    let err = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        None,
    )
    .await
    .expect_err("legacy migration must not rebind an existing pool row");
    assert!(matches!(
        err,
        ChatgptAccountPoolError::CredentialIdentityMismatch(ref id)
            if id == "pool-stable"
    ));
}

#[tokio::test]
async fn migrate_legacy_auth_does_not_overwrite_different_complete_pool_copy() {
    let codex_home = TempDir::new().expect("tempdir");
    let legacy_auth = chatgpt_auth("legacy@example.com", "workspace-legacy", "plus");
    save_auth(
        codex_home.path(),
        &legacy_auth,
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
    )
    .expect("legacy auth should save");
    let pool = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        None,
    )
    .await
    .expect("pool should open");
    sqlx::query(
        "UPDATE accounts SET access_token = 'newer-access', refresh_token = 'newer-refresh' WHERE account_id = ?",
    )
    .bind("workspace-legacy")
    .execute(&pool.pool)
    .await
    .expect("pool credential should be replaced");
    drop(pool);

    let reopened = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        None,
    )
    .await
    .expect("pool should reopen");
    let preserved = reopened
        .read_account_tokens("workspace-legacy")
        .await
        .expect("pool credential should load")
        .expect("pool credential should exist")
        .tokens
        .expect("pool tokens should exist");
    assert_eq!(preserved.access_token, "newer-access");
    assert_eq!(preserved.refresh_token, "newer-refresh");
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
async fn account_selection_and_audit_event_roll_back_together() {
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
    sqlx::query(
        r#"
        CREATE TRIGGER fail_account_selected_event
        BEFORE INSERT ON account_events
        WHEN NEW.event_type = 'account_selected'
        BEGIN
            SELECT RAISE(ABORT, 'forced selection event failure');
        END
        "#,
    )
    .execute(&pool.pool)
    .await
    .expect("failure trigger should be installed");

    pool.select_account("workspace-2")
        .await
        .expect_err("event failure should reject selection");

    assert_eq!(
        pool.selected_account_id()
            .await
            .expect("selection should load")
            .as_deref(),
        Some("workspace-1"),
        "a failed audit write must not leave a hidden selection change"
    );
}

#[tokio::test]
async fn stale_selection_compare_and_set_follows_the_first_committed_writer() {
    let codex_home = TempDir::new().expect("tempdir");
    let pool = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        None,
    )
    .await
    .expect("pool should open");
    for (email, account_id) in [
        ("one@example.com", "workspace-1"),
        ("two@example.com", "workspace-2"),
        ("three@example.com", "workspace-3"),
    ] {
        pool.register_account(&chatgpt_auth(email, account_id, "pro"))
            .await
            .expect("account should register");
    }

    assert!(
        pool.compare_and_set_selected_account_id_with_event(
            Some("workspace-1"),
            "workspace-2",
            "account_failover_selected",
            "first writer".to_string(),
        )
        .await
        .expect("first selection should commit")
    );
    assert!(
        !pool
            .compare_and_set_selected_account_id_with_event(
                Some("workspace-1"),
                "workspace-3",
                "account_failover_selected",
                "stale writer".to_string(),
            )
            .await
            .expect("stale selection should be rejected")
    );
    assert_eq!(
        pool.selected_account_id()
            .await
            .expect("selection should load")
            .as_deref(),
        Some("workspace-2")
    );
    assert!(
        pool.list_events(None)
            .await
            .expect("events should load")
            .iter()
            .all(|event| event.message != "stale writer"),
        "a rejected compare-and-set must not emit an audit event"
    );
}

#[tokio::test]
async fn resolve_turn_selection_does_not_rebalance_selected_account_at_95_percent() {
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

    pool.record_fetched_rate_limits("workspace-1", &[codex_snapshot(95.0)])
        .await
        .expect("current account usage should persist");
    pool.record_fetched_rate_limits("workspace-2", &[codex_snapshot(0.0)])
        .await
        .expect("idle account usage should persist");

    let selection = pool
        .resolve_turn_selection(Some("workspace-1"), false)
        .await
        .expect("selection should succeed");
    assert_eq!(selection, ChatgptAccountPoolSelectionOutcome::Unchanged);
    assert_eq!(
        pool.selected_account_id()
            .await
            .expect("selected account should load")
            .as_deref(),
        Some("workspace-1"),
        "capacity scoring must not proactively rotate a healthy selected account"
    );
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

    let mut missing_secret_auth = pool
        .read_account_tokens("workspace-1")
        .await
        .expect("selected account auth should load")
        .expect("selected account auth should exist");
    missing_secret_auth
        .tokens
        .as_mut()
        .expect("tokens should exist")
        .access_token
        .clear();
    pool.write_account_tokens("workspace-1", &missing_secret_auth)
        .await
        .expect("selected account secret should be clearable");

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
            individual_limit: None,
            plan_type: None,
            rate_limit_reached_type: Some(RateLimitReachedType::RateLimitReached),
            spend_control_reached: None,
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
            individual_limit: None,
            plan_type: None,
            rate_limit_reached_type: Some(RateLimitReachedType::RateLimitReached),
            spend_control_reached: None,
        }),
        None,
    )
    .await
    .expect("marking limit should succeed");
    let mut missing_secret_auth = pool
        .read_account_tokens("workspace-2")
        .await
        .expect("best fallback auth should load")
        .expect("best fallback auth should exist");
    missing_secret_auth
        .tokens
        .as_mut()
        .expect("tokens should exist")
        .access_token
        .clear();
    pool.write_account_tokens("workspace-2", &missing_secret_auth)
        .await
        .expect("best fallback secret should be clearable");

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

#[test]
fn capacity_score_ignores_auxiliary_limits_when_codex_usage_is_unknown() {
    let mut account = pending_account("workspace-auxiliary", true, None);
    account.auth_status = ChatgptAccountPoolAuthStatus::Valid;
    account.rate_limits.insert(
        "premium".to_string(),
        RateLimitSnapshot {
            limit_id: Some("premium".to_string()),
            limit_name: Some("Premium".to_string()),
            primary: Some(RateLimitWindow {
                used_percent: 100.0,
                window_minutes: Some(300),
                resets_at: Some(2_000),
            }),
            secondary: None,
            credits: None,
            individual_limit: None,
            plan_type: Some(AccountPlanType::Pro),
            rate_limit_reached_type: None,
            spend_control_reached: None,
        },
    );

    assert_eq!(
        capacity_score(&account, 1_000),
        (false, 100),
        "a named non-codex bucket must not drive account selection"
    );
}

#[test]
fn capacity_score_treats_metadata_only_codex_snapshot_as_unknown() {
    let mut account = pending_account("workspace-metadata", true, None);
    account.auth_status = ChatgptAccountPoolAuthStatus::Valid;
    account.rate_limits.insert(
        "codex".to_string(),
        RateLimitSnapshot {
            limit_id: Some("codex".to_string()),
            limit_name: None,
            primary: None,
            secondary: None,
            credits: None,
            individual_limit: None,
            plan_type: Some(AccountPlanType::Pro),
            rate_limit_reached_type: None,
            spend_control_reached: Some(false),
        },
    );

    assert_eq!(capacity_score(&account, 1_000), (false, 100));
}

#[test]
fn remaining_percent_includes_current_individual_spend_limit() {
    let snapshot = RateLimitSnapshot {
        limit_id: Some("codex".to_string()),
        limit_name: None,
        primary: Some(RateLimitWindow {
            used_percent: 20.0,
            window_minutes: Some(300),
            resets_at: Some(4_000),
        }),
        secondary: None,
        credits: None,
        individual_limit: Some(SpendControlLimitSnapshot {
            limit: "100".to_string(),
            used: "95".to_string(),
            remaining_percent: 5,
            resets_at: 3_000,
        }),
        plan_type: Some(AccountPlanType::Business),
        rate_limit_reached_type: None,
        spend_control_reached: Some(false),
    };

    assert_eq!(remaining_percent(&snapshot, false, 1_000), Some(5));
    assert_eq!(
        remaining_percent(&snapshot, false, 3_000),
        Some(80),
        "a completed billing-cycle reset must stop penalizing account capacity"
    );
}

#[test]
fn cooldown_uses_monthly_spend_reset_and_fallback_for_unknown_windows() {
    let monthly_reset = 50_000;
    let monthly = RateLimitSnapshot {
        limit_id: Some("codex".to_string()),
        limit_name: None,
        primary: None,
        secondary: None,
        credits: None,
        individual_limit: Some(SpendControlLimitSnapshot {
            limit: "100".to_string(),
            used: "100".to_string(),
            remaining_percent: 0,
            resets_at: monthly_reset,
        }),
        plan_type: Some(AccountPlanType::Business),
        rate_limit_reached_type: Some(RateLimitReachedType::WorkspaceMemberUsageLimitReached),
        spend_control_reached: Some(true),
    };
    assert_eq!(
        cooldown_until_from_snapshot(&monthly, 10_000, false),
        Some(monthly_reset)
    );

    let rolling_with_unknown_secondary = RateLimitSnapshot {
        primary: Some(RateLimitWindow {
            used_percent: 100.0,
            window_minutes: Some(300),
            resets_at: Some(10_600),
        }),
        secondary: Some(RateLimitWindow {
            used_percent: 100.0,
            window_minutes: Some(10_080),
            resets_at: None,
        }),
        individual_limit: None,
        rate_limit_reached_type: Some(RateLimitReachedType::RateLimitReached),
        spend_control_reached: None,
        ..monthly
    };
    assert_eq!(
        cooldown_until_from_snapshot(&rolling_with_unknown_secondary, 10_000, false),
        Some(13_600),
        "an exhausted window without a reset must retain the conservative fallback"
    );
}

#[test]
fn request_rejection_corroborates_cached_exhausted_codex_window() {
    let now = now_ts();
    let reset_at = now + 3_600;
    let snapshot = RateLimitSnapshot {
        limit_id: Some("codex".to_string()),
        limit_name: None,
        primary: Some(RateLimitWindow {
            used_percent: 100.0,
            window_minutes: Some(10_080),
            resets_at: Some(reset_at),
        }),
        secondary: None,
        credits: None,
        individual_limit: None,
        plan_type: Some(AccountPlanType::Team),
        rate_limit_reached_type: None,
        spend_control_reached: None,
    };

    assert_eq!(
        cooldown_until_from_snapshot(&snapshot, now, false),
        None,
        "a background sample at 100% is not authoritative by itself"
    );
    assert_eq!(
        corroborated_request_cooldown_until(&snapshot),
        Some(reset_at),
        "a rejected auxiliary fallback makes the cached Codex reset authoritative"
    );
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
            individual_limit: None,
            plan_type: None,
            rate_limit_reached_type: Some(RateLimitReachedType::RateLimitReached),
            spend_control_reached: None,
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
            individual_limit: None,
            plan_type: None,
            rate_limit_reached_type: None,
            spend_control_reached: None,
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
async fn resolve_turn_selection_does_not_refresh_stale_pending_token() {
    let server = MockServer::start().await;
    // An OAuth refresh endpoint is available, but the CLI must NEVER call it —
    // codex-accounts is the sole token refresher. If the CLI refreshed, the rotated
    // token below would be persisted; the assertions verify it is not.
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
    assert!(
        matches!(
            selection,
            ChatgptAccountPoolSelectionOutcome::NoEligibleAccounts
        ),
        "the CLI does not refresh a stale pending token, so it leaves no usable account, got {selection:?}"
    );

    let accounts = pool.list_accounts().await.expect("accounts should list");
    assert_eq!(
        accounts
            .iter()
            .find(|account| account.account_id == "workspace-1")
            .expect("account should remain")
            .auth_status,
        ChatgptAccountPoolAuthStatus::PendingValidation,
        "a stale pending token is left pending for codex-accounts to refresh, not refreshed by the CLI"
    );
    let persisted = pool
        .load_account_secret("workspace-1")
        .await
        .expect("secret should load")
        .expect("secret should exist");
    assert_ne!(
        persisted.tokens.expect("tokens should exist").refresh_token,
        "refresh-workspace-1-rotated",
        "the CLI must not rotate the refresh token (codex-accounts is the sole refresher)"
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
async fn resolve_turn_selection_does_not_mark_pending_account_invalid() {
    let server = MockServer::start().await;
    // Even if a refresh would be rejected (401), the CLI never calls the refresh
    // endpoint and never marks the account invalid — codex-accounts owns
    // invalidation. The stale pending account is simply left pending.
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
        "a stale pending token the CLI won't refresh leaves no usable account, got {selection:?}"
    );

    let accounts = pool.list_accounts().await.expect("accounts should list");
    assert_eq!(
        accounts
            .iter()
            .find(|account| account.account_id == "workspace-1")
            .expect("account should remain")
            .auth_status,
        ChatgptAccountPoolAuthStatus::PendingValidation,
        "the CLI must not mark a pending account invalid; codex-accounts owns invalidation"
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
async fn resolve_turn_selection_marks_pending_account_invalid_on_authoritative_probe_401() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/backend-api/wham/usage"))
        // An authoritative auth-failure code (token_revoked) confirms the credential
        // is dead, so the CLI may mark the pending account invalid.
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "error": {
                "message": "Encountered invalidated oauth token for user, failing request",
                "code": "token_revoked"
            },
            "status": 401
        })))
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
        "an authoritative 401 during validation leaves no usable account, got {selection:?}"
    );

    let accounts = pool.list_accounts().await.expect("accounts should list");
    assert_eq!(
        accounts
            .iter()
            .find(|account| account.account_id == "workspace-1")
            .expect("account should remain")
            .auth_status,
        ChatgptAccountPoolAuthStatus::Invalid,
        "an authoritative 401 (token_revoked) marks the pending account invalid"
    );
}

#[tokio::test]
async fn resolve_turn_selection_leaves_pending_account_pending_on_ambiguous_401() {
    // Regression guard: an ambiguous 401 (no recognised auth-failure code, e.g. a
    // transient server blip or a token codex-accounts is mid-refresh for) must NOT
    // permanently kill the account. The CLI is consumer-only; codex-accounts owns
    // invalidation. The account is left pending_validation for it to adjudicate.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/backend-api/wham/usage"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "error": { "message": "temporary upstream failure", "code": "server_error" }
        })))
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
        "an ambiguous 401 leaves no usable account this pass, got {selection:?}"
    );

    let accounts = pool.list_accounts().await.expect("accounts should list");
    assert_eq!(
        accounts
            .iter()
            .find(|account| account.account_id == "workspace-1")
            .expect("account should remain")
            .auth_status,
        ChatgptAccountPoolAuthStatus::PendingValidation,
        "an ambiguous 401 must NOT permanently invalidate; it stays pending_validation"
    );
}

#[tokio::test]
async fn append_event_stamps_cli_actor_and_list_events_surfaces_it() {
    let codex_home = TempDir::new().expect("tempdir");
    let pool = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        None,
    )
    .await
    .expect("pool should open");
    // register_account appends an "account_registered" event through append_event.
    pool.register_account(&chatgpt_auth("actor@example.com", "workspace-actor", "pro"))
        .await
        .expect("account should register");

    let events = pool.list_events(None).await.expect("events should list");
    let registered = events
        .iter()
        .find(|event| event.event_type == "account_registered")
        .expect("a registration event should exist");
    let actor = registered
        .actor
        .as_deref()
        .expect("CLI-written events must carry an actor");
    assert!(
        actor.starts_with("codex-cli:"),
        "actor = {actor:?}, want a codex-cli:<host>:<pid> identity"
    );
    assert_eq!(
        actor,
        super::cli_actor(),
        "the stamped actor must match the pool's process actor"
    );

    // A legacy row written without an actor (older build) must surface as None, not error.
    sqlx::query(
        "INSERT INTO account_events (account_id, event_type, message, created_at) VALUES (?, ?, ?, ?)",
    )
    .bind("workspace-actor")
    .bind("legacy_event")
    .bind("written before the actor column existed")
    .bind(super::now_ts())
    .execute(&pool.pool)
    .await
    .expect("legacy insert should succeed");

    let events = pool.list_events(None).await.expect("events should list");
    let legacy = events
        .iter()
        .find(|event| event.event_type == "legacy_event")
        .expect("legacy event should exist");
    assert_eq!(
        legacy.actor, None,
        "a NULL actor column must surface as None, not an error"
    );
}

#[tokio::test]
async fn append_event_rolls_back_insert_when_retention_pruning_fails() {
    let codex_home = TempDir::new().expect("tempdir");
    let pool = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        None,
    )
    .await
    .expect("pool should open");
    sqlx::query(
        r#"
        WITH RECURSIVE sequence(value) AS (
            VALUES (1)
            UNION ALL
            SELECT value + 1 FROM sequence WHERE value < 2000
        )
        INSERT INTO account_events (account_id, event_type, message, actor, created_at)
        SELECT NULL, 'fixture', 'retained event', NULL, value FROM sequence
        "#,
    )
    .execute(&pool.pool)
    .await
    .expect("retention fixture should be inserted");
    sqlx::query(
        r#"
        CREATE TRIGGER fail_event_retention_delete
        BEFORE DELETE ON account_events
        BEGIN
            SELECT RAISE(ABORT, 'forced retention failure');
        END
        "#,
    )
    .execute(&pool.pool)
    .await
    .expect("failure trigger should be installed");

    pool.append_event(
        None,
        "event_that_must_rollback",
        "event and pruning are atomic".to_string(),
    )
    .await
    .expect_err("retention failure should reject the event");
    let inserted_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM account_events WHERE event_type = 'event_that_must_rollback'",
    )
    .fetch_one(&pool.pool)
    .await
    .expect("event count should load");
    let total_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM account_events")
        .fetch_one(&pool.pool)
        .await
        .expect("total event count should load");
    assert_eq!(inserted_count, 0);
    assert_eq!(total_count, 2_000);
}

#[test]
fn usage_auth_failure_confirms_invalid_classification() {
    // Authoritative auth-failure codes (object form and string form).
    for body in [
        r#"{"error":{"code":"token_revoked"},"status":401}"#,
        r#"{"error":{"code":"token_invalidated"}}"#,
        r#"{"error":{"code":"invalid_token"}}"#,
        r#"{"error":{"code":"expired_token"}}"#,
        r#"{"error":{"code":"token_expired"}}"#,
        r#"{"error":{"type":"token_revoked"}}"#,
        r#"{"error":"invalid_token"}"#,
    ] {
        assert!(
            usage_auth_failure_confirms_invalid(body),
            "expected authoritative for body: {body}"
        );
    }
    // Ambiguous / non-authoritative bodies must NOT confirm invalid.
    for body in [
        "",
        "   ",
        "not json",
        r#"{"error":{"code":"server_error"}}"#,
        r#"{"error":{"code":"rate_limit_exceeded"}}"#,
        r#"{"status":401}"#,
        r#"{"error":{}}"#,
    ] {
        assert!(
            !usage_auth_failure_confirms_invalid(body),
            "expected NOT authoritative for body: {body:?}"
        );
    }
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

    let stored_pool_auth = pool
        .read_account_tokens("workspace-refresh")
        .await
        .expect("pool auth should load")
        .expect("pool auth should exist");
    assert_eq!(stored_pool_auth.tokens, rotated_auth.tokens);
    assert_eq!(
        stored_pool_auth.pool_account_id,
        rotated_auth.pool_account_id
    );

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
async fn persist_refreshed_account_auth_rolls_back_tokens_when_event_write_fails() {
    let codex_home = TempDir::new().expect("tempdir");
    let pool = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        None,
    )
    .await
    .expect("pool should open");
    let original_auth = chatgpt_auth("refresh@example.com", "workspace-refresh", "pro");
    pool.register_account(&original_auth)
        .await
        .expect("account should register");
    pool.mark_account_auth_failed("workspace-refresh", "fixture invalidation")
        .await
        .expect("fixture status should become invalid");
    sqlx::query(
        r#"
        CREATE TRIGGER fail_auth_refreshed_event
        BEFORE INSERT ON account_events
        WHEN NEW.event_type = 'account_auth_refreshed'
        BEGIN
            SELECT RAISE(ABORT, 'forced refreshed event failure');
        END
        "#,
    )
    .execute(&pool.pool)
    .await
    .expect("failure trigger should be installed");
    let mut rotated_auth = original_auth.clone();
    let rotated_tokens = rotated_auth.tokens.as_mut().expect("tokens should exist");
    rotated_tokens.access_token = fake_access_token("workspace-refresh", 4_000);
    rotated_tokens.refresh_token = "refresh-rotated".to_string();

    pool.persist_refreshed_account_auth("workspace-refresh", &rotated_auth)
        .await
        .expect_err("event failure should reject refreshed auth persistence");

    let stored = pool
        .read_account_tokens("workspace-refresh")
        .await
        .expect("stored auth should load")
        .expect("stored auth should exist");
    assert_eq!(stored.tokens, original_auth.tokens);
    assert_eq!(
        pool.read_account_auth_status("workspace-refresh")
            .await
            .expect("status should load"),
        Some(ChatgptAccountPoolAuthStatus::Invalid)
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

    let stored_pool_auth = pool
        .read_account_tokens("workspace-refresh")
        .await
        .expect("pool auth should load")
        .expect("pool auth should exist");
    assert_eq!(stored_pool_auth.tokens, rotated_auth.tokens);
    assert_eq!(
        stored_pool_auth.pool_account_id,
        rotated_auth.pool_account_id
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

#[tokio::test]
async fn read_account_tokens_round_trips_token_bytes_from_db() {
    let codex_home = TempDir::new().expect("tempdir");
    let pool = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        None,
    )
    .await
    .expect("pool should open");
    let auth = chatgpt_auth("rt@example.com", "workspace-rt", "pro");
    pool.register_account(&auth)
        .await
        .expect("account should register");

    // The DB is the single source of truth: read the token bytes back without any
    // per-account auth.json file existing.
    let loaded = pool
        .read_account_tokens("workspace-rt")
        .await
        .expect("read should succeed")
        .expect("tokens should exist");
    let loaded_tokens = loaded.tokens.expect("loaded tokens present");
    let original_tokens = auth.tokens.expect("original tokens present");
    assert_eq!(loaded_tokens.access_token, original_tokens.access_token);
    assert_eq!(loaded_tokens.refresh_token, original_tokens.refresh_token);
    assert_eq!(
        loaded_tokens.id_token.raw_jwt,
        original_tokens.id_token.raw_jwt
    );
    assert_eq!(loaded.pool_account_id.as_deref(), Some("workspace-rt"));
    assert!(loaded.last_refresh.is_some());
}

#[tokio::test]
async fn read_account_tokens_keeps_the_stable_workspace_binding_on_claim_mismatch() {
    let codex_home = TempDir::new().expect("tempdir");
    let pool = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        None,
    )
    .await
    .expect("pool should open");
    pool.register_account(&chatgpt_auth(
        "bound@example.com",
        "workspace-stable",
        "pro",
    ))
    .await
    .expect("account should register");

    let mismatched_id_token = fake_jwt("bound@example.com", "workspace-from-token", "pro");
    sqlx::query("UPDATE accounts SET id_token = ? WHERE account_id = ?")
        .bind(mismatched_id_token)
        .bind("workspace-stable")
        .execute(&pool.pool)
        .await
        .expect("fixture should update the stored ID token");

    let loaded = pool
        .read_account_tokens("workspace-stable")
        .await
        .expect("read should succeed")
        .expect("tokens should exist");
    assert_eq!(
        loaded.tokens.expect("tokens").account_id.as_deref(),
        Some("workspace-stable")
    );
}

#[tokio::test]
async fn read_account_tokens_rejects_a_member_identity_mismatch() {
    let codex_home = TempDir::new().expect("tempdir");
    let pool = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        None,
    )
    .await
    .expect("pool should open");
    pool.register_account(&chatgpt_auth_with_member(
        "bound@example.com",
        "workspace-stable",
        "pro",
        "user-1",
        "pool-stable",
    ))
    .await
    .expect("account should register");

    let mismatched_id_token =
        fake_jwt_with_member("other@example.com", "workspace-stable", "pro", "user-2");
    sqlx::query("UPDATE accounts SET id_token = ? WHERE account_id = ?")
        .bind(mismatched_id_token)
        .bind("pool-stable")
        .execute(&pool.pool)
        .await
        .expect("fixture should update the stored ID token");

    let err = pool
        .read_account_tokens("pool-stable")
        .await
        .expect_err("a stored token must not change the bound member");
    assert!(matches!(
        err,
        ChatgptAccountPoolError::CredentialIdentityMismatch(ref id)
            if id == "pool-stable"
    ));
}

#[tokio::test]
async fn read_account_tokens_returns_none_when_account_missing() {
    let codex_home = TempDir::new().expect("tempdir");
    let pool = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        None,
    )
    .await
    .expect("pool should open");
    let missing = pool
        .read_account_tokens("does-not-exist")
        .await
        .expect("read should succeed");
    assert!(missing.is_none());
}

#[tokio::test]
async fn write_account_tokens_rejects_missing_account() {
    let codex_home = TempDir::new().expect("tempdir");
    let pool = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        None,
    )
    .await
    .expect("pool should open");
    let err = pool
        .write_account_tokens(
            "does-not-exist",
            &chatgpt_auth("missing@example.com", "does-not-exist", "pro"),
        )
        .await
        .expect_err("a zero-row token update should not report success");
    assert!(matches!(
        err,
        ChatgptAccountPoolError::AccountNotFound(ref account_id)
            if account_id == "does-not-exist"
    ));
}

#[tokio::test]
async fn load_account_codex_auth_builds_chatgpt_auth_from_db() {
    let codex_home = TempDir::new().expect("tempdir");
    let pool = ChatgptAccountPool::open(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        None,
    )
    .await
    .expect("pool should open");
    let auth = chatgpt_auth("ca@example.com", "workspace-ca", "pro");
    pool.register_account(&auth)
        .await
        .expect("account should register");

    let codex_auth = pool
        .load_account_codex_auth("workspace-ca")
        .await
        .expect("load should succeed")
        .expect("auth should exist");
    assert!(codex_auth.is_chatgpt_auth());
    let token_data = codex_auth
        .get_token_data()
        .expect("token data should be available");
    assert_eq!(token_data.refresh_token, "refresh-workspace-ca");
}

#[test]
fn backend_ent26_plan_maps_to_the_account_protocol_type() {
    let backend_plan = serde_json::from_str::<BackendPlanType>("\"ent26\"")
        .expect("ent26 backend plan should deserialize");
    assert_eq!(map_plan_type(backend_plan), AccountPlanType::Ent26);
}

#[test]
fn account_pool_usage_mapping_preserves_spend_control_and_relative_resets() {
    let payload = RateLimitStatusPayload {
        plan_type: BackendPlanType::Business,
        rate_limit: Some(Some(Box::new(BackendRateLimitStatusDetails {
            allowed: false,
            limit_reached: true,
            primary_window: Some(Some(Box::new(RateLimitWindowSnapshot {
                used_percent: 100,
                limit_window_seconds: 300,
                reset_after_seconds: 120,
                reset_at: 0,
            }))),
            ..Default::default()
        }))),
        credits: None,
        spend_control: Some(Some(Box::new(BackendSpendControlStatusDetails {
            reached: true,
            individual_limit: Some(Some(Box::new(
                codex_backend_openapi_models::models::SpendControlLimitDetails {
                    source: None,
                    limit: "100".to_string(),
                    used: "100".to_string(),
                    remaining: "0".to_string(),
                    used_percent: 100,
                    remaining_percent: 0,
                    reset_after_seconds: 3_600,
                    reset_at: 0,
                },
            ))),
        }))),
        additional_rate_limits: None,
        rate_limit_reached_type: None,
    };

    let snapshots = rate_limit_snapshots_from_payload_at(payload, 10_000);

    assert_eq!(
        snapshots[0]
            .primary
            .as_ref()
            .and_then(|window| window.resets_at),
        Some(10_120)
    );
    assert_eq!(snapshots[0].spend_control_reached, Some(true));
    assert_eq!(
        snapshots[0].rate_limit_reached_type,
        Some(RateLimitReachedType::RateLimitReached)
    );
    assert_eq!(
        snapshots[0].individual_limit,
        Some(SpendControlLimitSnapshot {
            limit: "100".to_string(),
            used: "100".to_string(),
            remaining_percent: 0,
            resets_at: 13_600,
        })
    );
}
