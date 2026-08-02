mod activity;
mod token_refresh;

use std::collections::BTreeMap;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use chrono::DateTime;
use chrono::Utc;
use codex_backend_openapi_models::models::CreditStatusDetails;
use codex_backend_openapi_models::models::PlanType as BackendPlanType;
use codex_backend_openapi_models::models::RateLimitReachedKind as BackendRateLimitReachedKind;
use codex_backend_openapi_models::models::RateLimitStatusDetails as BackendRateLimitStatusDetails;
use codex_backend_openapi_models::models::RateLimitStatusPayload;
use codex_backend_openapi_models::models::RateLimitWindowSnapshot;
use codex_backend_openapi_models::models::SpendControlStatusDetails as BackendSpendControlStatusDetails;
use codex_client::build_reqwest_client_with_custom_ca;
use codex_http_client::HttpClientFactory;
use codex_http_client::OutboundProxyPolicy;
use codex_http_client::with_chatgpt_cloudflare_cookie_store;
use codex_protocol::account::PlanType as AccountPlanType;
use codex_protocol::auth::AuthMode;
use codex_protocol::protocol::RateLimitReachedType;
use codex_protocol::protocol::RateLimitSnapshot;
use codex_protocol::protocol::RateLimitWindow;
use codex_protocol::protocol::SpendControlLimitSnapshot;
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
use sqlx::Sqlite;
use sqlx::SqlitePool;
use sqlx::Transaction;
use sqlx::sqlite::SqliteRow;

use crate::AuthCredentialsStoreMode;
use crate::AuthDotJson;
use crate::AuthKeyringBackendKind;
use crate::AuthRouteConfig;
use crate::CodexAuth;
use crate::default_client::get_codex_user_agent;
use crate::default_client::originator;
use crate::http_body::MAX_AUTH_ERROR_BODY_BYTES;
use crate::http_body::MAX_AUTH_SUCCESS_BODY_BYTES;
use crate::http_body::read_response_text_limited;
use crate::load_auth_dot_json;
use crate::token_data::IdTokenInfo;
use crate::token_data::TokenData;
use crate::token_data::derive_pool_account_id;
use crate::token_data::parse_chatgpt_jwt_claims;

const DEFAULT_CHATGPT_BACKEND_BASE_URL: &str = "https://chatgpt.com/backend-api";
const POOL_DB_DIR: &str = "account-pool";
const POOL_DB_FILE: &str = "accounts.sqlite";
const EVENT_LIMIT_DEFAULT: i64 = 100;
const EVENT_RETENTION_LIMIT: i64 = 2_000;
const USAGE_HISTORY_RETENTION_PER_ACCOUNT: i64 = 2_000;
const ACCOUNT_POOL_SCHEMA_VERSION: &str = "3";
const ACCOUNT_POOL_SCHEMA_VERSION_NUMBER: u64 = 3;
const TOKEN_RECOVERY_DIRECTORY: &str = "token-recovery";
const CREDENTIAL_LOCK_RETRY_DELAY: Duration = Duration::from_millis(100);
const CREDENTIAL_LOCK_WAIT_GRACE: Duration = Duration::from_secs(5);
static CREDENTIAL_LOCK_SEQUENCE: AtomicU64 = AtomicU64::new(1);

struct RateLimitIdentity {
    id: Option<String>,
    name: Option<String>,
}

fn is_known_accounts_column(name: &str) -> bool {
    matches!(
        name,
        "account_id"
            | "workspace_account_id"
            | "member_identity_key"
            | "chatgpt_user_id"
            | "subject"
            | "email"
            | "plan_type"
            | "enabled"
            | "created_at"
            | "updated_at"
            | "last_used_at"
            | "last_auth_refresh_at"
            | "auth_status"
            | "cooldown_until"
            | "cooldown_reason"
            | "access_token"
            | "refresh_token"
            | "id_token"
            | "agent_identity"
            // Schema v1 column removed by the migration below.
            | "is_default"
    )
}

/// Returns the path to the account-pool SQLite database for the given
/// `codex_home`. External services that append accounts while the CLI is
/// running should open this file with WAL journal mode and write through the
/// same schema (use [`ChatgptAccountPool::open`] from the same crate). The
/// database contains token material as well as account metadata and must remain
/// private to the owning user.
pub fn account_pool_db_path(codex_home: &Path) -> PathBuf {
    pool_db_path(codex_home)
}

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
    #[error("managed ChatGPT auth has invalid {0}")]
    InvalidMetadata(&'static str),
    #[error("managed ChatGPT token account id does not match its ID token claim")]
    AccountIdMismatch,
    #[error("managed ChatGPT token identity does not match account-pool binding for {0}")]
    CredentialIdentityMismatch(String),
    #[error("only managed ChatGPT OAuth accounts can be stored in the account pool")]
    UnsupportedAuthMode,
    #[error("account not found: {0}")]
    AccountNotFound(String),
    #[error("account credential is locked by another process: {0}")]
    CredentialLocked(String),
    #[error("no eligible ChatGPT accounts are available")]
    NoEligibleAccounts,
    #[error(
        "account-pool schema version {found:?} is incompatible with supported version {supported}"
    )]
    IncompatibleSchemaVersion {
        found: String,
        supported: &'static str,
    },
    #[error("account-pool accounts table has unknown NOT NULL columns without defaults: {columns}")]
    IncompatibleAccountsTable { columns: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatgptAccountPoolAuthStatus {
    Valid,
    PendingValidation,
    MissingSecret,
    Invalid,
    /// Legacy terminal state written by older CLI builds. New permanent-auth-failure
    /// writes use `Invalid` instead. Kept here so existing DB records round-trip
    /// correctly; the Go poller and Rust selection logic already treat it as terminal.
    RefreshFailedPermanent,
}

impl ChatgptAccountPoolAuthStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Valid => "valid",
            Self::PendingValidation => "pending_validation",
            Self::MissingSecret => "missing_secret",
            Self::Invalid => "invalid",
            Self::RefreshFailedPermanent => "refresh_failed_permanent",
        }
    }

    fn from_db(value: &str) -> Self {
        match value {
            "valid" => Self::Valid,
            "pending_validation" => Self::PendingValidation,
            "missing_secret" => Self::MissingSecret,
            "invalid" => Self::Invalid,
            "refresh_failed_permanent" => Self::RefreshFailedPermanent,
            _ => {
                tracing::warn!(
                    auth_status = value,
                    "unknown auth_status in DB; treating as pending_validation"
                );
                Self::PendingValidation
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChatgptAccountPoolAccount {
    pub account_id: String,
    pub workspace_account_id: String,
    pub member_identity_key: Option<String>,
    pub chatgpt_user_id: Option<String>,
    pub subject: Option<String>,
    pub email: Option<String>,
    pub plan_type: Option<String>,
    pub enabled: bool,
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
    /// Which binary recorded the event (e.g. `codex-cli:<host>:<pid>` or
    /// `codex-accounts:<host>:<pid>`). `None` for rows written by builds that
    /// predate the column.
    pub actor: Option<String>,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChatgptAccountPoolRateLimitEntry {
    pub account_id: String,
    /// Latest fetch time across all stored limit buckets for this account.
    pub fetched_at: Option<i64>,
    /// Exact fetch time for each entry in `rate_limits`.
    ///
    /// A request-side observation can update one limit bucket without refreshing
    /// the others, so consumers must prefer this map when displaying freshness.
    pub fetched_at_by_limit_id: BTreeMap<String, i64>,
    pub rate_limits: BTreeMap<String, RateLimitSnapshot>,
}

#[derive(Debug, Default)]
struct StoredRateLimitGroup {
    latest_fetched_at: Option<i64>,
    fetched_at_by_limit_id: BTreeMap<String, i64>,
    rate_limits: BTreeMap<String, RateLimitSnapshot>,
}

#[derive(Debug, Clone, PartialEq)]
#[expect(
    clippy::large_enum_variant,
    reason = "boxing Activated.auth would change the public login API"
)]
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
    keyring_backend_kind: AuthKeyringBackendKind,
    auth_route_config: AuthRouteConfig,
    chatgpt_base_url: String,
    pool: SqlitePool,
    /// Value stamped into `account_events.actor` for every event this CLI writes,
    /// so it is always clear the CLI (not codex-accounts) performed the action.
    actor: String,
}

impl std::fmt::Debug for ChatgptAccountPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChatgptAccountPool")
            .field("codex_home", &self.codex_home)
            .field(
                "auth_credentials_store_mode",
                &self.auth_credentials_store_mode,
            )
            .field("keyring_backend_kind", &self.keyring_backend_kind)
            .field("auth_route_config", &self.auth_route_config)
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
        Self::open_with_auth_config(
            codex_home,
            auth_credentials_store_mode,
            chatgpt_base_url,
            AuthKeyringBackendKind::default(),
            AuthRouteConfig::from_http_client_factory(HttpClientFactory::new(
                OutboundProxyPolicy::ReqwestDefault,
            )),
        )
        .await
    }

    pub(crate) async fn open_with_auth_config(
        codex_home: PathBuf,
        auth_credentials_store_mode: AuthCredentialsStoreMode,
        chatgpt_base_url: Option<String>,
        keyring_backend_kind: AuthKeyringBackendKind,
        auth_route_config: AuthRouteConfig,
    ) -> Result<Self, ChatgptAccountPoolError> {
        if auth_credentials_store_mode != AuthCredentialsStoreMode::File {
            tracing::warn!(
                auth_credentials_store_mode = ?auth_credentials_store_mode,
                "ChatGPT account-pool tokens are persisted in the shared SQLite database \
                regardless of the configured credential store mode"
            );
        }
        let pool_root = pool_root_dir(&codex_home);
        std::fs::create_dir_all(&pool_root)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            std::fs::set_permissions(&pool_root, std::fs::Permissions::from_mode(0o700))?;
        }
        let pool =
            codex_state::open_durable_single_connection_sqlite_pool(&pool_db_path(&codex_home))
                .await?;
        let this = Self {
            codex_home,
            auth_credentials_store_mode,
            keyring_backend_kind,
            auth_route_config,
            chatgpt_base_url: chatgpt_base_url
                .unwrap_or_else(|| DEFAULT_CHATGPT_BACKEND_BASE_URL.to_string()),
            pool,
            actor: cli_actor(),
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
                workspace_account_id,
                member_identity_key,
                chatgpt_user_id,
                subject,
                email,
                plan_type,
                enabled,
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
        let grouped_rate_limits = self
            .load_rate_limits_grouped(/*only_account_id*/ None)
            .await?;
        Ok(rows
            .iter()
            .map(|row| account_from_row(row, selected_account_id.as_deref(), &grouped_rate_limits))
            .collect())
    }

    pub async fn list_events(
        &self,
        limit: Option<i64>,
    ) -> Result<Vec<ChatgptAccountEvent>, ChatgptAccountPoolError> {
        let rows = sqlx::query(
            r#"
            SELECT account_id, event_type, message, actor, created_at
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
                actor: row.get("actor"),
                created_at: row.get("created_at"),
            })
            .collect())
    }

    pub async fn list_rate_limits(
        &self,
    ) -> Result<Vec<ChatgptAccountPoolRateLimitEntry>, ChatgptAccountPoolError> {
        let grouped_rate_limits = self
            .load_rate_limits_grouped_with_fetch_time(/*only_account_id*/ None)
            .await?;
        Ok(grouped_rate_limits
            .into_iter()
            .map(|(account_id, group)| ChatgptAccountPoolRateLimitEntry {
                account_id,
                fetched_at: group.latest_fetched_at,
                fetched_at_by_limit_id: group.fetched_at_by_limit_id,
                rate_limits: group.rate_limits,
            })
            .collect())
    }

    /// Registers (or refreshes) a managed ChatGPT account in the pool.
    ///
    /// External account services and integration tests use this to seed the
    /// pool with credentials before the CLI opens [`ChatgptAccountPool::open`].
    pub async fn register_account(
        &self,
        auth: &AuthDotJson,
    ) -> Result<ChatgptAccountPoolAccount, ChatgptAccountPoolError> {
        self.register_account_with_superseded_refresh_token(auth)
            .await
            .map(|(account, _)| account)
    }

    /// Registers an account while serializing replacement of its OAuth
    /// credential with the service-owned refresh path.
    ///
    /// The returned token is the prior refresh token only when this commit
    /// superseded a different one. Callers that are completing an interactive
    /// login can revoke it after the new credential is safely committed.
    pub(crate) async fn register_account_with_superseded_refresh_token(
        &self,
        auth: &AuthDotJson,
    ) -> Result<(ChatgptAccountPoolAccount, Option<String>), ChatgptAccountPoolError> {
        let metadata = extract_chatgpt_metadata(auth)?;
        let tokens = auth
            .tokens
            .as_ref()
            .filter(|tokens| !tokens.access_token.is_empty() && !tokens.refresh_token.is_empty())
            .ok_or(ChatgptAccountPoolError::UnsupportedAuthMode)?;

        let account_id = metadata.account_id.clone();
        let lock_owner = self
            .acquire_credential_lock_waiting(&account_id, "register")
            .await?;
        let result = self
            .register_account_while_credential_locked(auth, metadata, tokens)
            .await;
        if let Err(err) = self
            .release_token_refresh_lock(&account_id, &lock_owner)
            .await
        {
            // The operation result remains authoritative. A failed cleanup
            // leaves only a bounded-TTL advisory lock; reporting a false write
            // failure would encourage replay of an already-committed login.
            tracing::warn!(%err, "failed to release account registration credential lock");
        }
        result
    }

    pub(crate) async fn acquire_credential_lock_waiting(
        &self,
        account_id: &str,
        operation: &str,
    ) -> Result<String, ChatgptAccountPoolError> {
        let lock_sequence = CREDENTIAL_LOCK_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let lock_owner = format!("{}:{operation}:{lock_sequence}", self.actor);
        let lock_deadline = tokio::time::Instant::now()
            + Self::token_refresh_lock_ttl()
            + CREDENTIAL_LOCK_WAIT_GRACE;
        loop {
            if self
                .try_acquire_token_refresh_lock(
                    account_id,
                    &lock_owner,
                    Self::token_refresh_lock_ttl(),
                )
                .await?
            {
                return Ok(lock_owner);
            }
            if tokio::time::Instant::now() >= lock_deadline {
                return Err(ChatgptAccountPoolError::CredentialLocked(
                    account_id.to_string(),
                ));
            }
            tokio::time::sleep(CREDENTIAL_LOCK_RETRY_DELAY).await;
        }
    }

    async fn register_account_while_credential_locked(
        &self,
        auth: &AuthDotJson,
        metadata: ChatgptAccountMetadata,
        tokens: &TokenData,
    ) -> Result<(ChatgptAccountPoolAccount, Option<String>), ChatgptAccountPoolError> {
        // A service crash may leave a durable pending rotation. This interactive
        // login is a new authoritative credential and supersedes that chain.
        // The shared lock guarantees the service is not writing this journal.
        self.clear_superseded_token_recovery_journal(&metadata.account_id)?;

        let now = now_ts();
        let auth_status = ChatgptAccountPoolAuthStatus::Valid.as_str();
        // We must inspect the prior refresh token and then replace it as one
        // unit. BEGIN IMMEDIATE reserves SQLite's writer slot before that read;
        // a deferred transaction lets two first-account registrations both
        // establish read snapshots and makes the later INSERT fail with
        // SQLITE_BUSY_SNAPSHOT instead of honoring busy_timeout.
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let previous_refresh_token_row = sqlx::query_scalar::<_, Option<String>>(
            "SELECT refresh_token FROM accounts WHERE account_id = ? LIMIT 1",
        )
        .bind(&metadata.account_id)
        .fetch_optional(&mut *tx)
        .await?;
        let replacing_existing = previous_refresh_token_row.is_some();
        let previous_refresh_token = previous_refresh_token_row.flatten();
        let result = sqlx::query(
            r#"
            INSERT INTO accounts (
                account_id,
                workspace_account_id,
                member_identity_key,
                chatgpt_user_id,
                subject,
                email,
                plan_type,
                enabled,
                created_at,
                updated_at,
                last_used_at,
                last_auth_refresh_at,
                auth_status,
                cooldown_until,
                cooldown_reason,
                access_token,
                refresh_token,
                id_token
            )
            VALUES (?, ?, ?, ?, ?, ?, ?, 1, ?, ?, NULL, ?, ?, NULL, NULL, ?, ?, ?)
            ON CONFLICT(account_id) DO UPDATE SET
                workspace_account_id = excluded.workspace_account_id,
                member_identity_key = excluded.member_identity_key,
                chatgpt_user_id = excluded.chatgpt_user_id,
                subject = excluded.subject,
                email = excluded.email,
                plan_type = excluded.plan_type,
                enabled = 1,
                updated_at = excluded.updated_at,
                last_auth_refresh_at = excluded.last_auth_refresh_at,
                auth_status = excluded.auth_status,
                access_token = excluded.access_token,
                refresh_token = excluded.refresh_token,
                id_token = excluded.id_token
            WHERE accounts.workspace_account_id = excluded.workspace_account_id
              AND (
                    accounts.member_identity_key IS NULL
                    OR accounts.member_identity_key IS excluded.member_identity_key
                  )
            "#,
        )
        .bind(&metadata.account_id)
        .bind(&metadata.workspace_account_id)
        .bind(&metadata.member_identity_key)
        .bind(&metadata.chatgpt_user_id)
        .bind(&metadata.subject)
        .bind(&metadata.email)
        .bind(&metadata.plan_type)
        .bind(now)
        .bind(now)
        .bind(auth.last_refresh.map(|value| value.timestamp()))
        .bind(auth_status)
        .bind(&tokens.access_token)
        .bind(&tokens.refresh_token)
        .bind(&tokens.id_token.raw_jwt)
        .execute(&mut *tx)
        .await?;
        if result.rows_affected() == 0 {
            tx.rollback().await?;
            return Err(ChatgptAccountPoolError::CredentialIdentityMismatch(
                metadata.account_id,
            ));
        }
        // Never replace a selection another writer committed while this
        // immediate transaction waited for SQLite's writer slot.
        sqlx::query(
            r#"
            INSERT INTO pool_state (key, value)
            VALUES ('selected_account_id', ?)
            ON CONFLICT(key) DO UPDATE SET value = excluded.value
            WHERE pool_state.value IS NULL
            "#,
        )
        .bind(&metadata.account_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO account_events (account_id, event_type, message, actor, created_at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&metadata.account_id)
        .bind(if replacing_existing {
            "account_reauthenticated"
        } else {
            "account_registered"
        })
        .bind(if replacing_existing {
            format!(
                "Reauthenticated ChatGPT account {}",
                account_suffix(&metadata.workspace_account_id)
            )
        } else {
            format!(
                "Registered ChatGPT account {}",
                account_suffix(&metadata.workspace_account_id)
            )
        })
        .bind(self.actor.as_str())
        .bind(now)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "DELETE FROM account_events WHERE id IN (SELECT id FROM account_events ORDER BY id DESC LIMIT -1 OFFSET ?)",
        )
        .bind(EVENT_RETENTION_LIMIT)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        let superseded_refresh_token = previous_refresh_token
            .filter(|previous| !previous.is_empty() && previous != &tokens.refresh_token);
        let account = self.account_by_id(&metadata.account_id).await?.ok_or(
            ChatgptAccountPoolError::AccountNotFound(metadata.account_id),
        )?;
        Ok((account, superseded_refresh_token))
    }

    fn clear_superseded_token_recovery_journal(
        &self,
        account_id: &str,
    ) -> Result<(), ChatgptAccountPoolError> {
        let digest = format!("{:x}", Sha256::digest(account_id.as_bytes()));
        let path = pool_root_dir(&self.codex_home)
            .join(TOKEN_RECOVERY_DIRECTORY)
            .join(format!("{}.json", &digest[..16]));
        match std::fs::remove_file(&path) {
            Ok(()) => {
                #[cfg(unix)]
                if let Some(parent) = path.parent() {
                    std::fs::File::open(parent)?.sync_all()?;
                }
                Ok(())
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err.into()),
        }
    }

    pub(crate) async fn record_superseded_credential_revocation(
        &self,
        account_id: &str,
        error: Option<&str>,
    ) -> Result<(), ChatgptAccountPoolError> {
        let (event_type, message) = match error {
            Some(_) => (
                "superseded_credential_revoke_failed",
                "Replacement credential committed, but superseded OAuth refresh-token revocation failed"
                    .to_string(),
            ),
            None => (
                "superseded_credential_revoked",
                "Superseded OAuth refresh token revoked after interactive reauthentication"
                    .to_string(),
            ),
        };
        self.append_event(Some(account_id), event_type, message)
            .await
    }

    pub async fn selected_account_auth(
        &self,
    ) -> Result<Option<(String, AuthDotJson)>, ChatgptAccountPoolError> {
        let Some(account_id) = self.selected_account_id().await? else {
            return Ok(None);
        };
        match self.load_account_secret(&account_id).await? {
            Some(auth) => Ok(Some((account_id, auth))),
            None => {
                self.mark_account_missing_secret(&account_id).await?;
                Ok(None)
            }
        }
    }

    #[cfg(test)]
    async fn select_account(
        &self,
        account_id: &str,
    ) -> Result<ChatgptAccountPoolAccount, ChatgptAccountPoolError> {
        self.set_selected_account_id_with_event(
            account_id,
            "account_selected",
            format!("Selected ChatGPT account {}", account_suffix(account_id)),
        )
        .await?;
        self.account_by_id(account_id)
            .await?
            .ok_or_else(|| ChatgptAccountPoolError::AccountNotFound(account_id.to_string()))
    }

    pub async fn resolve_turn_selection(
        &self,
        current_account_id: Option<&str>,
        _current_refresh_failed_permanently: bool,
    ) -> Result<ChatgptAccountPoolSelectionOutcome, ChatgptAccountPoolError> {
        let mut selected_account_id = self.selected_account_id().await?;
        let mut accounts = self.list_accounts().await?;
        if accounts.is_empty() {
            return Ok(if current_account_id.is_some() {
                ChatgptAccountPoolSelectionOutcome::NoEligibleAccounts
            } else {
                ChatgptAccountPoolSelectionOutcome::Unchanged
            });
        }
        // Each iteration either returns, marks one account as missing_secret, or
        // probes one pending_validation account exactly once (tracked below); cap
        // iterations at 3× the pool size to guard against unexpected DB races.
        let max_retries = (accounts.len() * 3).max(8);
        let mut retries = 0usize;
        // Pending accounts already probed this call, so a transient probe failure
        // never causes us to re-probe the same account in a tight loop.
        let mut probed_pending: HashSet<String> = HashSet::new();
        loop {
            if retries >= max_retries {
                tracing::warn!(
                    "resolve_turn_selection: exceeded max retries ({max_retries}); giving up"
                );
                return Ok(ChatgptAccountPoolSelectionOutcome::NoEligibleAccounts);
            }
            retries += 1;
            let now = now_ts();
            if let Some(selected_id) = selected_account_id.as_deref()
                && let Some(selected_account) = accounts
                    .iter()
                    .find(|account| account.account_id == selected_id)
                && is_account_eligible(selected_account, now)
            {
                if current_account_id == Some(selected_id) {
                    return Ok(ChatgptAccountPoolSelectionOutcome::Unchanged);
                }
                let failover = current_account_id.is_some_and(|current| current != selected_id);
                match self.activate_account(selected_id, failover).await? {
                    ChatgptAccountPoolSelectionOutcome::NoEligibleAccounts => {
                        selected_account_id = self.selected_account_id().await?;
                        accounts = self.list_accounts().await?;
                        continue;
                    }
                    outcome => return Ok(outcome),
                }
            }

            // Score valid accounts and not-yet-validated accounts together.
            // A pending_validation account is treated as having full capacity
            // (see `capacity_score`) so the scorer prefers bringing fresh
            // capacity online over reusing an idle valid account.
            let Some(best_account_id) = select_best_candidate(&accounts, now, &probed_pending)
            else {
                return Ok(ChatgptAccountPoolSelectionOutcome::NoEligibleAccounts);
            };
            let best_account_id = best_account_id.to_string();

            // If the scorer picked a not-yet-validated account, validate it on
            // pickup: probe /usage, store the snapshot, and promote it to valid
            // (or mark it invalid on a 401). The next pass then rescores with
            // its real capacity, selecting it only if it is actually usable.
            if accounts.iter().any(|account| {
                account.account_id == best_account_id
                    && matches!(
                        account.auth_status,
                        ChatgptAccountPoolAuthStatus::PendingValidation
                    )
            }) {
                probed_pending.insert(best_account_id.clone());
                self.validate_pending_account(&best_account_id).await?;
                selected_account_id = self.selected_account_id().await?;
                accounts = self.list_accounts().await?;
                continue;
            }

            let best_account_id = best_account_id.as_str();
            let failover = current_account_id.is_some_and(|current| current != best_account_id);
            if selected_account_id.as_deref() != Some(best_account_id) {
                let (event_type, message) = if failover {
                    (
                        "account_failover_selected",
                        format!(
                            "Selected fallback ChatGPT account {}",
                            account_suffix(best_account_id)
                        ),
                    )
                } else {
                    (
                        "account_selected",
                        format!(
                            "Selected ChatGPT account {}",
                            account_suffix(best_account_id)
                        ),
                    )
                };
                if !self
                    .compare_and_set_selected_account_id_with_event(
                        selected_account_id.as_deref(),
                        best_account_id,
                        event_type,
                        message,
                    )
                    .await?
                {
                    // Another process changed selection or candidate eligibility
                    // after this process took its scoring snapshot. Follow the
                    // committed winner instead of overwriting it.
                    selected_account_id = self.selected_account_id().await?;
                    accounts = self.list_accounts().await?;
                    continue;
                }
            }
            if current_account_id == Some(best_account_id) {
                return Ok(ChatgptAccountPoolSelectionOutcome::Unchanged);
            }
            match self.activate_account(best_account_id, failover).await? {
                ChatgptAccountPoolSelectionOutcome::NoEligibleAccounts => {
                    selected_account_id = self.selected_account_id().await?;
                    accounts = self.list_accounts().await?;
                }
                outcome => return Ok(outcome),
            }
        }
    }

    pub async fn mark_current_account_rate_limited(
        &self,
        account_id: &str,
        snapshot: Option<&RateLimitSnapshot>,
        resets_at: Option<DateTime<Utc>>,
    ) -> Result<(), ChatgptAccountPoolError> {
        let now = now_ts();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        self.require_account_tx(&mut tx, account_id).await?;
        // A non-codex usage limit (e.g. "premium" / workspace-credits depleted) does
        // NOT consume the "codex" quota the CLI actually runs on. Cooling the account
        // down for it would pull a healthy account (codex quota fine) out of rotation
        // and cascade failover across the whole pool — the "innocent active account
        // marked rate-limited" bug. So when the snapshot identifies a non-codex limit,
        // persist it for visibility but leave cooldown/eligibility untouched. Only the
        // codex limit (limit_id "codex"/unset) — or a bare resets_at with no snapshot —
        // is authoritative for account cooldown.
        if let Some(snapshot) = snapshot
            && !snapshot_contributes_account_cooldown(snapshot)
        {
            self.store_rate_limit_snapshot_tx(&mut tx, account_id, snapshot, now)
                .await?;
            self.append_event_tx(
                &mut tx,
                Some(account_id),
                "rate_limit_reached_non_codex",
                format!(
                    "Non-codex usage limit ({}) hit for ChatGPT account {}; codex quota unaffected, not cooling down",
                    snapshot.limit_id.as_deref().unwrap_or("unknown"),
                    account_suffix(account_id)
                ),
                now,
            )
            .await?;
            tx.commit().await?;
            return Ok(());
        }

        let mut cooldown_until = resets_at.map(|value| value.timestamp());
        if let Some(snapshot) = snapshot {
            if let Some(snapshot_cooldown_until) = cooldown_until_from_snapshot(snapshot, now, true)
            {
                cooldown_until = Some(
                    cooldown_until
                        .unwrap_or(snapshot_cooldown_until)
                        .max(snapshot_cooldown_until),
                );
            }
            self.store_rate_limit_snapshot_tx(&mut tx, account_id, snapshot, now)
                .await?;
        }
        // Codex-authoritative limit with no explicit reset: conservative 1-hour fallback
        // so we don't immediately hammer an account whose codex quota is unknown.
        let cooldown_until = cooldown_until.unwrap_or_else(|| now.saturating_add(3600));
        sqlx::query(
            r#"
            UPDATE accounts
            SET updated_at = ?,
                cooldown_until = MAX(COALESCE(cooldown_until, 0), ?),
                cooldown_reason = ?
            WHERE account_id = ?
            "#,
        )
        .bind(now)
        .bind(cooldown_until)
        .bind("usage_limit_reached")
        .bind(account_id)
        .execute(&mut *tx)
        .await?;
        self.append_event_tx(
            &mut tx,
            Some(account_id),
            "rate_limit_reached",
            format!(
                "Rate limit reached for ChatGPT account {}; cooldown until {cooldown_until} (reason: usage_limit_reached)",
                account_suffix(account_id)
            ),
            now,
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn mark_account_auth_failed(
        &self,
        account_id: &str,
        reason: &str,
    ) -> Result<(), ChatgptAccountPoolError> {
        self.update_auth_status_with_event(
            account_id,
            ChatgptAccountPoolAuthStatus::Invalid,
            "auth_failure_permanent",
            format!(
                "Permanent auth failure for ChatGPT account {}: {reason}",
                account_suffix(account_id)
            ),
        )
        .await
    }

    /// Marks an auth failure only while the database still contains the exact
    /// credential that produced it.
    ///
    /// codex-accounts may rotate or replace a credential between an API request
    /// and the CLI's error handler. An unconditional status write in that window
    /// would invalidate the repaired credential. `false` means the account still
    /// exists but its credential changed before this transaction acquired the
    /// SQLite writer slot.
    pub(crate) async fn mark_account_auth_failed_if_credential_matches(
        &self,
        account_id: &str,
        expected_auth: &AuthDotJson,
        reason: &str,
    ) -> Result<bool, ChatgptAccountPoolError> {
        let tokens = expected_auth
            .tokens
            .as_ref()
            .ok_or(ChatgptAccountPoolError::UnsupportedAuthMode)?;
        let now = now_ts();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let result = sqlx::query(
            r#"
            UPDATE accounts
            SET auth_status = ?,
                updated_at = ?
            WHERE account_id = ?
              AND access_token = ?
              AND COALESCE(refresh_token, '') = ?
              AND COALESCE(id_token, '') = ?
            "#,
        )
        .bind(ChatgptAccountPoolAuthStatus::Invalid.as_str())
        .bind(now)
        .bind(account_id)
        .bind(&tokens.access_token)
        .bind(&tokens.refresh_token)
        .bind(&tokens.id_token.raw_jwt)
        .execute(&mut *tx)
        .await?;
        if result.rows_affected() == 0 {
            self.require_account_tx(&mut tx, account_id).await?;
            tx.rollback().await?;
            return Ok(false);
        }
        self.append_event_tx(
            &mut tx,
            Some(account_id),
            "auth_failure_permanent",
            format!(
                "Permanent auth failure for ChatGPT account {}: {reason}",
                account_suffix(account_id)
            ),
            now,
        )
        .await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Applies a retryable-auth cooldown only to the credential that actually
    /// failed. See [`Self::mark_account_auth_failed_if_credential_matches`].
    pub(crate) async fn mark_account_auth_retryable_if_credential_matches(
        &self,
        account_id: &str,
        expected_auth: &AuthDotJson,
        reason: &str,
    ) -> Result<bool, ChatgptAccountPoolError> {
        let tokens = expected_auth
            .tokens
            .as_ref()
            .ok_or(ChatgptAccountPoolError::UnsupportedAuthMode)?;
        let now = now_ts();
        let cooldown_until = now.saturating_add(300);
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let result = sqlx::query(
            r#"
            UPDATE accounts
            SET updated_at = ?,
                cooldown_until = MAX(COALESCE(cooldown_until, 0), ?),
                cooldown_reason = ?
            WHERE account_id = ?
              AND access_token = ?
              AND COALESCE(refresh_token, '') = ?
              AND COALESCE(id_token, '') = ?
              AND auth_status NOT IN ('invalid', 'missing_secret', 'refresh_failed_permanent')
            "#,
        )
        .bind(now)
        .bind(cooldown_until)
        .bind("auth_failure_retryable")
        .bind(account_id)
        .bind(&tokens.access_token)
        .bind(&tokens.refresh_token)
        .bind(&tokens.id_token.raw_jwt)
        .execute(&mut *tx)
        .await?;
        if result.rows_affected() == 0 {
            self.require_account_tx(&mut tx, account_id).await?;
            tx.rollback().await?;
            return Ok(false);
        }
        self.append_event_tx(
            &mut tx,
            Some(account_id),
            "auth_failure_retryable",
            format!(
                "Retryable auth failure for ChatGPT account {}; cooldown until {cooldown_until}: {reason}",
                account_suffix(account_id)
            ),
            now,
        )
        .await?;
        tx.commit().await?;
        Ok(true)
    }

    pub(crate) async fn disable_all_accounts_for_logout(
        &self,
    ) -> Result<(), ChatgptAccountPoolError> {
        let now = now_ts();
        let mut tx = self.pool.begin().await?;
        sqlx::query("UPDATE accounts SET enabled = 0, updated_at = ? WHERE enabled != 0")
            .bind(now)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            r#"
            INSERT INTO pool_state (key, value)
            VALUES ('selected_account_id', NULL)
            ON CONFLICT(key) DO UPDATE SET value = NULL
            "#,
        )
        .execute(&mut *tx)
        .await?;
        self.append_event_tx(
            &mut tx,
            None,
            "account_pool_signed_out",
            "Disabled all ChatGPT pool accounts after sign-out".to_string(),
            now,
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub(crate) async fn refresh_token_for_logout(
        &self,
        account_id: &str,
    ) -> Result<Option<String>, ChatgptAccountPoolError> {
        let token = sqlx::query_scalar::<_, Option<String>>(
            "SELECT refresh_token FROM accounts WHERE account_id = ? LIMIT 1",
        )
        .bind(account_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| ChatgptAccountPoolError::AccountNotFound(account_id.to_string()))?
        .filter(|token| !token.is_empty());
        Ok(token)
    }

    pub(crate) async fn record_logout_credential_revocation(
        &self,
        account_id: &str,
        error: Option<&str>,
    ) -> Result<(), ChatgptAccountPoolError> {
        let now = now_ts();
        let mut tx = self.pool.begin().await?;
        self.require_account_tx(&mut tx, account_id).await?;
        let (event_type, message) = match error {
            Some(_) => (
                "logout_credential_revoke_failed",
                "Pool sign-out disabled this account, but OAuth refresh-token revocation failed"
                    .to_string(),
            ),
            None => {
                sqlx::query(
                    "UPDATE accounts SET auth_status = ?, updated_at = ?, cooldown_until = NULL, cooldown_reason = NULL WHERE account_id = ?",
                )
                .bind(ChatgptAccountPoolAuthStatus::Invalid.as_str())
                .bind(now)
                .bind(account_id)
                .execute(&mut *tx)
                .await?;
                (
                    "logout_credential_revoked",
                    "OAuth refresh token revoked during pool sign-out; reauthentication is required"
                        .to_string(),
                )
            }
        };
        self.append_event_tx(&mut tx, Some(account_id), event_type, message, now)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub(crate) async fn probe_token_status(
        &self,
        base_url: Option<&str>,
        auth: &CodexAuth,
    ) -> Option<reqwest::StatusCode> {
        probe_usage_status(base_url.unwrap_or(self.chatgpt_base_url.as_str()), auth).await
    }

    /// Probes the ChatGPT `/usage` endpoint with `auth` and returns `true` if
    /// the token is accepted. On HTTP 401 the account is marked `Invalid` in
    /// the pool so future startups skip it cleanly, and `false` is returned so
    /// the caller can suppress the MCP layer entirely. Network errors and other
    /// non-401 failures return `true` to avoid false positives.
    pub async fn probe_token_or_mark_invalid(&self, account_id: &str, auth: &CodexAuth) -> bool {
        match probe_usage_status(&self.chatgpt_base_url, auth).await {
            Some(status) if status == reqwest::StatusCode::UNAUTHORIZED => {
                tracing::warn!(
                    %account_id,
                    "ChatGPT account token rejected with 401; marking account invalid"
                );
                let _ = self
                    .update_auth_status(account_id, ChatgptAccountPoolAuthStatus::Invalid)
                    .await;
                false
            }
            _ => true,
        }
    }

    pub(crate) async fn record_fetched_rate_limits(
        &self,
        account_id: &str,
        snapshots: &[RateLimitSnapshot],
    ) -> Result<ChatgptAccountPoolRateLimitEntry, ChatgptAccountPoolError> {
        self.record_fetched_rate_limits_started_at(
            account_id,
            snapshots,
            now_ts(),
            /*apply_cooldown*/ true,
        )
        .await
    }

    /// Stores a usage snapshot fetched while bringing a `pending_validation`
    /// account online during failover, promoting it to `valid` — but without
    /// imposing a cooldown. A cooldown here would mark the very account we are
    /// switching *to* as rate-limited (the "fresh failover account also cooled"
    /// bug): a validation probe is a credential check, not real turn usage, and
    /// its `/usage` window can reflect a transient or workspace-shared limit. The
    /// snapshot is still persisted so the capacity scorer can deprioritize an
    /// exhausted candidate; an actual 429 on a turn is what cools the account.
    pub(crate) async fn record_validated_rate_limits(
        &self,
        account_id: &str,
        snapshots: &[RateLimitSnapshot],
    ) -> Result<ChatgptAccountPoolRateLimitEntry, ChatgptAccountPoolError> {
        self.record_fetched_rate_limits_started_at(
            account_id,
            snapshots,
            now_ts(),
            /*apply_cooldown*/ false,
        )
        .await
    }

    async fn record_fetched_rate_limits_started_at(
        &self,
        account_id: &str,
        snapshots: &[RateLimitSnapshot],
        fetch_started_at: i64,
        apply_cooldown: bool,
    ) -> Result<ChatgptAccountPoolRateLimitEntry, ChatgptAccountPoolError> {
        let fetched_at = now_ts();
        let mut cooldown_until = None;
        let mut grouped = BTreeMap::new();
        let mut fetched_at_by_limit_id = BTreeMap::new();
        // Read/compare/write of the latest snapshot is one serialized decision.
        // A deferred read transaction can otherwise fail with BUSY_SNAPSHOT or
        // recreate usage rows after codex-accounts removes the account.
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        self.require_account_tx(&mut tx, account_id).await?;
        let latest_fetched_at = sqlx::query(
            "SELECT MAX(fetched_at) AS fetched_at FROM account_rate_limits WHERE account_id = ?",
        )
        .bind(account_id)
        .fetch_one(&mut *tx)
        .await?
        .get::<Option<i64>, _>("fetched_at");
        let previous_rows = sqlx::query(
            "SELECT limit_id, snapshot_json, fetched_at FROM account_rate_limits WHERE account_id = ?",
        )
        .bind(account_id)
        .fetch_all(&mut *tx)
        .await?;
        let mut previous_grouped = BTreeMap::new();
        let mut previous_fetched_at_by_limit_id = BTreeMap::new();
        for row in previous_rows {
            let limit_id: String = row.get("limit_id");
            let snapshot_json: String = row.get("snapshot_json");
            let previous_fetched_at: i64 = row.get("fetched_at");
            previous_fetched_at_by_limit_id.insert(limit_id.clone(), previous_fetched_at);
            previous_grouped.insert(limit_id, serde_json::from_str(&snapshot_json)?);
        }
        let replace_latest = latest_fetched_at.is_none_or(|latest| latest <= fetch_started_at);
        let should_apply_cooldown = apply_cooldown && replace_latest;
        let before_cooldown_state = sqlx::query(
            "SELECT cooldown_until, cooldown_reason FROM accounts WHERE account_id = ? LIMIT 1",
        )
        .bind(account_id)
        .fetch_optional(&mut *tx)
        .await?
        .map(|row| {
            (
                row.get::<Option<i64>, _>("cooldown_until"),
                row.get::<Option<String>, _>("cooldown_reason"),
            )
        });
        if replace_latest {
            sqlx::query("DELETE FROM account_rate_limits WHERE account_id = ?")
                .bind(account_id)
                .execute(&mut *tx)
                .await?;
        }
        for snapshot in snapshots {
            if should_apply_cooldown
                && let Some(reset_at) = cooldown_until_from_snapshot(snapshot, fetched_at, false)
            {
                cooldown_until = Some(cooldown_until.unwrap_or(reset_at).max(reset_at));
            }

            let limit_id = snapshot
                .limit_id
                .clone()
                .unwrap_or_else(|| "codex".to_string());
            let snapshot_json = serde_json::to_string(snapshot)?;
            if replace_latest {
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
                .bind(limit_id.clone())
                .bind(snapshot_json.clone())
                .bind(fetched_at)
                .execute(&mut *tx)
                .await?;
                fetched_at_by_limit_id.insert(limit_id.clone(), fetched_at);
                grouped.insert(limit_id.clone(), snapshot.clone());
            }
            sqlx::query(
                r#"
                INSERT INTO account_usage_history (account_id, limit_id, snapshot_json, fetched_at)
                VALUES (?, ?, ?, ?)
                "#,
            )
            .bind(account_id)
            .bind(limit_id.clone())
            .bind(snapshot_json)
            .bind(fetched_at)
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query(
            r#"
            DELETE FROM account_usage_history
            WHERE account_id = ?
              AND id IN (
                  SELECT id
                  FROM account_usage_history
                  WHERE account_id = ?
                  ORDER BY id DESC
                  LIMIT -1 OFFSET ?
              )
            "#,
        )
        .bind(account_id)
        .bind(account_id)
        .bind(USAGE_HISTORY_RETENTION_PER_ACCOUNT)
        .execute(&mut *tx)
        .await?;
        if !replace_latest {
            grouped.clone_from(&previous_grouped);
            fetched_at_by_limit_id.clone_from(&previous_fetched_at_by_limit_id);
        }
        sqlx::query(
            r#"
            UPDATE accounts
            SET updated_at = CASE
                    WHEN ? = 0 THEN updated_at
                    ELSE MAX(updated_at, ?)
                END,
                cooldown_until = CASE
                    WHEN ? = 0 THEN cooldown_until
                    WHEN updated_at > ? THEN cooldown_until
                    WHEN ? IS NULL THEN NULL
                    ELSE MAX(COALESCE(cooldown_until, 0), ?)
                END,
                cooldown_reason = CASE
                    WHEN ? = 0 THEN cooldown_reason
                    WHEN updated_at > ? THEN cooldown_reason
                    WHEN ? IS NULL THEN NULL
                    ELSE ?
                END,
                auth_status = CASE
                    WHEN ? = 0 THEN auth_status
                    WHEN updated_at > ? THEN auth_status
                    WHEN auth_status IN ('missing_secret', 'invalid', 'refresh_failed_permanent')
                        THEN auth_status
                    WHEN auth_status = ? THEN ?
                    ELSE auth_status
                END
            WHERE account_id = ?
            "#,
        )
        .bind(i64::from(replace_latest))
        .bind(fetched_at)
        .bind(i64::from(should_apply_cooldown))
        .bind(fetch_started_at)
        .bind(cooldown_until)
        .bind(cooldown_until)
        .bind(i64::from(should_apply_cooldown))
        .bind(fetch_started_at)
        .bind(cooldown_until)
        .bind("rate_limits_refreshed")
        .bind(i64::from(replace_latest))
        .bind(fetch_started_at)
        .bind(ChatgptAccountPoolAuthStatus::PendingValidation.as_str())
        .bind(ChatgptAccountPoolAuthStatus::Valid.as_str())
        .bind(account_id)
        .execute(&mut *tx)
        .await?;
        let after_cooldown_state = sqlx::query(
            "SELECT cooldown_until, cooldown_reason FROM accounts WHERE account_id = ? LIMIT 1",
        )
        .bind(account_id)
        .fetch_optional(&mut *tx)
        .await?
        .map(|row| {
            (
                row.get::<Option<i64>, _>("cooldown_until"),
                row.get::<Option<String>, _>("cooldown_reason"),
            )
        });
        if should_apply_cooldown
            && let (Some(before_state), Some(after_state)) =
                (before_cooldown_state, after_cooldown_state)
            && before_state != after_state
        {
            let suffix = account_suffix(account_id);
            let event = match (before_state.0, after_state.0) {
                (None, Some(cooldown_until)) => Some((
                    "account_cooldown_started",
                    format!(
                        "Cooldown started for ChatGPT account {suffix} until {cooldown_until} (reason: {})",
                        after_state.1.as_deref().unwrap_or("unknown")
                    ),
                )),
                (Some(_), Some(cooldown_until)) => Some((
                    "account_cooldown_updated",
                    format!(
                        "Cooldown updated for ChatGPT account {suffix} until {cooldown_until} (reason: {})",
                        after_state.1.as_deref().unwrap_or("unknown")
                    ),
                )),
                (Some(_), None) => Some((
                    "account_cooldown_cleared",
                    format!("Cooldown cleared for ChatGPT account {suffix}"),
                )),
                (None, None) => None,
            };
            if let Some((event_type, message)) = event {
                self.append_event_tx(&mut tx, Some(account_id), event_type, message, fetched_at)
                    .await?;
            }
        }
        tx.commit().await?;

        Ok(ChatgptAccountPoolRateLimitEntry {
            account_id: account_id.to_string(),
            fetched_at: Some(if replace_latest {
                fetched_at
            } else {
                latest_fetched_at.unwrap_or(fetched_at)
            }),
            fetched_at_by_limit_id,
            rate_limits: grouped,
        })
    }

    pub(crate) async fn record_rate_limit_snapshot(
        &self,
        account_id: &str,
        snapshot: &RateLimitSnapshot,
    ) -> Result<(), ChatgptAccountPoolError> {
        self.require_account(account_id).await?;
        self.store_rate_limit_snapshot(account_id, snapshot, now_ts())
            .await
    }

    pub(crate) async fn clear_rate_limit_cooldown(
        &self,
        account_id: &str,
    ) -> Result<(), ChatgptAccountPoolError> {
        let now = now_ts();
        let mut tx = self.pool.begin().await?;
        let result = sqlx::query(
            r#"
            UPDATE accounts
            SET cooldown_until = NULL,
                cooldown_reason = NULL,
                updated_at = ?
            WHERE account_id = ?
              AND (cooldown_until IS NOT NULL OR cooldown_reason IS NOT NULL)
            "#,
        )
        .bind(now)
        .bind(account_id)
        .execute(&mut *tx)
        .await?;
        if result.rows_affected() > 0 {
            self.append_event_tx(
                &mut tx,
                Some(account_id),
                "account_cooldown_cleared",
                format!(
                    "Cooldown cleared for ChatGPT account {} after a usage limit reset",
                    account_suffix(account_id)
                ),
                now,
            )
            .await?;
        } else {
            self.require_account_tx(&mut tx, account_id).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn activate_account(
        &self,
        account_id: &str,
        failover: bool,
    ) -> Result<ChatgptAccountPoolSelectionOutcome, ChatgptAccountPoolError> {
        let Some(auth) = self.load_account_secret(account_id).await? else {
            self.mark_account_missing_secret(account_id).await?;
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

    /// Validates a `pending_validation` account on demand during failover by
    /// probing the `/usage` endpoint:
    /// - On success the usage snapshot is stored (and history recorded) and the
    ///   account is promoted to `valid`, so the next selection pass can pick it
    ///   up if it is not also in cooldown.
    /// - On an authoritative 401 the account is marked `invalid` so it is never
    ///   retried until an external service re-validates it.
    /// - Missing secrets and transient failures leave the account untouched
    ///   (still `pending_validation`) for a later attempt.
    async fn validate_pending_account(
        &self,
        account_id: &str,
    ) -> Result<(), ChatgptAccountPoolError> {
        let Some(auth) = self.load_account_codex_auth(account_id).await? else {
            self.mark_account_missing_secret(account_id).await?;
            return Ok(());
        };
        // Refresh-then-probe: an idle pending account may hold an expired (but
        // refreshable) access token. Probing with it would 401 and wrongly mark
        // the account invalid, so bring the token live first. Only a probe that
        // fails 401 with a *fresh* token is treated as authoritative below.
        let auth = match self.refresh_pending_account_auth(account_id, auth).await? {
            token_refresh::PendingAccountAuth::Ready(auth) => auth,
            token_refresh::PendingAccountAuth::Inconclusive => {
                tracing::warn!(
                    %account_id,
                    "pending account token refresh was inconclusive; leaving account pending_validation"
                );
                return Ok(());
            }
        };
        match fetch_usage_snapshots_with_status(&self.chatgpt_base_url, &auth).await {
            UsageFetchOutcome::Snapshots(snapshots) => {
                // Persists the usage snapshot + history and promotes
                // pending_validation -> valid (terminal states are kept), but does
                // NOT impose a cooldown: this is a validation probe for the account
                // we are failing over *to*, not real turn usage. Cooling it here is
                // the "fresh failover account also marked rate-limited" bug. The
                // stored snapshot still lets the capacity scorer deprioritize an
                // exhausted candidate; a real 429 on a turn is what cools it.
                self.record_validated_rate_limits(account_id, snapshots.as_slice())
                    .await?;
                self.append_event(
                    Some(account_id),
                    "account_validated_on_pickup",
                    format!(
                        "Validated ChatGPT account {} on pickup",
                        account_suffix(account_id)
                    ),
                )
                .await?;
                Ok(())
            }
            UsageFetchOutcome::Unauthorized { body } => {
                // Only a 401 whose body explicitly confirms the credential is dead is
                // authoritative. codex-accounts owns invalidation; an ambiguous 401 (no
                // recognised auth-failure code) may be a transient server blip or a token
                // codex-accounts is mid-refresh for, so leave the account
                // pending_validation rather than permanently killing it.
                if usage_auth_failure_confirms_invalid(&body) {
                    self.update_auth_status(account_id, ChatgptAccountPoolAuthStatus::Invalid)
                        .await?;
                    self.append_event(
                        Some(account_id),
                        "account_validation_failed",
                        format!(
                            "ChatGPT account {} failed validation on pickup (authoritative 401: {}); marked invalid",
                            account_suffix(account_id),
                            usage_oauth_error_code(&body).as_deref().unwrap_or("unknown")
                        ),
                    )
                    .await?;
                } else {
                    tracing::warn!(
                        %account_id,
                        "pending account probe returned an ambiguous 401 (no authoritative auth-failure code); leaving pending_validation for codex-accounts to adjudicate"
                    );
                    self.append_event(
                        Some(account_id),
                        "account_validation_failed",
                        format!(
                            "ChatGPT account {} probe returned an ambiguous 401 on pickup; left pending_validation (codex-accounts owns invalidation)",
                            account_suffix(account_id)
                        ),
                    )
                    .await?;
                }
                Ok(())
            }
            UsageFetchOutcome::Failed(err) => {
                tracing::warn!(
                    %account_id,
                    error = %err,
                    "pending account validation probe failed; leaving account pending_validation"
                );
                Ok(())
            }
        }
    }

    async fn initialize_schema(&self) -> Result<(), ChatgptAccountPoolError> {
        // Serialize the compatibility check and all migrations with every other
        // SQLite writer. Without an immediate transaction, the CLI and
        // codex-accounts can both observe a missing column and race the same
        // ALTER TABLE, leaving one startup to fail with "duplicate column".
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        // Check compatibility before running any migrations. A newer external
        // service may have changed table semantics that this build must not
        // overwrite or partially downgrade.
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS pool_state (
                key TEXT PRIMARY KEY,
                value TEXT NULL
            )
            "#,
        )
        .execute(&mut *tx)
        .await?;
        let existing_schema_version: Option<String> =
            sqlx::query_scalar("SELECT value FROM pool_state WHERE key = 'schema_version' LIMIT 1")
                .fetch_optional(&mut *tx)
                .await?
                .flatten();
        if let Some(found) = existing_schema_version
            && found
                .parse::<u64>()
                .ok()
                .is_none_or(|found| found > ACCOUNT_POOL_SCHEMA_VERSION_NUMBER)
        {
            return Err(ChatgptAccountPoolError::IncompatibleSchemaVersion {
                found,
                supported: ACCOUNT_POOL_SCHEMA_VERSION,
            });
        }

        // Match codex-accounts' forward-compatibility guard. An extension may
        // add nullable columns or columns with defaults without breaking this
        // build, but an unknown required column would make every account INSERT
        // fail. Reject it before creating indexes, columns, or other tables.
        let account_columns = sqlx::query(
            r#"
            SELECT name, "notnull" AS is_not_null, dflt_value, pk
            FROM pragma_table_info('accounts')
            "#,
        )
        .fetch_all(&mut *tx)
        .await?;
        let mut unknown_required_columns = Vec::new();
        for row in account_columns {
            let name: String = row.get("name");
            let is_not_null: i64 = row.get("is_not_null");
            let default_value: Option<String> = row.get("dflt_value");
            let is_primary_key: i64 = row.get("pk");
            if is_primary_key == 0
                && is_not_null != 0
                && default_value.is_none()
                && !is_known_accounts_column(&name)
            {
                unknown_required_columns.push(name);
            }
        }
        if !unknown_required_columns.is_empty() {
            unknown_required_columns.sort();
            return Err(ChatgptAccountPoolError::IncompatibleAccountsTable {
                columns: unknown_required_columns.join(", "),
            });
        }

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS accounts (
                account_id TEXT PRIMARY KEY,
                workspace_account_id TEXT NOT NULL DEFAULT '',
                member_identity_key TEXT,
                chatgpt_user_id TEXT,
                subject TEXT,
                email TEXT,
                plan_type TEXT,
                enabled INTEGER NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                last_used_at INTEGER NULL,
                last_auth_refresh_at INTEGER NULL,
                auth_status TEXT NOT NULL,
                cooldown_until INTEGER NULL,
                cooldown_reason TEXT NULL,
                access_token TEXT NULL,
                refresh_token TEXT NULL,
                id_token TEXT NULL,
                agent_identity TEXT NULL
            )
            "#,
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            r#"
            CREATE INDEX IF NOT EXISTS idx_accounts_workspace_account_id
                ON accounts (workspace_account_id)
            "#,
        )
        .execute(&mut *tx)
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
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS account_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                account_id TEXT NULL,
                event_type TEXT NOT NULL,
                message TEXT NOT NULL,
                actor TEXT NULL,
                created_at INTEGER NOT NULL
            )
            "#,
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            r#"
            CREATE INDEX IF NOT EXISTS idx_account_events_account_time
                ON account_events (account_id, created_at, id)
            "#,
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            r#"
            CREATE INDEX IF NOT EXISTS idx_account_events_type_time
                ON account_events (event_type, created_at, id)
            "#,
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS account_usage_history (
                id            INTEGER PRIMARY KEY AUTOINCREMENT,
                account_id    TEXT NOT NULL,
                limit_id      TEXT NOT NULL,
                snapshot_json TEXT NOT NULL,
                fetched_at    INTEGER NOT NULL
            );
            "#,
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            r#"
            CREATE INDEX IF NOT EXISTS idx_usage_history_acct_time
                ON account_usage_history (account_id, fetched_at);
            "#,
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            r#"
            CREATE INDEX IF NOT EXISTS idx_usage_history_acct_time_id
                ON account_usage_history (account_id, fetched_at, id);
            "#,
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS account_activity (
                account_id   TEXT NOT NULL,
                owner_pid    INTEGER NOT NULL,
                host         TEXT NOT NULL,
                started_at   INTEGER NOT NULL,
                heartbeat_at INTEGER NOT NULL,
                expires_at   INTEGER NOT NULL,
                PRIMARY KEY (account_id, owner_pid, host)
            );
            "#,
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS account_token_locks (
                account_id  TEXT PRIMARY KEY,
                locked_by   TEXT NOT NULL,
                acquired_at INTEGER NOT NULL,
                expires_at  INTEGER NOT NULL
            );
            "#,
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            r#"
            CREATE INDEX IF NOT EXISTS idx_account_activity_expires_at
                ON account_activity (expires_at);
            "#,
        )
        .execute(&mut *tx)
        .await?;
        // v1 → v2: drop the `is_default` column that was removed from the schema.
        // `CREATE TABLE IF NOT EXISTS` never alters existing tables, so we must do
        // this explicitly for databases created before schema version 2.
        let legacy_column_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pragma_table_info('accounts') WHERE name = 'is_default'",
        )
        .fetch_one(&mut *tx)
        .await?;
        if legacy_column_count > 0 {
            sqlx::query("ALTER TABLE accounts DROP COLUMN is_default")
                .execute(&mut *tx)
                .await?;
        }
        // Token columns: the DB is the single source of truth for pool-account
        // tokens (no per-account auth.json files). The Go service (codex-accounts)
        // adds these same columns within its schema-version-3 migration; mirror it
        // here so the columns exist regardless of which process opens the DB first.
        // SQLite has no `ADD COLUMN IF NOT EXISTS`, so check pragma_table_info and
        // add when missing. This is part of schema version 3 — do NOT bump the
        // version, or the Go side (pinned to "3") would flag the DB incompatible.
        for (column, add_column_stmt) in [
            (
                "access_token",
                "ALTER TABLE accounts ADD COLUMN access_token TEXT",
            ),
            (
                "refresh_token",
                "ALTER TABLE accounts ADD COLUMN refresh_token TEXT",
            ),
            ("id_token", "ALTER TABLE accounts ADD COLUMN id_token TEXT"),
            (
                "agent_identity",
                "ALTER TABLE accounts ADD COLUMN agent_identity TEXT",
            ),
        ] {
            let column_count: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM pragma_table_info('accounts') WHERE name = ?",
            )
            .bind(column)
            .fetch_one(&mut *tx)
            .await?;
            if column_count == 0 {
                sqlx::query(add_column_stmt).execute(&mut *tx).await?;
            }
        }
        // account_events.actor records which binary wrote each event (codex-cli vs
        // codex-accounts). Nullable + still part of schema version 3 — do NOT bump
        // the version (the Go side is pinned to "3"). Mirrors the Go migration so the
        // column exists regardless of which process opens the DB first.
        let actor_column_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pragma_table_info('account_events') WHERE name = 'actor'",
        )
        .fetch_one(&mut *tx)
        .await?;
        if actor_column_count == 0 {
            sqlx::query("ALTER TABLE account_events ADD COLUMN actor TEXT")
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query(
            r#"
            INSERT INTO pool_state (key, value)
            VALUES ('schema_version', ?)
            ON CONFLICT(key) DO UPDATE SET
                value = excluded.value
            WHERE pool_state.value IS NULL
                OR CAST(pool_state.value AS INTEGER) < CAST(excluded.value AS INTEGER)
            "#,
        )
        .bind(ACCOUNT_POOL_SCHEMA_VERSION)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn migrate_legacy_auth_if_needed(&self) -> Result<(), ChatgptAccountPoolError> {
        let Some(auth) = load_auth_dot_json(
            &self.codex_home,
            self.auth_credentials_store_mode,
            self.keyring_backend_kind,
        )?
        else {
            return Ok(());
        };
        let Ok(metadata) = extract_chatgpt_metadata(&auth) else {
            return Ok(());
        };
        let tokens = auth
            .tokens
            .as_ref()
            .ok_or(ChatgptAccountPoolError::UnsupportedAuthMode)?;
        // Hold SQLite's writer slot while deciding whether the legacy copy is
        // safe to migrate and while writing it. Otherwise codex-accounts can
        // rotate a token between the read and register steps and the CLI can
        // overwrite that newer credential with the preserved legacy token.
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let existing_tokens = sqlx::query(
            r#"
            SELECT workspace_account_id,
                   member_identity_key,
                   access_token,
                   refresh_token
            FROM accounts
            WHERE account_id = ?
            LIMIT 1
            "#,
        )
        .bind(&metadata.account_id)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(existing_tokens) = existing_tokens {
            let stored_workspace_account_id: String = existing_tokens.get("workspace_account_id");
            let stored_member_identity_key: Option<String> =
                existing_tokens.get("member_identity_key");
            if stored_workspace_account_id != metadata.workspace_account_id
                || (stored_member_identity_key.is_some()
                    && stored_member_identity_key.as_deref()
                        != metadata.member_identity_key.as_deref())
            {
                return Err(ChatgptAccountPoolError::CredentialIdentityMismatch(
                    metadata.account_id,
                ));
            }
            let stored_access_token: Option<String> = existing_tokens.get("access_token");
            let stored_refresh_token: Option<String> = existing_tokens.get("refresh_token");
            let Some(source_tokens) = auth.tokens.as_ref() else {
                return Ok(());
            };
            let pool_copy_is_incomplete = stored_access_token.as_deref().is_none_or(str::is_empty)
                || stored_refresh_token.as_deref().is_none_or(str::is_empty);
            let stored_values_match_source = stored_access_token
                .as_deref()
                .is_none_or(|stored| stored.is_empty() || stored == source_tokens.access_token)
                && stored_refresh_token.as_deref().is_none_or(|stored| {
                    stored.is_empty() || stored == source_tokens.refresh_token
                });
            if !pool_copy_is_incomplete || !stored_values_match_source {
                tx.commit().await?;
                return Ok(());
            }
            tracing::warn!(
                account_id = metadata.account_id,
                "repairing an incomplete legacy-auth migration from the preserved top-level credential"
            );
        }

        let now = now_ts();
        sqlx::query(
            r#"
            INSERT INTO accounts (
                account_id,
                workspace_account_id,
                member_identity_key,
                chatgpt_user_id,
                subject,
                email,
                plan_type,
                enabled,
                created_at,
                updated_at,
                last_used_at,
                last_auth_refresh_at,
                auth_status,
                cooldown_until,
                cooldown_reason,
                access_token,
                refresh_token,
                id_token
            )
            VALUES (?, ?, ?, ?, ?, ?, ?, 1, ?, ?, NULL, ?, ?, NULL, NULL, ?, ?, ?)
            ON CONFLICT(account_id) DO UPDATE SET
                workspace_account_id = excluded.workspace_account_id,
                member_identity_key = excluded.member_identity_key,
                chatgpt_user_id = excluded.chatgpt_user_id,
                subject = excluded.subject,
                email = excluded.email,
                plan_type = excluded.plan_type,
                enabled = 1,
                updated_at = excluded.updated_at,
                last_auth_refresh_at = excluded.last_auth_refresh_at,
                auth_status = excluded.auth_status,
                access_token = excluded.access_token,
                refresh_token = excluded.refresh_token,
                id_token = excluded.id_token
            "#,
        )
        .bind(&metadata.account_id)
        .bind(&metadata.workspace_account_id)
        .bind(&metadata.member_identity_key)
        .bind(&metadata.chatgpt_user_id)
        .bind(&metadata.subject)
        .bind(&metadata.email)
        .bind(&metadata.plan_type)
        .bind(now)
        .bind(now)
        .bind(auth.last_refresh.map(|value| value.timestamp()))
        .bind(ChatgptAccountPoolAuthStatus::Valid.as_str())
        .bind(&tokens.access_token)
        .bind(&tokens.refresh_token)
        .bind(&tokens.id_token.raw_jwt)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            r#"
            INSERT INTO pool_state (key, value)
            VALUES ('selected_account_id', ?)
            ON CONFLICT(key) DO UPDATE SET value = excluded.value
            WHERE pool_state.value IS NULL
            "#,
        )
        .bind(&metadata.account_id)
        .execute(&mut *tx)
        .await?;
        self.append_event_tx(
            &mut tx,
            Some(&metadata.account_id),
            "account_registered",
            format!(
                "Registered ChatGPT account {}",
                account_suffix(&metadata.workspace_account_id)
            ),
            now,
        )
        .await?;
        self.append_event_tx(
            &mut tx,
            Some(&metadata.account_id),
            "legacy_auth_migrated",
            format!(
                "Migrated legacy ChatGPT auth {} into the account pool",
                account_suffix(&metadata.workspace_account_id)
            ),
            now,
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn load_rate_limits_grouped(
        &self,
        only_account_id: Option<&str>,
    ) -> Result<BTreeMap<String, BTreeMap<String, RateLimitSnapshot>>, ChatgptAccountPoolError>
    {
        Ok(self
            .load_rate_limits_grouped_with_fetch_time(only_account_id)
            .await?
            .into_iter()
            .map(|(account_id, group)| (account_id, group.rate_limits))
            .collect())
    }

    async fn load_rate_limits_grouped_with_fetch_time(
        &self,
        only_account_id: Option<&str>,
    ) -> Result<BTreeMap<String, StoredRateLimitGroup>, ChatgptAccountPoolError> {
        // Two literal queries (rather than one built from a String) so sqlx's
        // `SqlSafeStr` guard is satisfied; the account id is always bound, never
        // interpolated.
        let rows = match only_account_id {
            Some(account_id) => {
                sqlx::query(
                    "SELECT account_id, limit_id, snapshot_json, fetched_at \
                     FROM account_rate_limits WHERE account_id = ?",
                )
                .bind(account_id)
                .fetch_all(&self.pool)
                .await?
            }
            None => {
                sqlx::query(
                    "SELECT account_id, limit_id, snapshot_json, fetched_at \
                     FROM account_rate_limits",
                )
                .fetch_all(&self.pool)
                .await?
            }
        };
        let mut grouped: BTreeMap<String, StoredRateLimitGroup> = BTreeMap::new();
        for row in rows {
            let account_id: String = row.get("account_id");
            let limit_id: String = row.get("limit_id");
            let snapshot_json: String = row.get("snapshot_json");
            let fetched_at: i64 = row.get("fetched_at");
            let snapshot = serde_json::from_str::<RateLimitSnapshot>(&snapshot_json)?;
            let entry = grouped.entry(account_id).or_default();
            entry.latest_fetched_at = Some(
                entry
                    .latest_fetched_at
                    .unwrap_or(fetched_at)
                    .max(fetched_at),
            );
            entry
                .fetched_at_by_limit_id
                .insert(limit_id.clone(), fetched_at);
            entry.rate_limits.insert(limit_id, snapshot);
        }
        Ok(grouped)
    }

    async fn store_rate_limit_snapshot(
        &self,
        account_id: &str,
        snapshot: &RateLimitSnapshot,
        fetched_at: i64,
    ) -> Result<(), ChatgptAccountPoolError> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        self.require_account_tx(&mut tx, account_id).await?;
        self.store_rate_limit_snapshot_tx(&mut tx, account_id, snapshot, fetched_at)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn store_rate_limit_snapshot_tx(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        account_id: &str,
        snapshot: &RateLimitSnapshot,
        fetched_at: i64,
    ) -> Result<(), ChatgptAccountPoolError> {
        let limit_id = snapshot
            .limit_id
            .clone()
            .unwrap_or_else(|| "codex".to_string());
        let snapshot_json = serde_json::to_string(snapshot)?;
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
        .bind(limit_id.clone())
        .bind(snapshot_json.clone())
        .bind(fetched_at)
        .execute(&mut **tx)
        .await?;
        sqlx::query(
            r#"
            INSERT INTO account_usage_history (account_id, limit_id, snapshot_json, fetched_at)
            VALUES (?, ?, ?, ?)
            "#,
        )
        .bind(account_id)
        .bind(limit_id)
        .bind(snapshot_json)
        .bind(fetched_at)
        .execute(&mut **tx)
        .await?;
        sqlx::query(
            r#"
            DELETE FROM account_usage_history
            WHERE account_id = ?
              AND id IN (
                  SELECT id
                  FROM account_usage_history
                  WHERE account_id = ?
                  ORDER BY id DESC
                  LIMIT -1 OFFSET ?
              )
            "#,
        )
        .bind(account_id)
        .bind(account_id)
        .bind(USAGE_HISTORY_RETENTION_PER_ACCOUNT)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    async fn append_event(
        &self,
        account_id: Option<&str>,
        event_type: &str,
        message: String,
    ) -> Result<(), ChatgptAccountPoolError> {
        let mut tx = self.pool.begin().await?;
        self.append_event_tx(&mut tx, account_id, event_type, message, now_ts())
            .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn append_event_tx(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        account_id: Option<&str>,
        event_type: &str,
        message: String,
        created_at: i64,
    ) -> Result<(), ChatgptAccountPoolError> {
        sqlx::query(
            "INSERT INTO account_events (account_id, event_type, message, actor, created_at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(account_id)
        .bind(event_type)
        .bind(message)
        .bind(self.actor.as_str())
        .bind(created_at)
        .execute(&mut **tx)
        .await?;
        sqlx::query(
            "DELETE FROM account_events WHERE id IN (SELECT id FROM account_events ORDER BY id DESC LIMIT -1 OFFSET ?)",
        )
        .bind(EVENT_RETENTION_LIMIT)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    async fn require_account_tx(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        account_id: &str,
    ) -> Result<(), ChatgptAccountPoolError> {
        let exists = sqlx::query("SELECT 1 FROM accounts WHERE account_id = ? LIMIT 1")
            .bind(account_id)
            .fetch_optional(&mut **tx)
            .await?
            .is_some();
        if exists {
            Ok(())
        } else {
            Err(ChatgptAccountPoolError::AccountNotFound(
                account_id.to_string(),
            ))
        }
    }

    #[cfg(test)]
    async fn set_selected_account_id_with_event(
        &self,
        account_id: &str,
        event_type: &str,
        message: String,
    ) -> Result<(), ChatgptAccountPoolError> {
        let now = now_ts();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        self.require_account_tx(&mut tx, account_id).await?;
        sqlx::query(
            "INSERT INTO pool_state (key, value) VALUES ('selected_account_id', ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        )
        .bind(account_id)
        .execute(&mut *tx)
        .await?;
        self.append_event_tx(&mut tx, Some(account_id), event_type, message, now)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Changes the global selection only if no other process changed it after
    /// the caller's scoring snapshot. Candidate eligibility and the audit event
    /// are committed in the same SQLite transaction as the selection.
    async fn compare_and_set_selected_account_id_with_event(
        &self,
        expected_account_id: Option<&str>,
        account_id: &str,
        event_type: &str,
        message: String,
    ) -> Result<bool, ChatgptAccountPoolError> {
        let now = now_ts();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        self.require_account_tx(&mut tx, account_id).await?;
        let selected_account_id = sqlx::query_scalar::<_, Option<String>>(
            "SELECT value FROM pool_state WHERE key = 'selected_account_id'",
        )
        .fetch_optional(&mut *tx)
        .await?
        .flatten();
        let candidate_is_eligible: bool = sqlx::query_scalar(
            r#"
            SELECT EXISTS (
                SELECT 1
                FROM accounts
                WHERE account_id = ?
                  AND enabled != 0
                  AND auth_status = 'valid'
                  AND (cooldown_until IS NULL OR cooldown_until <= ?)
            )
            "#,
        )
        .bind(account_id)
        .bind(now)
        .fetch_one(&mut *tx)
        .await?;
        if selected_account_id.as_deref() != expected_account_id || !candidate_is_eligible {
            tx.rollback().await?;
            return Ok(false);
        }
        sqlx::query(
            "INSERT INTO pool_state (key, value) VALUES ('selected_account_id', ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        )
        .bind(account_id)
        .execute(&mut *tx)
        .await?;
        self.append_event_tx(&mut tx, Some(account_id), event_type, message, now)
            .await?;
        tx.commit().await?;
        Ok(true)
    }

    async fn selected_account_id(&self) -> Result<Option<String>, ChatgptAccountPoolError> {
        let row = sqlx::query("SELECT value FROM pool_state WHERE key = 'selected_account_id'")
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.and_then(|row| row.get::<Option<String>, _>("value")))
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
        let selected_account_id = self.selected_account_id().await?;
        let row = sqlx::query(
            r#"
            SELECT
                account_id,
                workspace_account_id,
                member_identity_key,
                chatgpt_user_id,
                subject,
                email,
                plan_type,
                enabled,
                created_at,
                updated_at,
                last_used_at,
                last_auth_refresh_at,
                auth_status,
                cooldown_until,
                cooldown_reason
            FROM accounts
            WHERE account_id = ?
            LIMIT 1
            "#,
        )
        .bind(account_id)
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let grouped_rate_limits = self
            .load_rate_limits_grouped(/*only_account_id*/ Some(account_id))
            .await?;
        Ok(Some(account_from_row(
            &row,
            selected_account_id.as_deref(),
            &grouped_rate_limits,
        )))
    }

    async fn update_auth_status(
        &self,
        account_id: &str,
        auth_status: ChatgptAccountPoolAuthStatus,
    ) -> Result<(), ChatgptAccountPoolError> {
        let result =
            sqlx::query("UPDATE accounts SET auth_status = ?, updated_at = ? WHERE account_id = ?")
                .bind(auth_status.as_str())
                .bind(now_ts())
                .bind(account_id)
                .execute(&self.pool)
                .await?;
        if result.rows_affected() == 0 {
            return Err(ChatgptAccountPoolError::AccountNotFound(
                account_id.to_string(),
            ));
        }
        Ok(())
    }

    async fn update_auth_status_with_event(
        &self,
        account_id: &str,
        auth_status: ChatgptAccountPoolAuthStatus,
        event_type: &str,
        message: String,
    ) -> Result<(), ChatgptAccountPoolError> {
        let now = now_ts();
        let mut tx = self.pool.begin().await?;
        let result =
            sqlx::query("UPDATE accounts SET auth_status = ?, updated_at = ? WHERE account_id = ?")
                .bind(auth_status.as_str())
                .bind(now)
                .bind(account_id)
                .execute(&mut *tx)
                .await?;
        if result.rows_affected() == 0 {
            return Err(ChatgptAccountPoolError::AccountNotFound(
                account_id.to_string(),
            ));
        }
        self.append_event_tx(&mut tx, Some(account_id), event_type, message, now)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Reads only the `auth_status` column for `account_id`. Used by the token-
    /// refresh path to detect a terminal status written by codex-accounts without
    /// a full account reload.
    pub(crate) async fn read_account_auth_status(
        &self,
        account_id: &str,
    ) -> Result<Option<ChatgptAccountPoolAuthStatus>, ChatgptAccountPoolError> {
        let row = sqlx::query("SELECT auth_status FROM accounts WHERE account_id = ? LIMIT 1")
            .bind(account_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| {
            let s: String = r.get("auth_status");
            ChatgptAccountPoolAuthStatus::from_db(&s)
        }))
    }

    pub(crate) async fn mark_account_missing_secret(
        &self,
        account_id: &str,
    ) -> Result<(), ChatgptAccountPoolError> {
        self.update_auth_status_with_event(
            account_id,
            ChatgptAccountPoolAuthStatus::MissingSecret,
            "account_missing_secret",
            format!(
                "Missing auth secret for ChatGPT account {}",
                account_suffix(account_id)
            ),
        )
        .await
    }

    /// Builds an in-memory [`AuthDotJson`] from the token columns stored on the
    /// account row. The DB is the single source of truth for pool-account tokens;
    /// no per-account `auth.json` files are read. Returns `Ok(None)` when the
    /// account row is missing or no access token has been stored yet.
    pub(crate) async fn read_account_tokens(
        &self,
        account_id: &str,
    ) -> Result<Option<AuthDotJson>, ChatgptAccountPoolError> {
        let row = sqlx::query(
            r#"
            SELECT workspace_account_id,
                   member_identity_key,
                   access_token,
                   refresh_token,
                   id_token,
                   last_auth_refresh_at
            FROM accounts
            WHERE account_id = ?
            LIMIT 1
            "#,
        )
        .bind(account_id)
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let access_token: Option<String> = row.get("access_token");
        let access_token = match access_token {
            Some(token) if !token.is_empty() => token,
            // Row exists but tokens have not been written yet (e.g. an account
            // inserted before codex-accounts has run its first validate). Treat
            // as "no secret" so callers mark it missing rather than panicking.
            _ => return Ok(None),
        };
        let refresh_token: Option<String> = row.get("refresh_token");
        let id_token_raw: Option<String> = row.get("id_token");
        let workspace_account_id: String = row.get("workspace_account_id");
        let stored_member_identity_key: Option<String> = row.get("member_identity_key");
        let last_auth_refresh_at: Option<i64> = row.get("last_auth_refresh_at");

        let id_token = match id_token_raw {
            Some(raw) if !raw.is_empty() => parse_chatgpt_jwt_claims(&raw).unwrap_or_default(),
            _ => IdTokenInfo::default(),
        };
        let token_chatgpt_user_id = normalize_optional_metadata(
            "ChatGPT user id",
            id_token.chatgpt_user_id.as_deref(),
            MAX_CHATGPT_USER_ID_BYTES,
        )?;
        let token_subject = normalize_optional_metadata(
            "ID token subject",
            id_token.subject.as_deref(),
            MAX_SUBJECT_BYTES,
        )?;
        let token_member_identity_key = token_chatgpt_user_id
            .as_deref()
            .map(|value| format!("chatgpt_user_id:{value}"))
            .or_else(|| token_subject.as_deref().map(|value| format!("sub:{value}")));
        if stored_member_identity_key.is_some()
            && stored_member_identity_key != token_member_identity_key
        {
            return Err(ChatgptAccountPoolError::CredentialIdentityMismatch(
                account_id.to_string(),
            ));
        }
        // The account row is the stable identity binding established at
        // registration. An ID token is metadata, not authority to redirect a
        // stored credential to a different workspace.
        let account_id_claim = Some(workspace_account_id)
            .filter(|value| !value.is_empty())
            .or_else(|| {
                id_token
                    .chatgpt_account_id
                    .clone()
                    .filter(|value| !value.is_empty())
            });
        let tokens = TokenData {
            id_token,
            access_token,
            refresh_token: refresh_token.unwrap_or_default(),
            account_id: account_id_claim,
        };
        Ok(Some(AuthDotJson {
            // None resolves to ChatGPT auth (see `resolved_mode`); pool accounts
            // are always managed ChatGPT OAuth credentials.
            auth_mode: None,
            tokens: Some(tokens),
            pool_account_id: Some(account_id.to_string()),
            last_refresh: last_auth_refresh_at
                .and_then(|ts| DateTime::<Utc>::from_timestamp(ts, 0)),
            agent_identity: None,
        }))
    }

    /// Test-only direct token mutation helper. Production refresh acknowledgements
    /// use the atomic token/status/event transaction in `token_refresh`.
    #[cfg(test)]
    pub(crate) async fn write_account_tokens(
        &self,
        account_id: &str,
        auth: &AuthDotJson,
    ) -> Result<(), ChatgptAccountPoolError> {
        let tokens = auth
            .tokens
            .as_ref()
            .ok_or(ChatgptAccountPoolError::UnsupportedAuthMode)?;
        let now = now_ts();
        let refresh_at = auth
            .last_refresh
            .map(|value| value.timestamp())
            .unwrap_or(now);
        let result = sqlx::query(
            r#"
            UPDATE accounts
            SET access_token = ?,
                refresh_token = ?,
                id_token = ?,
                last_auth_refresh_at = ?,
                updated_at = ?
            WHERE account_id = ?
            "#,
        )
        .bind(&tokens.access_token)
        .bind(&tokens.refresh_token)
        .bind(&tokens.id_token.raw_jwt)
        .bind(refresh_at)
        .bind(now)
        .bind(account_id)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Err(ChatgptAccountPoolError::AccountNotFound(
                account_id.to_string(),
            ));
        }
        Ok(())
    }

    async fn load_account_secret(
        &self,
        account_id: &str,
    ) -> Result<Option<AuthDotJson>, ChatgptAccountPoolError> {
        self.read_account_tokens(account_id).await
    }

    async fn load_account_codex_auth(
        &self,
        account_id: &str,
    ) -> Result<Option<CodexAuth>, ChatgptAccountPoolError> {
        let Some(auth) = self.read_account_tokens(account_id).await? else {
            return Ok(None);
        };
        let codex_auth = CodexAuth::from_auth_dot_json(
            &self.codex_home,
            auth,
            self.auth_credentials_store_mode,
            Some(self.chatgpt_base_url.as_str()),
            self.keyring_backend_kind,
            /*agent_identity_authapi_base_url*/ None,
            &self.auth_route_config,
        )
        .await?;
        Ok(Some(codex_auth))
    }
}

fn account_from_row(
    row: &SqliteRow,
    selected_account_id: Option<&str>,
    grouped_rate_limits: &BTreeMap<String, BTreeMap<String, RateLimitSnapshot>>,
) -> ChatgptAccountPoolAccount {
    let account_id: String = row.get("account_id");
    ChatgptAccountPoolAccount {
        workspace_account_id: row.get("workspace_account_id"),
        member_identity_key: row.get("member_identity_key"),
        chatgpt_user_id: row.get("chatgpt_user_id"),
        subject: row.get("subject"),
        email: row.get("email"),
        plan_type: row.get("plan_type"),
        enabled: row.get::<i64, _>("enabled") != 0,
        is_selected: selected_account_id == Some(account_id.as_str()),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
        last_used_at: row.get("last_used_at"),
        last_auth_refresh_at: row.get("last_auth_refresh_at"),
        auth_status: ChatgptAccountPoolAuthStatus::from_db(&row.get::<String, _>("auth_status")),
        cooldown_until: row.get("cooldown_until"),
        cooldown_reason: row.get("cooldown_reason"),
        rate_limits: grouped_rate_limits
            .get(account_id.as_str())
            .cloned()
            .unwrap_or_default(),
        account_id,
    }
}

#[derive(Debug)]
struct ChatgptAccountMetadata {
    account_id: String,
    workspace_account_id: String,
    member_identity_key: Option<String>,
    chatgpt_user_id: Option<String>,
    subject: Option<String>,
    email: Option<String>,
    plan_type: Option<String>,
}

const MAX_WORKSPACE_ACCOUNT_ID_BYTES: usize = 256;
const MAX_POOL_ACCOUNT_ID_BYTES: usize = 256;
const MAX_CHATGPT_USER_ID_BYTES: usize = 256;
const MAX_SUBJECT_BYTES: usize = 512;
const MAX_EMAIL_BYTES: usize = 512;
const MAX_PLAN_TYPE_BYTES: usize = 128;

fn normalize_optional_metadata(
    field: &'static str,
    value: Option<&str>,
    max_bytes: usize,
) -> Result<Option<String>, ChatgptAccountPoolError> {
    let Some(value) = value.filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    if value.len() > max_bytes || value.trim() != value || value.chars().any(char::is_control) {
        return Err(ChatgptAccountPoolError::InvalidMetadata(field));
    }
    Ok(Some(value.to_string()))
}

fn extract_chatgpt_metadata(
    auth: &AuthDotJson,
) -> Result<ChatgptAccountMetadata, ChatgptAccountPoolError> {
    if auth.agent_identity.is_some() {
        return Err(ChatgptAccountPoolError::UnsupportedAuthMode);
    }
    let tokens = auth
        .tokens
        .as_ref()
        .ok_or(ChatgptAccountPoolError::UnsupportedAuthMode)?;
    if auth.auth_mode.is_some_and(|mode| mode != AuthMode::Chatgpt) {
        return Err(ChatgptAccountPoolError::UnsupportedAuthMode);
    }
    let token_workspace_account_id = normalize_optional_metadata(
        "workspace account id",
        tokens.account_id.as_deref(),
        MAX_WORKSPACE_ACCOUNT_ID_BYTES,
    )?;
    let claimed_workspace_account_id = normalize_optional_metadata(
        "ID token workspace account id",
        tokens.id_token.chatgpt_account_id.as_deref(),
        MAX_WORKSPACE_ACCOUNT_ID_BYTES,
    )?;
    if token_workspace_account_id.is_some()
        && claimed_workspace_account_id.is_some()
        && token_workspace_account_id != claimed_workspace_account_id
    {
        return Err(ChatgptAccountPoolError::AccountIdMismatch);
    }
    let workspace_account_id = token_workspace_account_id
        .or(claimed_workspace_account_id)
        .ok_or(ChatgptAccountPoolError::MissingAccountId)?;
    let chatgpt_user_id = normalize_optional_metadata(
        "ChatGPT user id",
        tokens.id_token.chatgpt_user_id.as_deref(),
        MAX_CHATGPT_USER_ID_BYTES,
    )?;
    let subject = normalize_optional_metadata(
        "ID token subject",
        tokens.id_token.subject.as_deref(),
        MAX_SUBJECT_BYTES,
    )?;
    let email = normalize_optional_metadata(
        "ID token email",
        tokens.id_token.email.as_deref(),
        MAX_EMAIL_BYTES,
    )?;
    let plan_type = normalize_optional_metadata(
        "ID token plan type",
        tokens.id_token.get_chatgpt_plan_type_raw().as_deref(),
        MAX_PLAN_TYPE_BYTES,
    )?;
    let member_identity_key = chatgpt_user_id
        .as_deref()
        .map(|value| format!("chatgpt_user_id:{value}"))
        .or_else(|| subject.as_deref().map(|value| format!("sub:{value}")));
    let pool_account_id = normalize_optional_metadata(
        "pool account id",
        auth.pool_account_id.as_deref(),
        MAX_POOL_ACCOUNT_ID_BYTES,
    )?;
    let account_id = pool_account_id.unwrap_or_else(|| {
        derive_pool_account_id(&workspace_account_id, member_identity_key.as_deref())
    });
    Ok(ChatgptAccountMetadata {
        account_id,
        workspace_account_id,
        member_identity_key,
        chatgpt_user_id,
        subject,
        email,
        plan_type,
    })
}

fn now_ts() -> i64 {
    Utc::now().timestamp()
}

/// The `account_events.actor` value stamped on every event this CLI writes:
/// `codex-cli:<host>:<pid>`. The host is resolved the same way as the activity
/// heartbeat owner so the two can be correlated.
fn cli_actor() -> String {
    let host = codex_config::host_name()
        .or_else(|| non_empty_env("HOSTNAME"))
        .or_else(|| non_empty_env("COMPUTERNAME"))
        .unwrap_or_else(|| "unknown-host".to_string());
    format!("codex-cli:{host}:{}", std::process::id())
}

fn pool_root_dir(codex_home: &Path) -> PathBuf {
    codex_home.join(POOL_DB_DIR)
}

fn pool_db_path(codex_home: &Path) -> PathBuf {
    pool_root_dir(codex_home).join(POOL_DB_FILE)
}

fn account_suffix(account_id: &str) -> &str {
    // Return at most the last 8 characters, slicing on a char boundary so a
    // non-ASCII account/workspace id can never trigger a byte-index panic.
    match account_id.char_indices().rev().nth(7) {
        Some((idx, _)) => &account_id[idx..],
        None => account_id,
    }
}

fn is_account_eligible(account: &ChatgptAccountPoolAccount, now: i64) -> bool {
    account.enabled
        && matches!(account.auth_status, ChatgptAccountPoolAuthStatus::Valid)
        && account
            .cooldown_until
            .is_none_or(|cooldown_until| cooldown_until <= now)
}

/// A not-yet-validated account that may be brought online during selection:
/// enabled, not cooling down, and not already probed this pass.
fn is_pending_validation_candidate(
    account: &ChatgptAccountPoolAccount,
    now: i64,
    probed: &HashSet<String>,
) -> bool {
    account.enabled
        && matches!(
            account.auth_status,
            ChatgptAccountPoolAuthStatus::PendingValidation
        )
        && account
            .cooldown_until
            .is_none_or(|cooldown_until| cooldown_until <= now)
        && !probed.contains(&account.account_id)
}

/// Picks the best account to run the next turn on, scoring already-valid
/// accounts and not-yet-validated accounts together. Pending accounts are
/// scored as full capacity (see [`capacity_score`]), so the scorer prefers
/// validating fresh capacity over reusing an idle valid account; the caller
/// validates the pick on pickup when it is still `pending_validation`.
fn select_best_candidate<'a>(
    accounts: &'a [ChatgptAccountPoolAccount],
    now: i64,
    probed: &HashSet<String>,
) -> Option<&'a str> {
    accounts
        .iter()
        .filter(|account| {
            is_account_eligible(account, now)
                || is_pending_validation_candidate(account, now, probed)
        })
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
    // A not-yet-validated account has no usage history yet. Treat it as fully
    // available (both windows at 100% remaining, i.e. a known-capacity max) so
    // the scorer prefers bringing it online over an idle valid account whose
    // capacity is unknown ((false, 100)). It is validated on pickup, after
    // which its real snapshot replaces this optimistic score.
    if matches!(
        account.auth_status,
        ChatgptAccountPoolAuthStatus::PendingValidation
    ) {
        return (true, 100);
    }
    let snapshot = account.rate_limits.get("codex").or_else(|| {
        account
            .rate_limits
            .iter()
            .find(|(limit_id, _)| limit_id.eq_ignore_ascii_case("codex"))
            .map(|(_, snapshot)| snapshot)
    });
    let Some(snapshot) = snapshot else {
        return (false, 100);
    };
    // Only treat rate_limit_reached_type as authoritative while the cooldown is
    // still active. Once the cooldown expires the stored snapshot may be stale
    // (the account recovered without a fresh refresh_rate_limits call), so fall
    // back to the raw window percentages instead of scoring the account at 0.
    let cooldown_active = account.cooldown_until.is_some_and(|c| c > now);
    match remaining_percent(snapshot, cooldown_active, now) {
        Some(remaining) => (true, remaining),
        None => (false, 100),
    }
}

fn remaining_percent(snapshot: &RateLimitSnapshot, cooldown_active: bool, now: i64) -> Option<i64> {
    if cooldown_active && snapshot_has_blocking_signal(snapshot) {
        return Some(0);
    }
    let mut remaining = Vec::new();
    if let Some(primary) = snapshot.primary.as_ref() {
        // When the cooldown has expired and the stored window has already reset,
        // the snapshot is stale — treat the window as empty rather than penalising
        // the account for usage from a previous cycle.
        let effective_used = if !cooldown_active && primary.resets_at.is_some_and(|r| r <= now) {
            0.0
        } else {
            primary.used_percent
        };
        if effective_used.is_finite() {
            remaining.push((100.0 - effective_used.clamp(0.0, 100.0)).floor() as i64);
        }
    }
    if let Some(secondary) = snapshot.secondary.as_ref() {
        let effective_used = if !cooldown_active && secondary.resets_at.is_some_and(|r| r <= now) {
            0.0
        } else {
            secondary.used_percent
        };
        if effective_used.is_finite() {
            remaining.push((100.0 - effective_used.clamp(0.0, 100.0)).floor() as i64);
        }
    }
    if let Some(individual_limit) = snapshot.individual_limit.as_ref() {
        // Spend-control limits are account-wide capacity just like the rolling
        // windows. Once their reset is in the past, the persisted value belongs
        // to the previous billing cycle and must no longer penalize selection.
        let effective_remaining = if !cooldown_active && individual_limit.resets_at <= now {
            100
        } else {
            i64::from(individual_limit.remaining_percent).clamp(0, 100)
        };
        remaining.push(effective_remaining);
    }
    if remaining.is_empty() {
        None
    } else {
        Some(remaining.into_iter().min().unwrap_or(0))
    }
}

pub(crate) fn snapshot_contributes_account_cooldown(snapshot: &RateLimitSnapshot) -> bool {
    snapshot
        .limit_id
        .as_deref()
        .is_none_or(|limit_id| limit_id.eq_ignore_ascii_case("codex"))
}

/// Re-evaluates a cached Codex snapshot after a request has been rejected for
/// exhausted auxiliary capacity.
///
/// Background usage polling cannot infer a cooldown from 100% alone because
/// overdrive or paid credits may still admit requests. Once the request itself
/// reports that the auxiliary fallback is exhausted, however, a concurrent
/// exhausted Codex window identifies the account-level reset boundary without
/// pretending that the cached sample was freshly fetched.
pub(crate) fn corroborated_request_cooldown_until(snapshot: &RateLimitSnapshot) -> Option<i64> {
    cooldown_until_from_snapshot(snapshot, now_ts(), true)
}

fn cooldown_until_from_snapshot(
    snapshot: &RateLimitSnapshot,
    fallback_from: i64,
    infer_exhausted_windows: bool,
) -> Option<i64> {
    if !snapshot_contributes_account_cooldown(snapshot) {
        return None;
    }

    let mut cooldown_until = None;
    let mut has_detailed_blocking_signal = false;
    let mut needs_fallback = false;
    let reached_type_blocks_account = snapshot
        .rate_limit_reached_type
        .is_some_and(account_cooldown_rate_limit_reached_type);

    // A request-side 429 makes an exhausted percentage authoritative. For a
    // background `/usage` response, the access/reached signal must also say the
    // account is blocked: 100% can remain allowed while overdrive or credits
    // are available.
    if infer_exhausted_windows || reached_type_blocks_account {
        for window in [snapshot.primary.as_ref(), snapshot.secondary.as_ref()]
            .into_iter()
            .flatten()
        {
            if !window_exhausted(window) {
                continue;
            }
            has_detailed_blocking_signal = true;
            match window.resets_at {
                Some(reset_at) if reset_at > fallback_from => {
                    cooldown_until = Some(cooldown_until.unwrap_or(reset_at).max(reset_at));
                }
                None => needs_fallback = true,
                // A reset in the past makes this an old window observation. Do not
                // manufacture a fresh cooldown from stale usage.
                Some(_) => {}
            }
        }
    }

    let usage_limit_reached = snapshot
        .rate_limit_reached_type
        .is_some_and(workspace_usage_limit_reached_type);
    let individual_limit_depleted = snapshot
        .individual_limit
        .as_ref()
        .is_some_and(|limit| limit.remaining_percent <= 0);
    let spend_control_reached = snapshot.spend_control_reached == Some(true);
    if usage_limit_reached || individual_limit_depleted || spend_control_reached {
        has_detailed_blocking_signal = true;
        match snapshot
            .individual_limit
            .as_ref()
            .map(|limit| limit.resets_at)
        {
            Some(reset_at) if reset_at > fallback_from => {
                cooldown_until = Some(cooldown_until.unwrap_or(reset_at).max(reset_at));
            }
            Some(_) if individual_limit_depleted => {
                // A depleted limit whose billing reset already passed is stale.
            }
            _ => needs_fallback = true,
        }
    }

    // Reached-type payloads sometimes omit all window/spend details. They still
    // mean the account cannot serve Codex requests, so retry conservatively.
    if reached_type_blocks_account && !has_detailed_blocking_signal {
        needs_fallback = true;
    }
    if needs_fallback {
        let fallback = fallback_from.saturating_add(3600);
        cooldown_until = Some(cooldown_until.unwrap_or(fallback).max(fallback));
    }
    cooldown_until
}

fn account_cooldown_rate_limit_reached_type(kind: RateLimitReachedType) -> bool {
    matches!(
        kind,
        RateLimitReachedType::RateLimitReached
            | RateLimitReachedType::WorkspaceOwnerCreditsDepleted
            | RateLimitReachedType::WorkspaceMemberCreditsDepleted
            | RateLimitReachedType::WorkspaceOwnerUsageLimitReached
            | RateLimitReachedType::WorkspaceMemberUsageLimitReached
    )
}

fn workspace_usage_limit_reached_type(kind: RateLimitReachedType) -> bool {
    matches!(
        kind,
        RateLimitReachedType::WorkspaceOwnerUsageLimitReached
            | RateLimitReachedType::WorkspaceMemberUsageLimitReached
    )
}

fn snapshot_has_blocking_signal(snapshot: &RateLimitSnapshot) -> bool {
    snapshot
        .rate_limit_reached_type
        .is_some_and(account_cooldown_rate_limit_reached_type)
        || snapshot.spend_control_reached == Some(true)
        || snapshot
            .individual_limit
            .as_ref()
            .is_some_and(|limit| limit.remaining_percent <= 0)
        || [snapshot.primary.as_ref(), snapshot.secondary.as_ref()]
            .into_iter()
            .flatten()
            .any(window_exhausted)
}

fn window_exhausted(window: &RateLimitWindow) -> bool {
    window.used_percent >= 100.0
}

/// Builds the ChatGPT `/usage` endpoint URL for the given base URL, picking the
/// right path for `backend-api` vs. the public API host.
fn usage_endpoint_url(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    if trimmed.contains("/backend-api") {
        format!("{trimmed}/wham/usage")
    } else {
        format!("{trimmed}/api/codex/usage")
    }
}

/// Builds the request headers (UA, bearer token, account id, FedRAMP) used for
/// `/usage` probes and rate-limit fetches. Returns `None` if a header value
/// cannot be constructed (e.g. the token is unavailable).
fn usage_request_headers(auth: &CodexAuth) -> Option<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(
        USER_AGENT,
        HeaderValue::from_str(&get_codex_user_agent()).ok()?,
    );
    // The codex HTTP client attaches `originator` to every authenticated backend
    // request via its default headers. The `/usage` probe builds its client
    // without those defaults, so set it explicitly to match a normal codex
    // request and avoid looking like non-codex automation.
    headers.insert("originator", originator().header_value);
    let token = auth.get_token().ok()?;
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}")).ok()?,
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
    Some(headers)
}

/// Makes a lightweight GET to the `/usage` endpoint and returns the HTTP
/// status code. Returns `None` on connection errors so the caller treats the
/// result as inconclusive.
async fn probe_usage_status(base_url: &str, auth: &CodexAuth) -> Option<reqwest::StatusCode> {
    let path = usage_endpoint_url(base_url);
    let headers = usage_request_headers(auth)?;
    // Bound the startup / failover probe so a hung or misconfigured ChatGPT
    // base URL cannot block AuthManager construction indefinitely.
    // Use the same narrowly allowlisted, process-shared Cloudflare cookie store
    // as ordinary ChatGPT backend traffic. A validation probe must be able to
    // reuse infrastructure cookies established by a model request, without
    // ever sharing account or session cookies across pool members.
    let client = build_reqwest_client_with_custom_ca(with_chatgpt_cloudflare_cookie_store(
        reqwest::Client::builder().timeout(Duration::from_secs(5)),
    ))
    .ok()?;
    client
        .get(&path)
        .headers(headers)
        .send()
        .await
        .ok()
        .map(|r| r.status())
}

/// Outcome of a `/usage` fetch that preserves the 401 distinction so callers
/// can mark an account invalid (authoritative) vs. leave it untouched (transient).
enum UsageFetchOutcome {
    Snapshots(Vec<RateLimitSnapshot>),
    /// HTTP 401 from `/usage`, carrying the (possibly empty) response body so the
    /// caller can distinguish an authoritative auth failure (`token_revoked`,
    /// `token_invalidated`, …) from an ambiguous 401 that must not permanently
    /// invalidate the account.
    Unauthorized {
        body: String,
    },
    Failed(ChatgptAccountPoolError),
}

/// Returns true only if a `/usage` 401 body explicitly confirms the credential is
/// dead, matching codex-accounts' `usageAuthFailureConfirmsInvalid`. An empty or
/// unrecognised body returns false (ambiguous — never authoritative), so the CLI
/// leaves invalidation to codex-accounts rather than killing an account on a
/// transient 401.
fn usage_auth_failure_confirms_invalid(body: &str) -> bool {
    matches!(
        usage_oauth_error_code(body).as_deref(),
        Some(
            "invalid_token"
                | "expired_token"
                | "token_expired"
                | "token_invalidated"
                | "token_revoked"
        )
    )
}

/// Extracts and normalises the OAuth-style error code from a `/usage` error body.
/// Handles both `{"error":"code"}` and `{"error":{"code":...,"type":...}}` shapes.
fn usage_oauth_error_code(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body.trim()).ok()?;
    let error = value.get("error")?;
    let code = match error {
        serde_json::Value::String(code) => code.clone(),
        serde_json::Value::Object(obj) => obj
            .get("code")
            .and_then(serde_json::Value::as_str)
            .or_else(|| obj.get("type").and_then(serde_json::Value::as_str))?
            .to_string(),
        _ => return None,
    };
    Some(code.trim().to_ascii_lowercase())
}

async fn fetch_usage_snapshots_with_status(base_url: &str, auth: &CodexAuth) -> UsageFetchOutcome {
    let path = usage_endpoint_url(base_url);
    let Some(headers) = usage_request_headers(auth) else {
        return UsageFetchOutcome::Failed(ChatgptAccountPoolError::Io(std::io::Error::other(
            "failed to build ChatGPT usage request headers",
        )));
    };
    // Keep rate-limit refreshes on the same allowlisted Cloudflare cookie path
    // as startup probes and the official backend client.
    let client = match build_reqwest_client_with_custom_ca(with_chatgpt_cloudflare_cookie_store(
        reqwest::Client::builder().timeout(Duration::from_secs(5)),
    )) {
        Ok(client) => client,
        Err(err) => {
            return UsageFetchOutcome::Failed(ChatgptAccountPoolError::Io(std::io::Error::other(
                err,
            )));
        }
    };
    let response = match client.get(&path).headers(headers).send().await {
        Ok(response) => response,
        Err(err) => {
            return UsageFetchOutcome::Failed(ChatgptAccountPoolError::Io(std::io::Error::other(
                err,
            )));
        }
    };
    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        // Read the body so the caller can classify the 401: an authoritative
        // auth-failure code permanently invalidates, an ambiguous 401 does not.
        let body = read_response_text_limited(response, MAX_AUTH_ERROR_BODY_BYTES)
            .await
            .unwrap_or_default();
        return UsageFetchOutcome::Unauthorized { body };
    }
    if !status.is_success() {
        return UsageFetchOutcome::Failed(ChatgptAccountPoolError::Io(std::io::Error::other(
            format!("rate-limit refresh failed with status {status}"),
        )));
    }
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string();
    let body = match read_response_text_limited(response, MAX_AUTH_SUCCESS_BODY_BYTES).await {
        Ok(body) => body,
        Err(err) => {
            return UsageFetchOutcome::Failed(ChatgptAccountPoolError::Io(std::io::Error::other(
                err,
            )));
        }
    };
    match serde_json::from_str::<RateLimitStatusPayload>(&body) {
        Ok(payload) => UsageFetchOutcome::Snapshots(rate_limit_snapshots_from_payload(payload)),
        Err(err) => UsageFetchOutcome::Failed(ChatgptAccountPoolError::Io(std::io::Error::other(
            format!("failed to decode rate-limit payload: {err}; content-type={content_type}"),
        ))),
    }
}

fn rate_limit_snapshots_from_payload(payload: RateLimitStatusPayload) -> Vec<RateLimitSnapshot> {
    rate_limit_snapshots_from_payload_at(payload, now_ts())
}

fn rate_limit_snapshots_from_payload_at(
    payload: RateLimitStatusPayload,
    captured_at: i64,
) -> Vec<RateLimitSnapshot> {
    let plan_type = Some(map_plan_type(payload.plan_type));
    let rate_limit_reached_type = payload
        .rate_limit_reached_type
        .flatten()
        .and_then(|details| map_rate_limit_reached_type(details.kind));
    let mut snapshots = vec![make_rate_limit_snapshot(
        RateLimitIdentity {
            id: Some("codex".to_string()),
            name: None,
        },
        payload.rate_limit.flatten().map(|details| *details),
        payload.credits.flatten().map(|details| *details),
        payload.spend_control.flatten().map(|details| *details),
        plan_type,
        rate_limit_reached_type,
        captured_at,
    )];
    if let Some(additional) = payload.additional_rate_limits.flatten() {
        snapshots.extend(additional.into_iter().map(|details| {
            make_rate_limit_snapshot(
                RateLimitIdentity {
                    id: Some(details.metered_feature),
                    name: Some(details.limit_name),
                },
                details.rate_limit.flatten().map(|rate_limit| *rate_limit),
                /*credits*/ None,
                /*spend_control*/ None,
                plan_type,
                /*rate_limit_reached_type*/ None,
                captured_at,
            )
        }));
    }
    snapshots
}

fn make_rate_limit_snapshot(
    identity: RateLimitIdentity,
    rate_limit: Option<BackendRateLimitStatusDetails>,
    credits: Option<CreditStatusDetails>,
    spend_control: Option<BackendSpendControlStatusDetails>,
    plan_type: Option<AccountPlanType>,
    rate_limit_reached_type: Option<RateLimitReachedType>,
    captured_at: i64,
) -> RateLimitSnapshot {
    let access_blocked = rate_limit
        .as_ref()
        .is_some_and(|details| !details.allowed || details.limit_reached);
    let rate_limit_reached_type = rate_limit_reached_type
        .or(access_blocked.then_some(RateLimitReachedType::RateLimitReached));
    let (primary, secondary) = match rate_limit {
        Some(details) => (
            map_rate_limit_window(details.primary_window, captured_at),
            map_rate_limit_window(details.secondary_window, captured_at),
        ),
        None => (None, None),
    };
    let spend_control_reached = spend_control.as_ref().map(|details| details.reached);
    let individual_limit = spend_control
        .and_then(|details| details.individual_limit.flatten())
        .map(|details| map_individual_limit(*details, captured_at));
    RateLimitSnapshot {
        limit_id: identity.id,
        limit_name: identity.name,
        primary,
        secondary,
        credits: map_credits(credits),
        individual_limit,
        spend_control_reached,
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
    captured_at: i64,
) -> Option<RateLimitWindow> {
    let snapshot = window.flatten().map(|details| *details)?;
    Some(RateLimitWindow {
        used_percent: f64::from(snapshot.used_percent),
        window_minutes: window_minutes_from_seconds(snapshot.limit_window_seconds),
        resets_at: reset_at_from_backend(
            snapshot.reset_at,
            snapshot.reset_after_seconds,
            captured_at,
        ),
    })
}

fn map_individual_limit(
    details: codex_backend_openapi_models::models::SpendControlLimitDetails,
    captured_at: i64,
) -> SpendControlLimitSnapshot {
    SpendControlLimitSnapshot {
        limit: details.limit,
        used: details.used,
        remaining_percent: details.remaining_percent,
        resets_at: reset_at_from_backend(
            details.reset_at,
            details.reset_after_seconds,
            captured_at,
        )
        .unwrap_or_default(),
    }
}

fn reset_at_from_backend(reset_at: i32, reset_after_seconds: i32, captured_at: i64) -> Option<i64> {
    if reset_at > 0 {
        return Some(i64::from(reset_at));
    }
    (reset_after_seconds > 0).then(|| captured_at.saturating_add(i64::from(reset_after_seconds)))
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
        BackendPlanType::SelfServeBusinessProLite => AccountPlanType::SelfServeBusinessProLite,
        BackendPlanType::SelfServeBusinessUsageBased => {
            AccountPlanType::SelfServeBusinessUsageBased
        }
        BackendPlanType::Business => AccountPlanType::Business,
        BackendPlanType::Ent26 => AccountPlanType::Ent26,
        BackendPlanType::EnterpriseCbpAutomation => AccountPlanType::EnterpriseCbpAutomation,
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

pub(crate) fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn window_minutes_from_seconds(seconds: i32) -> Option<i64> {
    if seconds <= 0 {
        return None;
    }
    // `i64::div_ceil` is still unstable on the pinned toolchain, and clippy's
    // `manual_div_ceil` only fires for unsigned types, so the manual ceiling
    // division is the right call for this signed value.
    let seconds_i64 = i64::from(seconds);
    Some((seconds_i64 + 59) / 60)
}

#[cfg(test)]
mod tests;
