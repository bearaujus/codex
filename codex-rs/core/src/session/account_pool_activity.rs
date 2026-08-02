use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;

use codex_login::AuthManager;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

const ACCOUNT_POOL_ACTIVITY_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

/// Process-global reference counts of how many live turn-owned heartbeats are
/// currently keeping each pool account marked "in use".
///
/// Every model-backed regular or compaction task owns one
/// [`AccountPoolActivityHeartbeat`]. When a regular logical turn completes, its
/// heartbeat is handed to a detached post-turn cache-read task. If the next turn
/// starts before that task finishes, two heartbeats are live at once.
/// They all share a single activity owner key (pid + host) in the pool DB for a
/// given account, so a naive per-turn
/// clear-on-teardown from the earlier turn would delete the in-use marker the
/// still-running later turn depends on. With the marker gone, codex-accounts (which
/// treats `account_activity` rows with `expires_at > now` as "in use" and refuses to
/// rotate or invalidate in-use accounts) would believe the live account is idle and
/// could refresh/rotate its token mid-turn — churning the rotating refresh token.
///
/// Gating the DB clear on the refcount dropping to zero keeps the marker alive
/// until the last turn using the account has finished. Different turns in one
/// process may temporarily own different accounts after one of them fails over,
/// so each heartbeat remains pinned to its turn's account until that turn
/// explicitly moves it.
fn account_activity_refcounts() -> &'static Mutex<HashMap<String, usize>> {
    static REFCOUNTS: OnceLock<Mutex<HashMap<String, usize>>> = OnceLock::new();
    REFCOUNTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Serializes in-process activity-row changes through their database writes.
///
/// Updating only the refcount under a synchronous mutex is insufficient: a last
/// release could decide to clear an account, another task could acquire and record
/// it, and then the delayed clear could erase the new holder's row. A semaphore is
/// intentionally held across the async DB calls so those decisions and writes stay
/// ordered without holding a mutex guard across an await point.
fn account_activity_operation_gate() -> &'static Semaphore {
    static OPERATION_GATE: OnceLock<Semaphore> = OnceLock::new();
    OPERATION_GATE.get_or_init(|| Semaphore::new(1))
}

async fn acquire_activity_operation_permit() -> Option<tokio::sync::SemaphorePermit<'static>> {
    match account_activity_operation_gate().acquire().await {
        Ok(permit) => Some(permit),
        Err(err) => {
            tracing::error!(
                %err,
                "account activity operation gate closed; skipping activity-row mutation"
            );
            None
        }
    }
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
    auth_manager: Arc<AuthManager>,
    owned_account_id: Arc<Mutex<Option<String>>>,
    cancellation_token: CancellationToken,
    task: Option<tokio::task::JoinHandle<()>>,
}

/// Cloneable request hook that moves its owning task's activity lease before
/// an exact credential is attached to a model request.
///
/// Auth recovery can switch accounts inside `ModelClientSession::stream`,
/// before control returns to the turn loop. Keeping this hook on the client
/// session closes that window so codex-accounts never sees the replacement
/// account as idle while its request is already in flight.
#[derive(Clone)]
pub(crate) struct AccountPoolActivityRequestTracker {
    auth_manager: Arc<AuthManager>,
    owned_account_id: Arc<Mutex<Option<String>>>,
}

impl AccountPoolActivityRequestTracker {
    pub(crate) async fn track_request_account(&self, account_id: Option<String>) {
        switch_account_pool_activity(
            self.auth_manager.as_ref(),
            self.owned_account_id.as_ref(),
            account_id,
        )
        .await;
    }
}

impl AccountPoolActivityHeartbeat {
    pub(crate) async fn start(
        auth_manager: Arc<AuthManager>,
        turn_cancellation_token: &CancellationToken,
    ) -> Self {
        let initial_account_id = auth_manager
            .auth_cached()
            .and_then(|auth| auth.get_pool_account_id());
        let owned_account_id = Arc::new(Mutex::new(None));
        switch_account_pool_activity(
            auth_manager.as_ref(),
            owned_account_id.as_ref(),
            initial_account_id,
        )
        .await;
        let cancellation_token = turn_cancellation_token.child_token();
        let task_cancellation_token = cancellation_token.clone();
        let task_auth_manager = Arc::clone(&auth_manager);
        let task_owned_account_id = Arc::clone(&owned_account_id);
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = task_cancellation_token.cancelled() => {
                        teardown_account_pool_activity(
                            task_auth_manager.as_ref(),
                            task_owned_account_id.as_ref(),
                        )
                        .await;
                        break;
                    }
                    _ = tokio::time::sleep(ACCOUNT_POOL_ACTIVITY_HEARTBEAT_INTERVAL) => {
                        renew_account_pool_activity(
                            task_auth_manager.as_ref(),
                            task_owned_account_id.as_ref(),
                        )
                        .await;
                    }
                }
            }
        });
        Self {
            auth_manager,
            owned_account_id,
            cancellation_token,
            task: Some(task),
        }
    }

    /// Moves only this turn's lease to the currently cached account. Other
    /// overlapping turns keep their prior account leased until they finish or
    /// independently fail over.
    pub(crate) async fn switch_to_current_account(&self) {
        let current_account_id = self
            .auth_manager
            .auth_cached()
            .and_then(|auth| auth.get_pool_account_id());
        switch_account_pool_activity(
            self.auth_manager.as_ref(),
            self.owned_account_id.as_ref(),
            current_account_id,
        )
        .await;
    }

    pub(crate) async fn switch_to_account(&self, account_id: &str) {
        switch_account_pool_activity(
            self.auth_manager.as_ref(),
            self.owned_account_id.as_ref(),
            Some(account_id.to_string()),
        )
        .await;
    }

    pub(crate) fn request_tracker(&self) -> AccountPoolActivityRequestTracker {
        AccountPoolActivityRequestTracker {
            auth_manager: Arc::clone(&self.auth_manager),
            owned_account_id: Arc::clone(&self.owned_account_id),
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

/// Moves this heartbeat's in-use marker to its turn's newly serving account.
///
/// - When the turn account is unchanged, the marker is renewed (refreshing its
///   `expires_at`) so codex-accounts keeps seeing the account as in use.
/// - When this turn has switched, the previously-owned account is released
///   (clearing its DB row only when this was its last live holder) and the new
///   account is acquired and recorded.
async fn switch_account_pool_activity(
    auth_manager: &AuthManager,
    owned_account_id: &Mutex<Option<String>>,
    current_account_id: Option<String>,
) {
    let Some(_operation_permit) = acquire_activity_operation_permit().await else {
        return;
    };
    let previous_account_id = {
        let mut owned = owned_account_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if current_account_id == *owned {
            None
        } else {
            Some(std::mem::replace(&mut *owned, current_account_id.clone()))
        }
    };

    let Some(previous_account_id) = previous_account_id else {
        if let Some(account_id) = current_account_id.as_deref() {
            auth_manager
                .record_pool_account_activity_for(account_id)
                .await;
        }
        return;
    };

    if let Some(account_id) = current_account_id.as_deref() {
        acquire_activity(account_id);
        auth_manager
            .record_pool_account_activity_for(account_id)
            .await;
    }
    if let Some(account_id) = previous_account_id.as_deref()
        && release_activity(account_id)
    {
        auth_manager
            .clear_pool_account_activity_for(account_id)
            .await;
    }
}

async fn renew_account_pool_activity(
    auth_manager: &AuthManager,
    owned_account_id: &Mutex<Option<String>>,
) {
    let Some(_operation_permit) = acquire_activity_operation_permit().await else {
        return;
    };
    let account_id = owned_account_id
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    if let Some(account_id) = account_id.as_deref() {
        auth_manager
            .record_pool_account_activity_for(account_id)
            .await;
    }
}

/// Releases this heartbeat's hold on its owned account when the turn ends. The DB
/// row is cleared only when no other live turn heartbeat is still using the account,
/// so an earlier turn tearing down never wipes the in-use marker of a turn that is
/// still running on the same account.
async fn teardown_account_pool_activity(
    auth_manager: &AuthManager,
    owned_account_id: &Mutex<Option<String>>,
) {
    let Some(_operation_permit) = acquire_activity_operation_permit().await else {
        return;
    };
    let account_id = owned_account_id
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    if let Some(account_id) = account_id
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
