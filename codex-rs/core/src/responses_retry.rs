//! Shared retry and transport fallback decisions for Responses requests.

use std::time::Duration;

use crate::client::ModelClientSession;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::util::backoff;
use codex_protocol::error::CodexErr;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::WarningEvent;
use tracing::warn;

#[derive(Debug, Clone, Copy)]
pub(crate) enum ResponsesStreamRequest {
    Sampling,
    RemoteCompactionV2,
}

/// Handles a retryable stream error and returns `Ok(())` when the caller should
/// retry the request loop.
pub(crate) async fn handle_retryable_response_stream_error(
    retries: &mut u64,
    max_retries: u64,
    err: CodexErr,
    client_session: &mut ModelClientSession,
    sess: &Session,
    turn_context: &TurnContext,
    request: ResponsesStreamRequest,
) -> Result<(), CodexErr> {
    if *retries >= max_retries
        && client_session.try_switch_fallback_transport(
            &turn_context.session_telemetry,
            &turn_context.model_info,
        )
    {
        sess.send_event(
            turn_context,
            EventMsg::Warning(WarningEvent {
                message: format!("Falling back from WebSockets to HTTPS transport. {err:#}"),
            }),
        )
        .await;
        // Reset the retry counter for the new transport so it receives its full
        // reconnect budget.
        *retries = 0;
        return Ok(());
    }

    if *retries < max_retries {
        *retries += 1;
        let retry_count = *retries;
        let delay = if retry_count == 1 {
            // First attempt: retry immediately — these errors are usually transient and
            // resolving on the first reconnect is common enough that adding latency here
            // hurts more than it helps.
            match &err {
                CodexErr::Stream(_, Some(requested_delay)) => *requested_delay,
                _ => Duration::ZERO,
            }
        } else {
            match &err {
                CodexErr::Stream(_, requested_delay) => {
                    requested_delay.unwrap_or_else(|| backoff(retry_count))
                }
                _ => backoff(retry_count),
            }
        };
        log_retry(request, turn_context, &err, retry_count, max_retries, delay);

        // Hide the first retry — it is silent and instant, so surfacing it would only
        // flash a spurious error for what is almost always a self-healing blip.
        if retry_count > 1 {
            // Surface retry information to any UI/front-end so the user understands what is
            // happening instead of staring at a seemingly frozen screen.
            sess.notify_stream_error(
                turn_context,
                format!("Reconnecting... {retry_count}/{max_retries}"),
                err,
            )
            .await;
        }
        tokio::time::sleep(delay).await;
        return Ok(());
    }

    Err(err)
}

fn log_retry(
    request: ResponsesStreamRequest,
    turn_context: &TurnContext,
    err: &CodexErr,
    retries: u64,
    max_retries: u64,
    delay: Duration,
) {
    match request {
        ResponsesStreamRequest::Sampling => {
            warn!(
                turn_id = %turn_context.sub_id,
                retries,
                max_retries,
                sampling_error = %err,
                "stream disconnected - retrying sampling request ({retries}/{max_retries} in {delay:?})...",
            );
        }
        ResponsesStreamRequest::RemoteCompactionV2 => {
            warn!(
                turn_id = %turn_context.sub_id,
                retries,
                max_retries,
                compact_error = %err,
                "remote compaction v2 stream failed; retrying request after delay"
            );
        }
    }
}

#[cfg(test)]
#[path = "responses_retry_tests.rs"]
mod tests;
