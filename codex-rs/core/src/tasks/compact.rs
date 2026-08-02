use std::sync::Arc;

use super::SessionTask;
use super::SessionTaskResult;
use super::emit_compact_metric;
use crate::session::AccountPoolActivityHeartbeat;
use crate::session::TurnInput;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::state::TaskKind;
use codex_features::Feature;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::user_input::UserInput;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Default)]
pub(crate) struct CompactTask;

impl SessionTask for CompactTask {
    fn kind(&self) -> TaskKind {
        TaskKind::Compact
    }

    fn span_name(&self) -> &'static str {
        "session_task.compact"
    }

    async fn run(
        self: Arc<Self>,
        session: Arc<Session>,
        ctx: Arc<TurnContext>,
        _input: Vec<TurnInput>,
        cancellation_token: CancellationToken,
    ) -> SessionTaskResult {
        let _profile_guard = ctx.turn_timing_state.begin_compaction();
        if ctx.config.features.enabled(Feature::TokenBudget) {
            crate::compact_token_budget::run_manual_compact_task(session, ctx).await?;
            return Ok(None);
        }

        let account_pool_activity_heartbeat = AccountPoolActivityHeartbeat::start(
            Arc::clone(&session.services.auth_manager),
            &cancellation_token,
        )
        .await;
        let mut client_session = session.services.model_client.new_session();
        client_session
            .set_account_pool_activity_tracker(account_pool_activity_heartbeat.request_tracker());
        let result = if crate::compact::should_use_remote_compact_task(ctx.provider.info()) {
            if ctx
                .config
                .features
                .enabled(codex_features::Feature::RemoteCompactionV2)
            {
                emit_compact_metric(
                    &session.services.session_telemetry,
                    "remote_v2",
                    /*manual*/ true,
                );
                crate::compact_remote_v2::run_remote_compact_task(
                    session.clone(),
                    ctx.clone(),
                    &mut client_session,
                )
                .await
            } else {
                emit_compact_metric(
                    &session.services.session_telemetry,
                    "remote",
                    /*manual*/ true,
                );
                crate::compact_remote::run_remote_compact_task(
                    session.clone(),
                    ctx.clone(),
                    &mut client_session,
                )
                .await
            }
        } else {
            emit_compact_metric(
                &session.services.session_telemetry,
                "local",
                /*manual*/ true,
            );
            let input = vec![UserInput::Text {
                text: ctx
                    .config
                    .compact_prompt
                    .as_deref()
                    .unwrap_or(crate::compact::SUMMARIZATION_PROMPT)
                    .to_string(),
                // Compaction prompt is synthesized; no UI element ranges to preserve.
                text_elements: Vec::new(),
            }];
            crate::compact::run_compact_task(
                session.clone(),
                ctx.clone(),
                input,
                &mut client_session,
            )
            .await
        };
        let failing_account_id = client_session
            .last_request_pool_account_id()
            .map(str::to_string);
        match &result {
            Err(err) if let CodexErrorDetails::UsageLimitReached(error) = err.details() => {
                if let Some(rate_limits) = error.rate_limits.as_deref() {
                    session.update_rate_limits(&ctx, rate_limits.clone()).await;
                }
                if let Err(pool_error) = session
                    .services
                    .auth_manager
                    .handle_chatgpt_account_pool_usage_limit(
                        failing_account_id.as_deref(),
                        /*safe_to_retry*/ false,
                        error.rate_limits.as_deref(),
                        error.resets_at,
                    )
                    .await
                {
                    tracing::warn!(
                        "failed to process ChatGPT account-pool usage limit after manual compaction: {pool_error}"
                    );
                }
            }
            Err(err) if let CodexErrorDetails::RefreshTokenFailed(error) = err.details() => {
                if let Err(pool_error) = session
                    .services
                    .auth_manager
                    .handle_chatgpt_account_pool_auth_failure(
                        failing_account_id.as_deref(),
                        /*safe_to_retry*/ false,
                        error,
                    )
                    .await
                {
                    tracing::warn!(
                        "failed to process ChatGPT account-pool auth failure after manual compaction: {pool_error}"
                    );
                }
            }
            _ => {}
        }
        let active_account_changed = session
            .services
            .auth_manager
            .auth_cached()
            .and_then(|auth| auth.get_pool_account_id())
            != failing_account_id;
        if active_account_changed {
            session.clear_rate_limits(&ctx).await;
        }
        account_pool_activity_heartbeat.shutdown().await;
        if let Err(err) = result
            && matches!(err.details(), CodexErrorDetails::TurnAborted)
        {
            return Err(err);
        }
        Ok(None)
    }
}
