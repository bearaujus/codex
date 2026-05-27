mod registration;

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

use chrono::DateTime;
use chrono::Utc;
use codex_backend_openapi_models::models::CreditStatusDetails;
use codex_backend_openapi_models::models::PlanType as BackendPlanType;
use codex_backend_openapi_models::models::RateLimitReachedKind as BackendRateLimitReachedKind;
use codex_backend_openapi_models::models::RateLimitStatusDetails as BackendRateLimitStatusDetails;
use codex_backend_openapi_models::models::RateLimitStatusPayload;
use codex_backend_openapi_models::models::RateLimitWindowSnapshot;
use codex_client::build_reqwest_client_with_custom_ca;
use codex_protocol::account::PlanType as AccountPlanType;
use codex_protocol::protocol::RateLimitReachedType;
use codex_protocol::protocol::RateLimitSnapshot;
use codex_protocol::protocol::RateLimitWindow;
use reqwest::header::AUTHORIZATION;
use reqwest::header::CONTENT_TYPE;
use reqwest::header::HeaderMap;
use reqwest::header::HeaderName;
use reqwest::header::HeaderValue;
use reqwest::header::USER_AGENT;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use sqlx::Row;
use sqlx::SqlitePool;
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::sqlite::SqliteJournalMode;
use sqlx::sqlite::SqlitePoolOptions;

use crate::AuthCredentialsStoreMode;
use crate::AuthDotJson;
use crate::CodexAuth;
use crate::default_client::get_codex_user_agent;
use crate::load_auth_dot_json;
use crate::logout;
use crate::save_auth;

pub use registration::AccountRegistrationServer;
pub use registration::AccountRegistrationStart;
pub use registration::run_account_registration_server;

const DEFAULT_CHATGPT_BACKEND_BASE_URL: &str = "https://chatgpt.com/backend-api";
const POOL_DB_DIR: &str = "account-pool";
const POOL_DB_FILE: &str = "accounts.sqlite";
const SECRET_ROOT_DIR: &str = "auth";
const EVENT_LIMIT_DEFAULT: i64 = 100;

#[derive(Debug, thiserror::Error)]
pub enum ChatgptAccountPoolError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
    #[error(transparent)]
    SerdeJson(#[from] serde_json::Error),
    #[error("managed ChatGPT auth is missing an account id")]
    MissingAccountId,
    #[error("only managed ChatGPT OAuth accounts can be stored in the account pool")]
    UnsupportedAuthMode,
    #[error("account not found: {0}")]
    AccountNotFound(String),
    #[error("no eligible ChatGPT accounts are available")]
    NoEligibleAccounts,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatgptAccountPoolAuthStatus {
    Valid,
    MissingSecret,
    Invalid,
    RefreshFailedPermanent,
}

impl ChatgptAccountPoolAuthStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Valid => "valid",
            Self::MissingSecret => "missing_secret",
            Self::Invalid => "invalid",
            Self::RefreshFailedPermanent => "refresh_failed_permanent",
        }
    }

    fn from_db(value: &str) -> Self {
        match value {
            "missing_secret" => Self::MissingSecret,
            "invalid" => Self::Invalid,
            "refresh_failed_permanent" => Self::RefreshFailedPermanent,
            _ => Self::Valid,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChatgptAccountPoolAccount {
    pub account_id: String,
    pub email: Option<String>,
    pub plan_type: Option<String>,
    pub enabled: bool,
    pub is_default: bool,
    pub is_selected: bool,
    pub created_at: i64,
    pub updated_at: i64,
    pub last_used_at: Option<i64>,
    pub last_auth_refresh_at: Option<i64>,
    pub auth_status: ChatgptAccountPoolAuthStatus,
    pub cooldown_until: Option<i64>,
    pub cooldown_reason: Option<String>,
    pub rate_limits: BTreeMap<String, RateLimitSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatgptAccountEvent {
    pub account_id: Option<String>,
    pub event_type: String,
    pub message: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChatgptAccountPoolRateLimitEntry {
    pub account_id: String,
    pub fetched_at: Option<i64>,
    pub rate_limits: BTreeMap<String, RateLimitSnapshot>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ChatgptAccountPoolSelectionOutcome {
    Unchanged,
    Activated {
        account_id: String,
        auth: AuthDotJson,
        failover: bool,
    },
    NoEligibleAccounts,
}

#[derive(Clone)]
pub struct ChatgptAccountPool {
    codex_home: PathBuf,
    auth_credentials_store_mode: AuthCredentialsStoreMode,
    chatgpt_base_url: String,
    pool: SqlitePool,
}

impl std::fmt::Debug for ChatgptAccountPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChatgptAccountPool")
            .field("codex_home", &self.codex_home)
            .field(
                "auth_credentials_store_mode",
                &self.auth_credentials_store_mode,
            )
            .field("chatgpt_base_url", &self.chatgpt_base_url)
            .finish_non_exhaustive()
    }
}

impl ChatgptAccountPool {
    pub async fn open(
        codex_home: PathBuf,
        auth_credentials_store_mode: AuthCredentialsStoreMode,
        chatgpt_base_url: Option<String>,
    ) -> Result<Self, ChatgptAccountPoolError> {
        std::fs::create_dir_all(pool_root_dir(&codex_home))?;
        let connect_options = SqliteConnectOptions::new()
            .filename(pool_db_path(&codex_home))
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(connect_options)
            .await?;
        let this = Self {
            codex_home,
            auth_credentials_store_mode,
            chatgpt_base_url: chatgpt_base_url
                .unwrap_or_else(|| DEFAULT_CHATGPT_BACKEND_BASE_URL.to_string()),
            pool,
        };
        this.initialize_schema().await?;
        this.migrate_legacy_auth_if_needed().await?;
        Ok(this)
    }

    pub async fn list_accounts(
        &self,
    ) -> Result<Vec<ChatgptAccountPoolAccount>, ChatgptAccountPoolError> {
        let selected_account_id = self.selected_account_id().await?;
        let rows = sqlx::query(
            r#"
            SELECT
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
            FROM accounts
            ORDER BY created_at ASC, account_id ASC
            "#,
        )
        .fetch_all(&self.pool)
        .await?;
        let grouped_rate_limits = self.load_rate_limits_grouped().await?;
        rows.into_iter()
            .map(|row| {
                let account_id: String = row.get("account_id");
                Ok(ChatgptAccountPoolAccount {
                    email: row.get("email"),
                    plan_type: row.get("plan_type"),
                    enabled: row.get::<i64, _>("enabled") != 0,
                    is_default: row.get::<i64, _>("is_default") != 0,
                    is_selected: selected_account_id.as_deref() == Some(account_id.as_str()),
                    created_at: row.get("created_at"),
                    updated_at: row.get("updated_at"),
                    last_used_at: row.get("last_used_at"),
                    last_auth_refresh_at: row.get("last_auth_refresh_at"),
                    auth_status: ChatgptAccountPoolAuthStatus::from_db(
                        &row.get::<String, _>("auth_status"),
                    ),
                    cooldown_until: row.get("cooldown_until"),
                    cooldown_reason: row.get("cooldown_reason"),
                    rate_limits: grouped_rate_limits
                        .get(account_id.as_str())
                        .cloned()
                        .unwrap_or_default(),
                    account_id,
                })
            })
            .collect()
    }

    pub async fn list_events(
        &self,
        limit: Option<i64>,
    ) -> Result<Vec<ChatgptAccountEvent>, ChatgptAccountPoolError> {
        let rows = sqlx::query(
            r#"
            SELECT account_id, event_type, message, created_at
            FROM account_events
            ORDER BY id DESC
            LIMIT ?
            "#,
        )
        .bind(limit.unwrap_or(EVENT_LIMIT_DEFAULT))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| ChatgptAccountEvent {
                account_id: row.get("account_id"),
                event_type: row.get("event_type"),
                message: row.get("message"),
                created_at: row.get("created_at"),
            })
            .collect())
    }

    pub async fn list_rate_limits(
        &self,
    ) -> Result<Vec<ChatgptAccountPoolRateLimitEntry>, ChatgptAccountPoolError> {
        let grouped_rate_limits = self.load_rate_limits_grouped_with_fetch_time().await?;
        Ok(grouped_rate_limits
            .into_iter()
            .map(
                |(account_id, (fetched_at, rate_limits))| ChatgptAccountPoolRateLimitEntry {
                    account_id,
                    fetched_at,
                    rate_limits,
                },
            )
            .collect())
    }

    pub async fn register_account(
        &self,
        auth: &AuthDotJson,
    ) -> Result<ChatgptAccountPoolAccount, ChatgptAccountPoolError> {
        let metadata = extract_chatgpt_metadata(auth)?;
        save_auth(
            &self.secret_codex_home(&metadata.account_id),
            auth,
            self.auth_credentials_store_mode,
        )?;

        let now = now_ts();
        let auth_status = ChatgptAccountPoolAuthStatus::Valid.as_str();
        let selected_account_id = self.selected_account_id().await?;
        let has_default = self.has_default_account().await?;
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
            VALUES (?, ?, ?, 1, ?, ?, ?, NULL, ?, ?, NULL, NULL)
            ON CONFLICT(account_id) DO UPDATE SET
                email = excluded.email,
                plan_type = excluded.plan_type,
                enabled = 1,
                updated_at = excluded.updated_at,
                last_auth_refresh_at = excluded.last_auth_refresh_at,
                auth_status = excluded.auth_status,
                cooldown_until = NULL,
                cooldown_reason = NULL
            "#,
        )
        .bind(&metadata.account_id)
        .bind(&metadata.email)
        .bind(&metadata.plan_type)
        .bind((!has_default) as i64)
        .bind(now)
        .bind(now)
        .bind(auth.last_refresh.map(|value| value.timestamp()))
        .bind(auth_status)
        .execute(&self.pool)
        .await?;
        if selected_account_id.is_none() {
            self.set_selected_account_id(Some(&metadata.account_id), /*change_default*/ false)
                .await?;
        }
        self.append_event(
            Some(&metadata.account_id),
            "account_registered",
            format!(
                "Registered ChatGPT account {}",
                account_suffix(&metadata.account_id)
            ),
        )
        .await?;
        self.account_by_id(&metadata.account_id).await?.ok_or(
            ChatgptAccountPoolError::AccountNotFound(metadata.account_id),
        )
    }

    pub async fn selected_account_auth(
        &self,
    ) -> Result<Option<(String, AuthDotJson)>, ChatgptAccountPoolError> {
        let Some(account_id) = self.selected_account_id().await? else {
            return Ok(None);
        };
        match self.load_account_secret(&account_id)? {
            Some(auth) => Ok(Some((account_id, auth))),
            None => {
                self.update_auth_status(&account_id, ChatgptAccountPoolAuthStatus::MissingSecret)
                    .await?;
                Ok(None)
            }
        }
    }

    pub async fn select_account(
        &self,
        account_id: &str,
    ) -> Result<ChatgptAccountPoolAccount, ChatgptAccountPoolError> {
        self.require_account(account_id).await?;
        self.set_selected_account_id(Some(account_id), /*change_default*/ true)
            .await?;
        self.append_event(
            Some(account_id),
            "account_selected",
            format!("Selected ChatGPT account {}", account_suffix(account_id)),
        )
        .await?;
        self.account_by_id(account_id)
            .await?
            .ok_or_else(|| ChatgptAccountPoolError::AccountNotFound(account_id.to_string()))
    }

    pub async fn remove_account(&self, account_id: &str) -> Result<bool, ChatgptAccountPoolError> {
        let selected_account_id = self.selected_account_id().await?;
        let default_account_id = self.default_account_id().await?;
        let removed_rows = sqlx::query("DELETE FROM accounts WHERE account_id = ?")
            .bind(account_id)
            .execute(&self.pool)
            .await?
            .rows_affected();
        if removed_rows == 0 {
            return Ok(false);
        }
        sqlx::query("DELETE FROM account_rate_limits WHERE account_id = ?")
            .bind(account_id)
            .execute(&self.pool)
            .await?;
        let _ = logout(
            &self.secret_codex_home(account_id),
            self.auth_credentials_store_mode,
        )?;
        if selected_account_id.as_deref() == Some(account_id) {
            let replacement = self.best_default_replacement().await?;
            self.set_selected_account_id(replacement.as_deref(), /*change_default*/ false)
                .await?;
        }
        if default_account_id.as_deref() == Some(account_id) {
            let replacement = self.best_default_replacement().await?;
            self.set_default_account_id(replacement.as_deref()).await?;
        }
        self.append_event(
            Some(account_id),
            "account_removed",
            format!("Removed ChatGPT account {}", account_suffix(account_id)),
        )
        .await?;
        Ok(true)
    }

    pub async fn resolve_turn_selection(
        &self,
        current_account_id: Option<&str>,
        current_refresh_failed_permanently: bool,
    ) -> Result<ChatgptAccountPoolSelectionOutcome, ChatgptAccountPoolError> {
        if let Some(account_id) = current_account_id
            && current_refresh_failed_permanently
        {
            self.update_auth_status(
                account_id,
                ChatgptAccountPoolAuthStatus::RefreshFailedPermanent,
            )
            .await?;
        }
        let selected_account_id = self.selected_account_id().await?;
        let accounts = self.list_accounts().await?;
        if accounts.is_empty() {
            return Ok(ChatgptAccountPoolSelectionOutcome::Unchanged);
        }
        let now = now_ts();
        if let Some(selected_account_id) = selected_account_id.as_deref()
            && let Some(selected_account) = accounts
                .iter()
                .find(|account| account.account_id == selected_account_id)
            && is_account_eligible(selected_account, now)
        {
            if current_account_id == Some(selected_account_id) {
                return Ok(ChatgptAccountPoolSelectionOutcome::Unchanged);
            }
            return self
                .activate_account(selected_account_id, /*failover*/ false)
                .await;
        }

        let Some(best_account_id) = select_best_account(&accounts, now) else {
            return Ok(ChatgptAccountPoolSelectionOutcome::NoEligibleAccounts);
        };
        let failover = current_account_id.is_some_and(|current| current != best_account_id);
        if selected_account_id.as_deref() != Some(best_account_id) {
            self.set_selected_account_id(Some(best_account_id), /*change_default*/ false)
                .await?;
            self.append_event(
                Some(best_account_id),
                "account_failover_selected",
                format!(
                    "Selected fallback ChatGPT account {}",
                    account_suffix(best_account_id)
                ),
            )
            .await?;
        }
        if current_account_id == Some(best_account_id) {
            return Ok(ChatgptAccountPoolSelectionOutcome::Unchanged);
        }
        self.activate_account(best_account_id, failover).await
    }

    pub async fn mark_current_account_rate_limited(
        &self,
        account_id: &str,
        snapshot: Option<&RateLimitSnapshot>,
        resets_at: Option<DateTime<Utc>>,
    ) -> Result<(), ChatgptAccountPoolError> {
        let mut cooldown_until = resets_at.map(|value| value.timestamp());
        if let Some(snapshot) = snapshot {
            if let Some(primary_reset) = snapshot
                .primary
                .as_ref()
                .and_then(|window| window.resets_at)
            {
                cooldown_until = Some(cooldown_until.unwrap_or(primary_reset).max(primary_reset));
            }
            if let Some(secondary_reset) = snapshot
                .secondary
                .as_ref()
                .and_then(|window| window.resets_at)
            {
                cooldown_until = Some(
                    cooldown_until
                        .unwrap_or(secondary_reset)
                        .max(secondary_reset),
                );
            }
            self.store_rate_limit_snapshot(account_id, snapshot, now_ts())
                .await?;
        }
        sqlx::query(
            r#"
            UPDATE accounts
            SET updated_at = ?, cooldown_until = ?, cooldown_reason = ?, auth_status = ?
            WHERE account_id = ?
            "#,
        )
        .bind(now_ts())
        .bind(cooldown_until)
        .bind(Some("usage_limit_reached"))
        .bind(ChatgptAccountPoolAuthStatus::Valid.as_str())
        .bind(account_id)
        .execute(&self.pool)
        .await?;
        self.append_event(
            Some(account_id),
            "rate_limit_reached",
            format!(
                "Rate limit reached for ChatGPT account {}",
                account_suffix(account_id)
            ),
        )
        .await?;
        Ok(())
    }

    pub async fn refresh_rate_limits(
        &self,
        account_id: &str,
    ) -> Result<ChatgptAccountPoolRateLimitEntry, ChatgptAccountPoolError> {
        let auth = self
            .load_account_codex_auth(account_id)
            .await?
            .ok_or_else(|| ChatgptAccountPoolError::AccountNotFound(account_id.to_string()))?;
        let snapshots = fetch_rate_limit_snapshots(&self.chatgpt_base_url, &auth).await?;
        let fetched_at = now_ts();
        sqlx::query("DELETE FROM account_rate_limits WHERE account_id = ?")
            .bind(account_id)
            .execute(&self.pool)
            .await?;
        let mut cooldown_until = None;
        let mut grouped = BTreeMap::new();
        for snapshot in snapshots {
            if let Some(reset_at) = latest_reset_at(&snapshot) {
                let exhausted = snapshot.primary.as_ref().is_some_and(window_exhausted)
                    || snapshot.secondary.as_ref().is_some_and(window_exhausted);
                if exhausted {
                    cooldown_until = Some(cooldown_until.unwrap_or(reset_at).max(reset_at));
                }
            }
            self.store_rate_limit_snapshot(account_id, &snapshot, fetched_at)
                .await?;
            let limit_id = snapshot
                .limit_id
                .clone()
                .unwrap_or_else(|| "codex".to_string());
            grouped.insert(limit_id, snapshot);
        }
        sqlx::query(
            r#"
            UPDATE accounts
            SET updated_at = ?, cooldown_until = ?, cooldown_reason = ?, auth_status = ?
            WHERE account_id = ?
            "#,
        )
        .bind(fetched_at)
        .bind(cooldown_until)
        .bind(cooldown_until.map(|_| "rate_limits_refreshed"))
        .bind(ChatgptAccountPoolAuthStatus::Valid.as_str())
        .bind(account_id)
        .execute(&self.pool)
        .await?;
        self.append_event(
            Some(account_id),
            "rate_limits_refreshed",
            format!(
                "Refreshed rate limits for ChatGPT account {}",
                account_suffix(account_id)
            ),
        )
        .await?;
        Ok(ChatgptAccountPoolRateLimitEntry {
            account_id: account_id.to_string(),
            fetched_at: Some(fetched_at),
            rate_limits: grouped,
        })
    }

    async fn activate_account(
        &self,
        account_id: &str,
        failover: bool,
    ) -> Result<ChatgptAccountPoolSelectionOutcome, ChatgptAccountPoolError> {
        let Some(auth) = self.load_account_secret(account_id)? else {
            self.update_auth_status(account_id, ChatgptAccountPoolAuthStatus::MissingSecret)
                .await?;
            return Ok(ChatgptAccountPoolSelectionOutcome::NoEligibleAccounts);
        };
        let now = now_ts();
        sqlx::query("UPDATE accounts SET last_used_at = ?, updated_at = ? WHERE account_id = ?")
            .bind(now)
            .bind(now)
            .bind(account_id)
            .execute(&self.pool)
            .await?;
        Ok(ChatgptAccountPoolSelectionOutcome::Activated {
            account_id: account_id.to_string(),
            auth,
            failover,
        })
    }

    async fn initialize_schema(&self) -> Result<(), ChatgptAccountPoolError> {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS accounts (
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
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS account_rate_limits (
                account_id TEXT NOT NULL,
                limit_id TEXT NOT NULL,
                snapshot_json TEXT NOT NULL,
                fetched_at INTEGER NOT NULL,
                PRIMARY KEY (account_id, limit_id)
            )
            "#,
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS account_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                account_id TEXT NULL,
                event_type TEXT NOT NULL,
                message TEXT NOT NULL,
                created_at INTEGER NOT NULL
            )
            "#,
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS pool_state (
                key TEXT PRIMARY KEY,
                value TEXT NULL
            )
            "#,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn migrate_legacy_auth_if_needed(&self) -> Result<(), ChatgptAccountPoolError> {
        let Some(auth) = load_auth_dot_json(&self.codex_home, self.auth_credentials_store_mode)?
        else {
            return Ok(());
        };
        if extract_chatgpt_metadata(&auth).is_err() {
            return Ok(());
        }
        let metadata = extract_chatgpt_metadata(&auth)?;
        if self.account_exists(&metadata.account_id).await? {
            return Ok(());
        }
        self.register_account(&auth).await?;
        self.append_event(
            Some(&metadata.account_id),
            "legacy_auth_migrated",
            format!(
                "Migrated legacy ChatGPT auth {} into the account pool",
                account_suffix(&metadata.account_id)
            ),
        )
        .await?;
        Ok(())
    }

    async fn load_rate_limits_grouped(
        &self,
    ) -> Result<BTreeMap<String, BTreeMap<String, RateLimitSnapshot>>, ChatgptAccountPoolError>
    {
        Ok(self
            .load_rate_limits_grouped_with_fetch_time()
            .await?
            .into_iter()
            .map(|(account_id, (_fetched_at, rate_limits))| (account_id, rate_limits))
            .collect())
    }

    async fn load_rate_limits_grouped_with_fetch_time(
        &self,
    ) -> Result<
        BTreeMap<String, (Option<i64>, BTreeMap<String, RateLimitSnapshot>)>,
        ChatgptAccountPoolError,
    > {
        let rows = sqlx::query(
            "SELECT account_id, limit_id, snapshot_json, fetched_at FROM account_rate_limits",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut grouped = BTreeMap::new();
        for row in rows {
            let account_id: String = row.get("account_id");
            let limit_id: String = row.get("limit_id");
            let snapshot_json: String = row.get("snapshot_json");
            let fetched_at: i64 = row.get("fetched_at");
            let snapshot = serde_json::from_str::<RateLimitSnapshot>(&snapshot_json)?;
            let entry = grouped
                .entry(account_id)
                .or_insert_with(|| (Some(fetched_at), BTreeMap::new()));
            entry.0 = Some(entry.0.unwrap_or(fetched_at).max(fetched_at));
            entry.1.insert(limit_id, snapshot);
        }
        Ok(grouped)
    }

    async fn store_rate_limit_snapshot(
        &self,
        account_id: &str,
        snapshot: &RateLimitSnapshot,
        fetched_at: i64,
    ) -> Result<(), ChatgptAccountPoolError> {
        let limit_id = snapshot
            .limit_id
            .clone()
            .unwrap_or_else(|| "codex".to_string());
        sqlx::query(
            r#"
            INSERT INTO account_rate_limits (account_id, limit_id, snapshot_json, fetched_at)
            VALUES (?, ?, ?, ?)
            ON CONFLICT(account_id, limit_id) DO UPDATE SET
                snapshot_json = excluded.snapshot_json,
                fetched_at = excluded.fetched_at
            "#,
        )
        .bind(account_id)
        .bind(limit_id)
        .bind(serde_json::to_string(snapshot)?)
        .bind(fetched_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn append_event(
        &self,
        account_id: Option<&str>,
        event_type: &str,
        message: String,
    ) -> Result<(), ChatgptAccountPoolError> {
        sqlx::query(
            "INSERT INTO account_events (account_id, event_type, message, created_at) VALUES (?, ?, ?, ?)",
        )
        .bind(account_id)
        .bind(event_type)
        .bind(message)
        .bind(now_ts())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn set_selected_account_id(
        &self,
        account_id: Option<&str>,
        change_default: bool,
    ) -> Result<(), ChatgptAccountPoolError> {
        sqlx::query(
            "INSERT INTO pool_state (key, value) VALUES ('selected_account_id', ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        )
        .bind(account_id)
        .execute(&self.pool)
        .await?;
        if change_default {
            self.set_default_account_id(account_id).await?;
        }
        Ok(())
    }

    async fn set_default_account_id(
        &self,
        account_id: Option<&str>,
    ) -> Result<(), ChatgptAccountPoolError> {
        sqlx::query("UPDATE accounts SET is_default = CASE WHEN account_id = ? THEN 1 ELSE 0 END")
            .bind(account_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn selected_account_id(&self) -> Result<Option<String>, ChatgptAccountPoolError> {
        let row = sqlx::query("SELECT value FROM pool_state WHERE key = 'selected_account_id'")
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.and_then(|row| row.get::<Option<String>, _>("value")))
    }

    async fn default_account_id(&self) -> Result<Option<String>, ChatgptAccountPoolError> {
        let row = sqlx::query("SELECT account_id FROM accounts WHERE is_default = 1 LIMIT 1")
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|row| row.get("account_id")))
    }

    async fn has_default_account(&self) -> Result<bool, ChatgptAccountPoolError> {
        Ok(self.default_account_id().await?.is_some())
    }

    async fn account_exists(&self, account_id: &str) -> Result<bool, ChatgptAccountPoolError> {
        let row = sqlx::query("SELECT 1 FROM accounts WHERE account_id = ? LIMIT 1")
            .bind(account_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.is_some())
    }

    async fn require_account(&self, account_id: &str) -> Result<(), ChatgptAccountPoolError> {
        if self.account_exists(account_id).await? {
            Ok(())
        } else {
            Err(ChatgptAccountPoolError::AccountNotFound(
                account_id.to_string(),
            ))
        }
    }

    async fn account_by_id(
        &self,
        account_id: &str,
    ) -> Result<Option<ChatgptAccountPoolAccount>, ChatgptAccountPoolError> {
        Ok(self
            .list_accounts()
            .await?
            .into_iter()
            .find(|account| account.account_id == account_id))
    }

    async fn best_default_replacement(&self) -> Result<Option<String>, ChatgptAccountPoolError> {
        let accounts = self.list_accounts().await?;
        let now = now_ts();
        Ok(select_best_account(&accounts, now)
            .map(str::to_string)
            .or_else(|| accounts.first().map(|account| account.account_id.clone())))
    }

    async fn update_auth_status(
        &self,
        account_id: &str,
        auth_status: ChatgptAccountPoolAuthStatus,
    ) -> Result<(), ChatgptAccountPoolError> {
        sqlx::query("UPDATE accounts SET auth_status = ?, updated_at = ? WHERE account_id = ?")
            .bind(auth_status.as_str())
            .bind(now_ts())
            .bind(account_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    fn secret_codex_home(&self, account_id: &str) -> PathBuf {
        pool_root_dir(&self.codex_home)
            .join(SECRET_ROOT_DIR)
            .join(hash_fragment(account_id))
    }

    fn load_account_secret(
        &self,
        account_id: &str,
    ) -> Result<Option<AuthDotJson>, ChatgptAccountPoolError> {
        load_auth_dot_json(
            &self.secret_codex_home(account_id),
            self.auth_credentials_store_mode,
        )
        .map_err(ChatgptAccountPoolError::from)
    }

    async fn load_account_codex_auth(
        &self,
        account_id: &str,
    ) -> Result<Option<CodexAuth>, ChatgptAccountPoolError> {
        CodexAuth::from_auth_storage(
            &self.secret_codex_home(account_id),
            self.auth_credentials_store_mode,
            Some(self.chatgpt_base_url.as_str()),
        )
        .await
        .map_err(ChatgptAccountPoolError::from)
    }
}

#[derive(Debug)]
struct ChatgptAccountMetadata {
    account_id: String,
    email: Option<String>,
    plan_type: Option<String>,
}

fn extract_chatgpt_metadata(
    auth: &AuthDotJson,
) -> Result<ChatgptAccountMetadata, ChatgptAccountPoolError> {
    if auth.openai_api_key.is_some() || auth.agent_identity.is_some() {
        return Err(ChatgptAccountPoolError::UnsupportedAuthMode);
    }
    let tokens = auth
        .tokens
        .as_ref()
        .ok_or(ChatgptAccountPoolError::UnsupportedAuthMode)?;
    if auth
        .auth_mode
        .is_some_and(|mode| mode != codex_app_server_protocol::AuthMode::Chatgpt)
    {
        return Err(ChatgptAccountPoolError::UnsupportedAuthMode);
    }
    let account_id = tokens
        .account_id
        .clone()
        .or_else(|| tokens.id_token.chatgpt_account_id.clone())
        .ok_or(ChatgptAccountPoolError::MissingAccountId)?;
    Ok(ChatgptAccountMetadata {
        account_id,
        email: tokens.id_token.email.clone(),
        plan_type: tokens.id_token.get_chatgpt_plan_type_raw(),
    })
}

fn now_ts() -> i64 {
    Utc::now().timestamp()
}

fn pool_root_dir(codex_home: &Path) -> PathBuf {
    codex_home.join(POOL_DB_DIR)
}

fn pool_db_path(codex_home: &Path) -> PathBuf {
    pool_root_dir(codex_home).join(POOL_DB_FILE)
}

fn hash_fragment(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    let digest = hasher.finalize();
    let hex = format!("{digest:x}");
    hex[..16].to_string()
}

fn account_suffix(account_id: &str) -> &str {
    let split_at = account_id.len().saturating_sub(8);
    &account_id[split_at..]
}

fn is_account_eligible(account: &ChatgptAccountPoolAccount, now: i64) -> bool {
    account.enabled
        && matches!(account.auth_status, ChatgptAccountPoolAuthStatus::Valid)
        && account
            .cooldown_until
            .is_none_or(|cooldown_until| cooldown_until <= now)
}

fn select_best_account<'a>(accounts: &'a [ChatgptAccountPoolAccount], now: i64) -> Option<&'a str> {
    accounts
        .iter()
        .filter(|account| is_account_eligible(account, now))
        .max_by(|left, right| compare_account_capacity(left, right, now))
        .map(|account| account.account_id.as_str())
}

fn compare_account_capacity(
    left: &ChatgptAccountPoolAccount,
    right: &ChatgptAccountPoolAccount,
    now: i64,
) -> std::cmp::Ordering {
    let left_score = capacity_score(left, now);
    let right_score = capacity_score(right, now);
    left_score
        .cmp(&right_score)
        .then_with(|| right.last_used_at.cmp(&left.last_used_at))
        .then_with(|| right.account_id.cmp(&left.account_id))
}

fn capacity_score(account: &ChatgptAccountPoolAccount, now: i64) -> (bool, i64) {
    let snapshot = account
        .rate_limits
        .get("codex")
        .or_else(|| account.rate_limits.values().next());
    let Some(snapshot) = snapshot else {
        return (false, -1);
    };
    // Only treat rate_limit_reached_type as authoritative while the cooldown is
    // still active. Once the cooldown expires the stored snapshot may be stale
    // (the account recovered without a fresh refresh_rate_limits call), so fall
    // back to the raw window percentages instead of scoring the account at 0.
    let cooldown_active = account.cooldown_until.is_some_and(|c| c > now);
    let remaining = remaining_percent(snapshot, cooldown_active).unwrap_or(-1);
    (true, remaining)
}

fn remaining_percent(snapshot: &RateLimitSnapshot, cooldown_active: bool) -> Option<i64> {
    if cooldown_active
        && snapshot.rate_limit_reached_type.is_some_and(|kind| {
            matches!(
                kind,
                RateLimitReachedType::RateLimitReached
                    | RateLimitReachedType::WorkspaceOwnerCreditsDepleted
                    | RateLimitReachedType::WorkspaceMemberCreditsDepleted
                    | RateLimitReachedType::WorkspaceOwnerUsageLimitReached
                    | RateLimitReachedType::WorkspaceMemberUsageLimitReached
            )
        })
    {
        return Some(0);
    }
    let mut remaining = Vec::new();
    if let Some(primary) = snapshot.primary.as_ref() {
        remaining.push((100.0 - primary.used_percent).floor() as i64);
    }
    if let Some(secondary) = snapshot.secondary.as_ref() {
        remaining.push((100.0 - secondary.used_percent).floor() as i64);
    }
    snapshot
        .credits
        .as_ref()
        .filter(|credits| !credits.unlimited && !credits.has_credits)
        .map(|_| remaining.push(0));
    if remaining.is_empty() {
        None
    } else {
        Some(remaining.into_iter().min().unwrap_or(0))
    }
}

fn latest_reset_at(snapshot: &RateLimitSnapshot) -> Option<i64> {
    let mut reset_at = snapshot
        .primary
        .as_ref()
        .and_then(|window| window.resets_at);
    if let Some(secondary_reset) = snapshot
        .secondary
        .as_ref()
        .and_then(|window| window.resets_at)
    {
        reset_at = Some(reset_at.unwrap_or(secondary_reset).max(secondary_reset));
    }
    reset_at
}

fn window_exhausted(window: &RateLimitWindow) -> bool {
    window.used_percent >= 100.0
}

async fn fetch_rate_limit_snapshots(
    base_url: &str,
    auth: &CodexAuth,
) -> Result<Vec<RateLimitSnapshot>, ChatgptAccountPoolError> {
    let trimmed_base_url = base_url.trim_end_matches('/');
    let path = if trimmed_base_url.contains("/backend-api") {
        format!("{trimmed_base_url}/wham/usage")
    } else {
        format!("{trimmed_base_url}/api/codex/usage")
    };
    let mut headers = HeaderMap::new();
    headers.insert(
        USER_AGENT,
        HeaderValue::from_str(&get_codex_user_agent()).map_err(std::io::Error::other)?,
    );
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!(
            "Bearer {}",
            auth.get_token().map_err(std::io::Error::other)?
        ))
        .map_err(std::io::Error::other)?,
    );
    if let Some(account_id) = auth.get_account_id()
        && let Ok(header_name) = HeaderName::from_bytes(b"ChatGPT-Account-Id")
        && let Ok(header_value) = HeaderValue::from_str(&account_id)
    {
        headers.insert(header_name, header_value);
    }
    if auth.is_fedramp_account()
        && let Ok(header_name) = HeaderName::from_bytes(b"X-OpenAI-Fedramp")
    {
        headers.insert(header_name, HeaderValue::from_static("true"));
    }
    let client = build_reqwest_client_with_custom_ca(reqwest::Client::builder())
        .map_err(std::io::Error::other)?;
    let response = client
        .get(&path)
        .headers(headers)
        .send()
        .await
        .map_err(std::io::Error::other)?;
    if !response.status().is_success() {
        return Err(ChatgptAccountPoolError::Io(std::io::Error::other(format!(
            "rate-limit refresh failed with status {}",
            response.status()
        ))));
    }
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string();
    let body = response.text().await.map_err(std::io::Error::other)?;
    let payload = serde_json::from_str::<RateLimitStatusPayload>(&body).map_err(|err| {
        std::io::Error::other(format!(
            "failed to decode rate-limit payload: {err}; content-type={content_type}; body={body}"
        ))
    })?;
    Ok(rate_limit_snapshots_from_payload(payload))
}

fn rate_limit_snapshots_from_payload(payload: RateLimitStatusPayload) -> Vec<RateLimitSnapshot> {
    let plan_type = Some(map_plan_type(payload.plan_type));
    let rate_limit_reached_type = payload
        .rate_limit_reached_type
        .flatten()
        .and_then(|details| map_rate_limit_reached_type(details.kind));
    let mut snapshots = vec![make_rate_limit_snapshot(
        Some("codex".to_string()),
        /*limit_name*/ None,
        payload.rate_limit.flatten().map(|details| *details),
        payload.credits.flatten().map(|details| *details),
        plan_type.clone(),
        rate_limit_reached_type,
    )];
    if let Some(additional) = payload.additional_rate_limits.flatten() {
        snapshots.extend(additional.into_iter().map(|details| {
            make_rate_limit_snapshot(
                Some(details.metered_feature),
                Some(details.limit_name),
                details.rate_limit.flatten().map(|rate_limit| *rate_limit),
                /*credits*/ None,
                plan_type.clone(),
                /*rate_limit_reached_type*/ None,
            )
        }));
    }
    snapshots
}

fn make_rate_limit_snapshot(
    limit_id: Option<String>,
    limit_name: Option<String>,
    rate_limit: Option<BackendRateLimitStatusDetails>,
    credits: Option<CreditStatusDetails>,
    plan_type: Option<AccountPlanType>,
    rate_limit_reached_type: Option<RateLimitReachedType>,
) -> RateLimitSnapshot {
    let (primary, secondary) = match rate_limit {
        Some(details) => (
            map_rate_limit_window(details.primary_window),
            map_rate_limit_window(details.secondary_window),
        ),
        None => (None, None),
    };
    RateLimitSnapshot {
        limit_id,
        limit_name,
        primary,
        secondary,
        credits: map_credits(credits),
        plan_type,
        rate_limit_reached_type,
    }
}

fn map_rate_limit_reached_type(kind: BackendRateLimitReachedKind) -> Option<RateLimitReachedType> {
    match kind {
        BackendRateLimitReachedKind::RateLimitReached => {
            Some(RateLimitReachedType::RateLimitReached)
        }
        BackendRateLimitReachedKind::WorkspaceOwnerCreditsDepleted => {
            Some(RateLimitReachedType::WorkspaceOwnerCreditsDepleted)
        }
        BackendRateLimitReachedKind::WorkspaceMemberCreditsDepleted => {
            Some(RateLimitReachedType::WorkspaceMemberCreditsDepleted)
        }
        BackendRateLimitReachedKind::WorkspaceOwnerUsageLimitReached => {
            Some(RateLimitReachedType::WorkspaceOwnerUsageLimitReached)
        }
        BackendRateLimitReachedKind::WorkspaceMemberUsageLimitReached => {
            Some(RateLimitReachedType::WorkspaceMemberUsageLimitReached)
        }
        BackendRateLimitReachedKind::Unknown => None,
    }
}

fn map_rate_limit_window(
    window: Option<Option<Box<RateLimitWindowSnapshot>>>,
) -> Option<RateLimitWindow> {
    let snapshot = window.flatten().map(|details| *details)?;
    Some(RateLimitWindow {
        used_percent: f64::from(snapshot.used_percent),
        window_minutes: window_minutes_from_seconds(snapshot.limit_window_seconds),
        resets_at: Some(i64::from(snapshot.reset_at)),
    })
}

fn map_credits(
    credits: Option<CreditStatusDetails>,
) -> Option<codex_protocol::protocol::CreditsSnapshot> {
    let details = credits?;
    Some(codex_protocol::protocol::CreditsSnapshot {
        has_credits: details.has_credits,
        unlimited: details.unlimited,
        balance: details.balance.flatten(),
    })
}

fn map_plan_type(plan_type: BackendPlanType) -> AccountPlanType {
    match plan_type {
        BackendPlanType::Free => AccountPlanType::Free,
        BackendPlanType::Go => AccountPlanType::Go,
        BackendPlanType::Plus => AccountPlanType::Plus,
        BackendPlanType::Pro => AccountPlanType::Pro,
        BackendPlanType::ProLite => AccountPlanType::ProLite,
        BackendPlanType::Team => AccountPlanType::Team,
        BackendPlanType::SelfServeBusinessUsageBased => {
            AccountPlanType::SelfServeBusinessUsageBased
        }
        BackendPlanType::Business => AccountPlanType::Business,
        BackendPlanType::EnterpriseCbpUsageBased => AccountPlanType::EnterpriseCbpUsageBased,
        BackendPlanType::Enterprise => AccountPlanType::Enterprise,
        BackendPlanType::Edu | BackendPlanType::Education => AccountPlanType::Edu,
        BackendPlanType::Guest
        | BackendPlanType::FreeWorkspace
        | BackendPlanType::Quorum
        | BackendPlanType::K12
        | BackendPlanType::Unknown => AccountPlanType::Unknown,
    }
}

fn window_minutes_from_seconds(seconds: i32) -> Option<i64> {
    if seconds <= 0 {
        return None;
    }
    let seconds_i64 = i64::from(seconds);
    Some((seconds_i64 + 59) / 60)
}

#[cfg(test)]
mod tests;
