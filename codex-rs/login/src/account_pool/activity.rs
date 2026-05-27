use std::env;

use codex_config::host_name;

use super::ChatgptAccountPool;
use super::ChatgptAccountPoolError;
use super::now_ts;

pub(crate) const ACCOUNT_ACTIVITY_TTL_SECONDS: i64 = 60;

struct AccountActivityOwner {
    owner_pid: i64,
    host: String,
}

impl ChatgptAccountPool {
    pub async fn record_account_activity(&self, account_id: &str) {
        let owner = current_account_activity_owner();
        if let Err(err) = self
            .record_account_activity_for_owner_at(
                account_id,
                owner.owner_pid,
                &owner.host,
                now_ts(),
            )
            .await
        {
            tracing::warn!(
                account_id,
                owner_pid = owner.owner_pid,
                host = %owner.host,
                "failed to record ChatGPT account activity heartbeat: {err}"
            );
        }
    }

    pub async fn clear_account_activity(&self, account_id: &str) {
        let owner = current_account_activity_owner();
        if let Err(err) = self
            .clear_account_activity_for_owner_at(account_id, owner.owner_pid, &owner.host, now_ts())
            .await
        {
            tracing::warn!(
                account_id,
                owner_pid = owner.owner_pid,
                host = %owner.host,
                "failed to clear ChatGPT account activity heartbeat: {err}"
            );
        }
    }

    pub(crate) async fn record_account_activity_for_owner_at(
        &self,
        account_id: &str,
        owner_pid: i64,
        host: &str,
        now: i64,
    ) -> Result<(), ChatgptAccountPoolError> {
        self.gc_expired_account_activity(now).await?;
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
            ON CONFLICT(account_id, owner_pid, host) DO UPDATE SET
                heartbeat_at = excluded.heartbeat_at,
                expires_at = excluded.expires_at
            "#,
        )
        .bind(account_id)
        .bind(owner_pid)
        .bind(host)
        .bind(now)
        .bind(now)
        .bind(now + ACCOUNT_ACTIVITY_TTL_SECONDS)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub(crate) async fn clear_account_activity_for_owner_at(
        &self,
        account_id: &str,
        owner_pid: i64,
        host: &str,
        now: i64,
    ) -> Result<(), ChatgptAccountPoolError> {
        self.gc_expired_account_activity(now).await?;
        sqlx::query(
            r#"
            DELETE FROM account_activity
            WHERE account_id = ? AND owner_pid = ? AND host = ?
            "#,
        )
        .bind(account_id)
        .bind(owner_pid)
        .bind(host)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn gc_expired_account_activity(&self, now: i64) -> Result<(), ChatgptAccountPoolError> {
        sqlx::query("DELETE FROM account_activity WHERE expires_at < ?")
            .bind(now)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

fn current_account_activity_owner() -> AccountActivityOwner {
    AccountActivityOwner {
        owner_pid: i64::from(std::process::id()),
        host: current_account_activity_host(),
    }
}

fn current_account_activity_host() -> String {
    host_name()
        .or_else(|| non_empty_env("HOSTNAME"))
        .or_else(|| non_empty_env("COMPUTERNAME"))
        .unwrap_or_else(|| "unknown".to_string())
}

fn non_empty_env(key: &str) -> Option<String> {
    env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}
