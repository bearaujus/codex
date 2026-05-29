use std::sync::Arc;
use std::time::Duration;

use codex_login::AuthManager;
use tokio_util::sync::CancellationToken;

const ACCOUNT_POOL_ACTIVITY_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

pub(crate) struct AccountPoolActivityHeartbeat {
    cancellation_token: CancellationToken,
    _task: tokio::task::JoinHandle<()>,
}

impl AccountPoolActivityHeartbeat {
    pub(crate) async fn start(
        auth_manager: Arc<AuthManager>,
        turn_cancellation_token: &CancellationToken,
    ) -> Self {
        auth_manager.record_account_pool_activity().await;
        let cancellation_token = turn_cancellation_token.child_token();
        let task_cancellation_token = cancellation_token.clone();
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = task_cancellation_token.cancelled() => {
                        auth_manager.clear_account_pool_activity().await;
                        break;
                    }
                    _ = tokio::time::sleep(ACCOUNT_POOL_ACTIVITY_HEARTBEAT_INTERVAL) => {
                        auth_manager.record_account_pool_activity().await;
                    }
                }
            }
        });
        Self {
            cancellation_token,
            _task: task,
        }
    }
}

impl Drop for AccountPoolActivityHeartbeat {
    fn drop(&mut self) {
        self.cancellation_token.cancel();
    }
}
