use std::time::Duration;

use chrono::DateTime;
use chrono::Utc;
use sqlx::Row;

use super::ChatgptAccountPool;
use super::ChatgptAccountPoolError;
use super::account_suffix;
use super::now_ts;
use crate::AuthDotJson;
use crate::CodexAuth;
use crate::token_data::parse_jwt_expiration;

/// Outcome of preparing a pending account's auth for the validate-on-pickup
/// probe (see [`ChatgptAccountPool::refresh_pending_account_auth`]).
pub(crate) enum PendingAccountAuth {
    /// A live `CodexAuth` whose access token is fresh and ready to probe.
    Ready(CodexAuth),
    /// A live token is not available right now (the pool copy is still stale and
    /// codex-accounts has not refreshed it yet, or the secret is missing). The
    /// account is left `pending_validation` for a later attempt rather than
    /// condemned — codex-accounts owns invalidation.
    Inconclusive,
}

pub(crate) const ACCOUNT_TOKEN_REFRESH_EXPIRATION_SKEW_SECONDS: i64 = 60;
// 90 s gives a slow OAuth round-trip (including network + fsync) time to finish
// before the lock expires.  The previous 30 s was too tight: if the lock elapsed
// mid-call a second worker could acquire it and race the same rotating R2, causing
// refresh_token_reused and permanent account loss.
pub(crate) const ACCOUNT_TOKEN_REFRESH_LOCK_TTL_SECONDS: i64 = 90;

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
        // Write tokens into the DB columns — the single source of truth for
        // pool-account tokens. No per-account auth.json file is written.
        self.write_account_tokens(account_id, auth).await?;
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

    /// Ensures a pending account holds a live access token before it is probed
    /// during validate-on-pickup.
    ///
    /// An idle pending account can carry an expired access token; probing with it
    /// would yield a 401 and wrongly mark the account invalid. The CLI never
    /// refreshes pool-managed tokens itself — codex-accounts is the sole refresh
    /// authority — so when the cached token is stale this reloads the freshest copy
    /// codex-accounts has written to the account-pool secret. If it is now live the
    /// account is `Ready`; otherwise it is left `Inconclusive` (still
    /// `pending_validation`) for codex-accounts to refresh on its next cycle. This
    /// never calls the OAuth endpoint or takes the refresh lock, so it cannot burn
    /// the rotating refresh token.
    pub(crate) async fn refresh_pending_account_auth(
        &self,
        account_id: &str,
        auth: CodexAuth,
    ) -> Result<PendingAccountAuth, ChatgptAccountPoolError> {
        if !auth.chatgpt_access_token_is_stale() {
            return Ok(PendingAccountAuth::Ready(auth));
        }
        // Stale cached token: reload the freshest copy codex-accounts wrote. Use it
        // if live; otherwise leave the account pending for codex-accounts to refresh.
        match self.load_account_codex_auth(account_id).await? {
            Some(reloaded) if !reloaded.chatgpt_access_token_is_stale() => {
                Ok(PendingAccountAuth::Ready(reloaded))
            }
            _ => Ok(PendingAccountAuth::Inconclusive),
        }
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
               OR account_token_locks.locked_by = excluded.locked_by
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

    pub(crate) fn token_refresh_lock_ttl() -> Duration {
        Duration::from_secs(ACCOUNT_TOKEN_REFRESH_LOCK_TTL_SECONDS as u64)
    }
}
