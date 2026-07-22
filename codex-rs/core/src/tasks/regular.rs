use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::session::AccountPoolActivityHeartbeat;
use crate::session::TurnInput;
use crate::session::turn::run_turn;
use crate::session::turn_context::TurnContext;
use crate::session_startup_prewarm::SessionStartupPrewarmResolution;
use crate::state::TaskKind;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TurnStartedEvent;
use tracing::Instrument;
use tracing::trace_span;

use super::SessionTask;
use super::SessionTaskContext;
use super::SessionTaskResult;

#[derive(Default)]
pub(crate) struct RegularTask;

impl RegularTask {
    pub(crate) fn new() -> Self {
        Self
    }
}

impl SessionTask for RegularTask {
    fn kind(&self) -> TaskKind {
        TaskKind::Regular
    }

    fn span_name(&self) -> &'static str {
        "session_task.turn"
    }

    async fn run(
        self: Arc<Self>,
        session: Arc<SessionTaskContext>,
        ctx: Arc<TurnContext>,
        input: Vec<TurnInput>,
        cancellation_token: CancellationToken,
    ) -> SessionTaskResult {
        let sess = session.clone_session();
        let turn_extension_data = session.turn_extension_data();
        let run_turn_span = trace_span!("run_turn");
        // Regular turns emit `TurnStarted` inline so first-turn lifecycle does
        // not wait on startup prewarm resolution.
        let prewarmed_client_session = async {
            let event = EventMsg::TurnStarted(TurnStartedEvent {
                turn_id: ctx.sub_id.clone(),
                trace_id: ctx.trace_id.clone(),
                started_at: ctx.turn_timing_state.started_at_unix_secs().await,
                model_context_window: ctx.model_context_window(),
                collaboration_mode_kind: ctx.mode,
            });
            sess.send_event(ctx.as_ref(), event).await;
            sess.set_server_reasoning_included(/*included*/ false).await;
            sess.consume_startup_prewarm_for_regular_turn(&cancellation_token)
                .await
        }
        .instrument(trace_span!("regular_task.prepare_run_turn"))
        .await;
        let prewarmed_client_session = match prewarmed_client_session {
            SessionStartupPrewarmResolution::Cancelled => return Ok(None),
            SessionStartupPrewarmResolution::Unavailable { .. } => None,
            SessionStartupPrewarmResolution::Ready(prewarmed_client_session) => {
                Some(*prewarmed_client_session)
            }
        };
        let mut next_input = input;
        let mut prewarmed_client_session = prewarmed_client_session;
        let account_pool_activity_heartbeat = AccountPoolActivityHeartbeat::start(
            Arc::clone(&sess.services.auth_manager),
            &cancellation_token,
        )
        .await;
        loop {
            let turn_result = run_turn(
                Arc::clone(&sess),
                Arc::clone(&ctx),
                Arc::clone(&turn_extension_data),
                next_input,
                prewarmed_client_session.take(),
                cancellation_token.child_token(),
            )
            .instrument(run_turn_span.clone())
            .await?;
            if sess.input_queue.has_pending_input(&sess.active_turn).await {
                next_input = Vec::new();
                continue;
            }

            let last_agent_message = turn_result.last_agent_message;
            if !turn_result.completed_successfully {
                account_pool_activity_heartbeat.shutdown().await;
                return Ok(last_agent_message);
            }
            let Some(serving_account_id) = turn_result.serving_account_id else {
                account_pool_activity_heartbeat.shutdown().await;
                return Ok(last_agent_message);
            };

            sess.services
                .auth_manager
                .record_pool_account_activity_for(&serving_account_id)
                .await;

            // A logical protocol turn can call `run_turn` more than once while
            // draining steering input. Read the service-owned cache only after
            // the outer task is truly complete, and bind the detached result to
            // the account that served its final sampling request.
            let probe_sess = Arc::clone(&sess);
            let probe_turn_context = Arc::clone(&ctx);
            tokio::spawn(async move {
                let probe = probe_sess
                    .services
                    .auth_manager
                    .load_cached_rate_limits_post_turn(&serving_account_id)
                    .await;
                for snapshot in probe.snapshots {
                    probe_sess
                        .update_rate_limits(&probe_turn_context, snapshot)
                        .await;
                }
                account_pool_activity_heartbeat.shutdown().await;
            });
            return Ok(last_agent_message);
        }
    }
}
