use codex_core::config::Config;
use codex_login::AuthManager;
use codex_protocol::protocol::RateLimitSnapshot;
use codex_protocol::protocol::RateLimitWindow;
use tracing::info;
use tracing::warn;

pub(crate) async fn rate_limits_ok(auth_manager: &AuthManager, config: &Config) -> bool {
    rate_limits_check(auth_manager, config)
        .await
        .unwrap_or(true)
}

async fn rate_limits_check(auth_manager: &AuthManager, config: &Config) -> Option<bool> {
    let auth = auth_manager.auth().await?;
    if !auth.uses_codex_backend() {
        return None;
    }
    let account_id = auth.get_pool_account_id()?;
    let account_pool = auth_manager.chatgpt_account_pool()?;
    let cached_entry = account_pool
        .list_rate_limits()
        .await
        .map_err(|err| warn!(%err, "failed to read cached rate limits"))
        .ok()?
        .into_iter()
        .find(|entry| entry.account_id == account_id)?;
    let snapshot = cached_entry
        .rate_limits
        .get(crate::guard_limits::CODEX_LIMIT_ID)
        .or_else(|| cached_entry.rate_limits.values().next())?;

    let min_remaining_percent = config.memories.min_rate_limit_remaining_percent;
    let allowed = snapshot_allows_startup(snapshot, min_remaining_percent);

    if !allowed {
        info!(
            min_remaining_percent,
            "skipping memories startup because Codex rate limits are below the configured threshold"
        );
    }

    Some(allowed)
}

fn snapshot_allows_startup(snapshot: &RateLimitSnapshot, min_remaining_percent: i64) -> bool {
    if snapshot.rate_limit_reached_type.is_some() {
        return false;
    }

    let max_used_percent = 100.0 - min_remaining_percent.clamp(0, 100) as f64;
    window_allows_startup(snapshot.primary.as_ref(), max_used_percent)
        && window_allows_startup(snapshot.secondary.as_ref(), max_used_percent)
}

fn window_allows_startup(window: Option<&RateLimitWindow>, max_used_percent: f64) -> bool {
    match window {
        Some(window) => window.used_percent <= max_used_percent,
        None => true,
    }
}

#[cfg(test)]
#[path = "guard_tests.rs"]
mod tests;
