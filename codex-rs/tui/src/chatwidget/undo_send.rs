//! Locally reversible composer submission grace window.
//!
//! Regular composer messages remain entirely inside the TUI for five seconds.
//! They are absent from transcript and persistent history until the grace
//! window expires, so Escape can restore the exact editable payload without a
//! protocol-level rollback.

use std::collections::HashSet;
use std::time::Duration;
use std::time::Instant;

use super::user_messages::remap_colliding_paste_placeholders;
use super::*;

#[cfg(not(test))]
const DEFAULT_UNDO_SEND_DELAY: Duration = Duration::from_secs(5);
#[cfg(test)]
const DEFAULT_UNDO_SEND_DELAY: Duration = Duration::ZERO;
const MIN_UNDO_SEND_TICK_DELAY: Duration = Duration::from_millis(16);

#[derive(Debug)]
pub(super) struct UndoSendState {
    delay: Duration,
}

impl Default for UndoSendState {
    fn default() -> Self {
        Self {
            delay: DEFAULT_UNDO_SEND_DELAY,
        }
    }
}

impl ChatWidget {
    pub(super) fn stage_user_message_for_undo(
        &mut self,
        user_message: UserMessage,
        restore_draft: ComposerDraftSnapshot,
        route: StagedInputRoute,
    ) {
        if self.undo_send.delay.is_zero() {
            self.dispatch_staged_user_message(user_message, route);
            return;
        }

        let now = Instant::now();
        let local_history_entry_id = restore_draft.local_history_entry_id;
        self.input_queue
            .staged_user_messages
            .push_back(StagedUserMessage {
                user_message,
                route,
                restore_composer: thread_composer_state_from_snapshot(restore_draft),
                local_history_entry_id,
                send_at: now + self.undo_send.delay,
            });
        self.refresh_pending_input_preview();
        self.schedule_next_undo_send_tick(now);
    }

    /// Process expired grace windows before a frame renders.
    pub(super) fn tick_undo_send(&mut self, now: Instant) {
        let mut due = Vec::new();
        while self
            .input_queue
            .staged_user_messages
            .front()
            .is_some_and(|message| message.send_at <= now)
        {
            if let Some(message) = self.input_queue.staged_user_messages.pop_front() {
                due.push(message);
            }
        }

        let had_due = !due.is_empty();
        if had_due {
            for message in due {
                self.dispatch_staged_user_message(message.user_message, message.route);
            }
        }
        if had_due || self.has_staged_submission() {
            self.refresh_pending_input_preview_at(now);
        }
        self.schedule_next_undo_send_tick(now);
    }

    pub(super) fn cancel_latest_staged_submission(&mut self) -> bool {
        let Some(staged) = self.input_queue.staged_user_messages.pop_back() else {
            return false;
        };
        self.bottom_pane.clear_esc_backtrack_hint();
        if let Some(id) = staged.local_history_entry_id {
            self.bottom_pane.remove_local_submission_history(id);
        }
        self.restore_cancelled_submission(staged.restore_composer);
        self.refresh_pending_input_preview();
        self.request_redraw();
        true
    }

    pub(crate) fn has_staged_submission(&self) -> bool {
        !self.input_queue.staged_user_messages.is_empty()
    }

    fn dispatch_staged_user_message(&mut self, user_message: UserMessage, route: StagedInputRoute) {
        match route {
            StagedInputRoute::Dispatch => self.dispatch_composer_user_message(user_message),
            StagedInputRoute::Queue {
                action,
                pending_pastes,
            } => {
                self.input_queue
                    .queued_user_messages
                    .push_back(QueuedUserMessage {
                        user_message,
                        action,
                        pending_pastes,
                    });
                self.input_queue
                    .queued_user_message_history_records
                    .push_back(UserMessageHistoryRecord::UserMessageText);
                self.refresh_pending_input_preview();
                if self.is_session_configured()
                    && !self.is_user_turn_pending_or_running()
                    && !self.input_queue.suppress_queue_autosend
                {
                    self.maybe_send_next_queued_input();
                }
            }
        }
    }

    fn restore_cancelled_submission(&mut self, cancelled: ThreadComposerState) {
        let current =
            thread_composer_state_from_snapshot(self.bottom_pane.composer_draft_snapshot());
        if !current.has_content() {
            self.restore_composer_state(cancelled);
            return;
        }

        let (cancelled_message, cancelled_pastes) =
            user_message_and_pastes_from_composer_state(cancelled);
        let (current_message, current_pastes) =
            user_message_and_pastes_from_composer_state(current);
        let mut used_placeholders = HashSet::new();
        let (cancelled_message, cancelled_pastes) = remap_colliding_paste_placeholders(
            cancelled_message,
            cancelled_pastes,
            &mut used_placeholders,
        );
        let (current_message, current_pastes) = remap_colliding_paste_placeholders(
            current_message,
            current_pastes,
            &mut used_placeholders,
        );
        let mut pending_pastes = cancelled_pastes;
        pending_pastes.extend(current_pastes);
        self.restore_composer_state(Self::composer_state_from_user_message(
            merge_user_messages(vec![cancelled_message, current_message]),
            pending_pastes,
        ));
    }

    fn schedule_next_undo_send_tick(&self, now: Instant) {
        let Some(delay) = self
            .input_queue
            .staged_user_messages
            .iter()
            .map(|message| delay_until_next_countdown_change(message.send_at, now))
            .min()
        else {
            return;
        };
        self.frame_requester
            .schedule_frame_in(delay.max(MIN_UNDO_SEND_TICK_DELAY));
    }

    #[cfg(test)]
    pub(crate) fn set_undo_send_delay_for_test(&mut self, delay: Duration) {
        self.undo_send.delay = delay;
    }
}

fn delay_until_next_countdown_change(send_at: Instant, now: Instant) -> Duration {
    let remaining = send_at.saturating_duration_since(now);
    if remaining.is_zero() {
        return Duration::ZERO;
    }
    let displayed_seconds = countdown_seconds(remaining);
    remaining.saturating_sub(Duration::from_secs(displayed_seconds.saturating_sub(1)))
}

pub(super) fn countdown_seconds(remaining: Duration) -> u64 {
    remaining
        .as_secs()
        .saturating_add(u64::from(remaining.subsec_nanos() > 0))
        .max(1)
}

fn thread_composer_state_from_snapshot(snapshot: ComposerDraftSnapshot) -> ThreadComposerState {
    ThreadComposerState {
        text: snapshot.text,
        local_images: snapshot.local_images,
        remote_image_urls: snapshot.remote_image_urls,
        text_elements: snapshot.text_elements,
        mention_bindings: snapshot.mention_bindings,
        pending_pastes: snapshot.pending_pastes,
    }
}

fn user_message_and_pastes_from_composer_state(
    state: ThreadComposerState,
) -> (UserMessage, Vec<(String, String)>) {
    let ThreadComposerState {
        text,
        local_images,
        remote_image_urls,
        text_elements,
        mention_bindings,
        pending_pastes,
    } = state;
    (
        UserMessage {
            text,
            local_images,
            remote_image_urls,
            text_elements,
            mention_bindings,
        },
        pending_pastes,
    )
}

#[cfg(test)]
#[path = "undo_send_tests.rs"]
mod tests;
