use std::time::Duration;

use chrono::DateTime;
use chrono::Utc;
use codex_config::host_name;
use sqlx::Row;

use super::ChatgptAccountPool;
use super::ChatgptAccountPoolError;
use super::account_suffix;
use super::now_ts;
use crate::AuthDotJson;
use crate::save_auth;
use crate::token_data::parse_jwt_expiration;

pub(crate) const ACCOUNT_TOKEN_REFRESH_EXPIRATION_SKEW_SECONDS: i64 = 60;
pub(crate) const ACCOUNT_TOKEN_REFRESH_LOCK_TTL_SECONDS: i64 = 30;

impl ChatgptAccountPool {
    pub async fn try_acquire_token_refresh_lock(
        &self,
        account_id: &str,
        owner: &str,
        ttl: Duration,
    ) -> Result<bool, ChatgptAccountPoolError> {
        self.try_acquire_token_refresh_lock_at(account_id, owner, ttl, now_ts())
            .await
    }

    pub async fn release_token_refresh_lock(
        &self,
        account_id: &str,
        owner: &str,
    ) -> Result<(), ChatgptAccountPoolError> {
        sqlx::query(
            r#"
            DELETE FROM account_token_locks
            WHERE account_id = ? AND locked_by = ?
            "#,
        )
        .bind(account_id)
        .bind(owner)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn persist_refreshed_account_auth(
        &self,
        account_id: &str,
        auth: &AuthDotJson,
    ) -> Result<(), ChatgptAccountPoolError> {
        self.require_account(account_id).await?;
        save_auth(
            &self.secret_codex_home(account_id),
            auth,
            self.auth_credentials_store_mode,
        )?;
        let now = now_ts();
        sqlx::query(
            r#"
            UPDATE accounts
            SET last_auth_refresh_at = ?, updated_at = ?, auth_status = ?
            WHERE account_id = ?
            "#,
        )
        .bind(now)
        .bind(now)
        .bind(super::ChatgptAccountPoolAuthStatus::Valid.as_str())
        .bind(account_id)
        .execute(&self.pool)
        .await?;
        self.append_event(
            Some(account_id),
            "account_auth_refreshed",
            format!(
                "Persisted refreshed auth for ChatGPT account {}",
                account_suffix(account_id)
            ),
        )
        .await?;
        Ok(())
    }

    pub async fn account_last_auth_refresh_at(
        &self,
        account_id: &str,
    ) -> Result<Option<i64>, ChatgptAccountPoolError> {
        let row = sqlx::query(
            r#"
            SELECT last_auth_refresh_at
            FROM accounts
            WHERE account_id = ?
            LIMIT 1
            "#,
        )
        .bind(account_id)
        .fetch_optional(&self.pool)
        .await?;
        match row {
            Some(row) => Ok(row.get("last_auth_refresh_at")),
            None => Err(ChatgptAccountPoolError::AccountNotFound(
                account_id.to_string(),
            )),
        }
    }

    pub(crate) async fn try_acquire_token_refresh_lock_at(
        &self,
        account_id: &str,
        owner: &str,
        ttl: Duration,
        acquired_at: i64,
    ) -> Result<bool, ChatgptAccountPoolError> {
        let ttl_seconds = i64::try_from(ttl.as_secs())
            .map_err(|_| std::io::Error::other("token refresh lock ttl exceeded i64 seconds"))?;
        let expires_at = acquired_at.saturating_add(ttl_seconds);
        let rows_affected = sqlx::query(
            r#"
            INSERT INTO account_token_locks (account_id, locked_by, acquired_at, expires_at)
            VALUES (?, ?, ?, ?)
            ON CONFLICT(account_id) DO UPDATE SET
                locked_by = excluded.locked_by,
                acquired_at = excluded.acquired_at,
                expires_at = excluded.expires_at
            WHERE account_token_locks.expires_at <= excluded.acquired_at
            "#,
        )
        .bind(account_id)
        .bind(owner)
        .bind(acquired_at)
        .bind(expires_at)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(rows_affected == 1)
    }

    pub(crate) fn account_auth_needs_token_refresh(auth: &AuthDotJson, now: DateTime<Utc>) -> bool {
        let Some(tokens) = auth.tokens.as_ref() else {
            return true;
        };
        match parse_jwt_expiration(&tokens.access_token) {
            Ok(Some(expires_at)) => {
                expires_at
                    <= now
                        + chrono::Duration::seconds(ACCOUNT_TOKEN_REFRESH_EXPIRATION_SKEW_SECONDS)
            }
            Ok(None) | Err(_) => true,
        }
    }

    pub(crate) fn token_refresh_lock_owner() -> String {
        let host = host_name()
            .or_else(|| super::non_empty_env("HOSTNAME"))
            .or_else(|| super::non_empty_env("COMPUTERNAME"))
            .unwrap_or_else(|| "unknown".to_string());
        format!("{host}:{}", std::process::id())
    }

    pub(crate) fn token_refresh_lock_ttl() -> Duration {
        Duration::from_secs(ACCOUNT_TOKEN_REFRESH_LOCK_TTL_SECONDS as u64)
    }
}
