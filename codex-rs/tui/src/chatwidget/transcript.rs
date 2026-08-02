//! Transcript and active-cell bookkeeping for `ChatWidget`.

use std::collections::HashMap;

use codex_protocol::models::MessagePhase;

use super::ChatWidget;
use super::HistoryCell;

#[derive(Default)]
pub(super) struct TranscriptState {
    pub(super) active_cell: Option<Box<dyn HistoryCell>>,
    /// Monotonic-ish counter used to invalidate transcript overlay caching.
    pub(super) active_cell_revision: u64,
    /// Raw markdown of the most recently completed agent response.
    pub(super) last_agent_markdown: Option<String>,
    pub(super) last_completed_agent_message: Option<(String, String)>,
    /// Raw markdown of the most recently completed proposed plan.
    pub(super) latest_proposed_plan_markdown: Option<String>,
    /// Whether this turn already produced a copyable response.
    pub(super) saw_copy_source_this_turn: bool,
    /// Item ID of the assistant message whose deltas are currently being streamed.
    /// Used to prevent only that item's completed fallback from duplicating content.
    pub(super) agent_message_delta_item_id: Option<String>,
    /// Message phases learned from item-start notifications before text deltas arrive.
    pub(super) agent_message_phases: HashMap<String, MessagePhase>,
    /// Lossless in-flight commentary source keyed by item id.
    pub(super) commentary_buffers: HashMap<String, String>,
    /// Item ID whose commentary buffer most recently received a non-empty delta.
    pub(super) latest_commentary_delta_item_id: Option<String>,
    /// Latest completed commentary used only for the interrupted-turn fallback.
    pub(super) latest_commentary_markdown: Option<String>,
    /// Latest user-facing reasoning summary shown beneath the live status row.
    ///
    /// Reasoning deltas are newline-gated before they enter the streaming
    /// transcript. Keeping the latest complete summary separately ensures an
    /// unterminated summary remains visible while commands temporarily own the
    /// active-cell slot.
    pub(super) latest_reasoning_status: Option<String>,
    /// Whether this turn produced a real final answer item.
    pub(super) saw_final_answer_this_turn: bool,
    /// Whether the current turn has detail available only in the transcript.
    pub(super) has_hidden_detail_this_turn: bool,
    /// Whether the current turn performed "work" (exec commands, MCP tool calls, patch applications).
    pub(super) had_work_activity: bool,
    /// Whether the current turn emitted a plan update.
    pub(super) saw_plan_update_this_turn: bool,
    /// Whether the current turn emitted a proposed plan item that has not been superseded by a
    /// later steer.
    pub(super) saw_plan_item_this_turn: bool,
    /// Latest `update_plan` checklist task counts for terminal-title rendering.
    pub(super) last_plan_progress: Option<(usize, usize)>,
    /// Incremental buffer for streamed plan content.
    pub(super) plan_delta_buffer: String,
    /// True while a plan item is streaming.
    pub(super) plan_item_active: bool,
}

impl TranscriptState {
    pub(super) fn new(active_cell: Option<Box<dyn HistoryCell>>) -> Self {
        Self {
            active_cell,
            ..Self::default()
        }
    }

    pub(super) fn bump_active_cell_revision(&mut self) {
        // Wrapping avoids overflow; wraparound would require 2^64 bumps and at
        // worst causes a one-time cache-key collision.
        self.active_cell_revision = self.active_cell_revision.wrapping_add(1);
    }

    pub(super) fn record_agent_markdown(&mut self, markdown: String) {
        self.last_agent_markdown = Some(markdown);
        self.saw_copy_source_this_turn = true;
    }

    pub(super) fn reset_copy_history(&mut self) {
        self.last_agent_markdown = None;
        self.saw_copy_source_this_turn = false;
    }

    pub(super) fn reset_turn_flags(&mut self) {
        self.saw_copy_source_this_turn = false;
        self.agent_message_delta_item_id = None;
        self.agent_message_phases.clear();
        self.commentary_buffers.clear();
        self.latest_commentary_delta_item_id = None;
        self.latest_commentary_markdown = None;
        self.latest_reasoning_status = None;
        self.saw_final_answer_this_turn = false;
        self.has_hidden_detail_this_turn = false;
        self.last_completed_agent_message = None;
        self.saw_plan_update_this_turn = false;
        self.saw_plan_item_this_turn = false;
        self.had_work_activity = false;
        self.latest_proposed_plan_markdown = None;
        self.plan_delta_buffer.clear();
        self.plan_item_active = false;
    }
}

impl ChatWidget {
    pub(super) fn mark_hidden_transcript_detail(&mut self) {
        if self.mark_hidden_transcript_detail_without_redraw() {
            self.request_redraw();
        }
    }

    pub(super) fn mark_hidden_transcript_detail_without_redraw(&mut self) -> bool {
        if self.transcript.has_hidden_detail_this_turn {
            return false;
        }
        self.transcript.has_hidden_detail_this_turn = true;
        self.bottom_pane
            .set_has_hidden_transcript_detail_without_redraw(/*has_hidden*/ true);
        true
    }

    pub(super) fn reset_hidden_transcript_detail(&mut self) {
        self.transcript.has_hidden_detail_this_turn = false;
        self.bottom_pane
            .set_has_hidden_transcript_detail(/*has_hidden*/ false);
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn active_cell_revision_wraps() {
        let mut state = TranscriptState {
            active_cell_revision: u64::MAX,
            ..TranscriptState::default()
        };

        state.bump_active_cell_revision();

        assert_eq!(state.active_cell_revision, 0);
    }
}
