use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;

use codex_login::AuthManager;
use tokio_util::sync::CancellationToken;

const ACCOUNT_POOL_ACTIVITY_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

/// Process-global reference counts of how many live turn heartbeats are currently
/// keeping each pool account marked "in use".
///
/// Every regular task owns one [`AccountPoolActivityHeartbeat`], and when the logical
/// turn completes the heartbeat is handed to a detached post-turn cache-read task. If
/// the next turn starts before that task finishes, two heartbeats are live at once.
/// They all share a
/// single activity owner key (pid + host) in the pool DB, so a naive per-turn
/// clear-on-teardown from the earlier turn would delete the in-use marker the
/// still-running later turn depends on. With the marker gone, codex-accounts (which
/// treats `account_activity` rows with `expires_at > now` as "in use" and refuses to
/// rotate or invalidate in-use accounts) would believe the live account is idle and
/// could refresh/rotate its token mid-turn — churning the rotating refresh token.
///
/// Gating the DB clear on the refcount dropping to zero keeps the marker alive until
/// the last turn using the account has finished. The active pool account is
/// process-global (a single `auth_cached` slot), so keying by account id alone is
/// sufficient: at any instant every live heartbeat tracks the same account.
fn account_activity_refcounts() -> &'static Mutex<HashMap<String, usize>> {
    static REFCOUNTS: OnceLock<Mutex<HashMap<String, usize>>> = OnceLock::new();
    REFCOUNTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Registers one more live holder of `account_id`'s in-use marker.
fn acquire_activity(account_id: &str) {
    let mut counts = account_activity_refcounts()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *counts.entry(account_id.to_string()).or_insert(0) += 1;
}

/// Drops one live holder of `account_id`'s in-use marker. Returns `true` when this
/// was the last holder and the DB row should now be cleared.
fn release_activity(account_id: &str) -> bool {
    let mut counts = account_activity_refcounts()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match counts.get_mut(account_id) {
        Some(count) => {
            *count = count.saturating_sub(1);
            if *count == 0 {
                counts.remove(account_id);
                true
            } else {
                false
            }
        }
        // Untracked account: default to clearing so a row is never leaked.
        None => true,
    }
}

pub(crate) struct AccountPoolActivityHeartbeat {
    cancellation_token: CancellationToken,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl AccountPoolActivityHeartbeat {
    pub(crate) async fn start(
        auth_manager: Arc<AuthManager>,
        turn_cancellation_token: &CancellationToken,
    ) -> Self {
        let mut owned_account_id = None;
        sync_account_pool_activity(auth_manager.as_ref(), &mut owned_account_id).await;
        let cancellation_token = turn_cancellation_token.child_token();
        let task_cancellation_token = cancellation_token.clone();
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = task_cancellation_token.cancelled() => {
                        // A failover may have recorded the newly active account directly
                        // before this heartbeat's next periodic tick. Reconcile ownership
                        // once more so teardown clears that account instead of only the
                        // stale account captured before failover.
                        sync_account_pool_activity(auth_manager.as_ref(), &mut owned_account_id).await;
                        teardown_account_pool_activity(auth_manager.as_ref(), &mut owned_account_id).await;
                        break;
                    }
                    _ = tokio::time::sleep(ACCOUNT_POOL_ACTIVITY_HEARTBEAT_INTERVAL) => {
                        sync_account_pool_activity(auth_manager.as_ref(), &mut owned_account_id).await;
                    }
                }
            }
        });
        Self {
            cancellation_token,
            task: Some(task),
        }
    }

    pub(crate) async fn shutdown(mut self) {
        self.cancellation_token.cancel();
        if let Some(task) = self.task.take()
            && let Err(err) = task.await
        {
            tracing::warn!("account-pool activity heartbeat task failed during shutdown: {err}");
        }
    }
}

impl Drop for AccountPoolActivityHeartbeat {
    fn drop(&mut self) {
        self.cancellation_token.cancel();
    }
}

/// Reconciles this heartbeat's in-use marker with the currently active pool account.
///
/// - When the active account is unchanged, the marker is renewed (refreshing its
///   `expires_at`) so codex-accounts keeps seeing the account as in use.
/// - When the active account has switched, the previously-owned account is released
///   (clearing its DB row only when this was its last live holder) and the new
///   account is acquired and recorded.
async fn sync_account_pool_activity(
    auth_manager: &AuthManager,
    owned_account_id: &mut Option<String>,
) {
    let current_account_id = auth_manager
        .auth_cached()
        .and_then(|auth| auth.get_pool_account_id());
    if current_account_id == *owned_account_id {
        if let Some(account_id) = current_account_id.as_deref() {
            auth_manager
                .record_pool_account_activity_for(account_id)
                .await;
        }
        return;
    }

    if let Some(account_id) = owned_account_id.as_deref()
        && release_activity(account_id)
    {
        auth_manager
            .clear_pool_account_activity_for(account_id)
            .await;
    }
    if let Some(account_id) = current_account_id.as_deref() {
        acquire_activity(account_id);
        auth_manager
            .record_pool_account_activity_for(account_id)
            .await;
    }
    *owned_account_id = current_account_id;
}

/// Releases this heartbeat's hold on its owned account when the turn ends. The DB
/// row is cleared only when no other live turn heartbeat is still using the account,
/// so an earlier turn tearing down never wipes the in-use marker of a turn that is
/// still running on the same account.
async fn teardown_account_pool_activity(
    auth_manager: &AuthManager,
    owned_account_id: &mut Option<String>,
) {
    if let Some(account_id) = owned_account_id.take()
        && release_activity(&account_id)
    {
        auth_manager
            .clear_pool_account_activity_for(&account_id)
            .await;
    }
}

#[cfg(test)]
#[path = "account_pool_activity_tests.rs"]
mod tests;
