//! Streaming transcript updates for `ChatWidget`.
//!
//! This module owns assistant, plan, and reasoning deltas, including stream-tail
//! cells, commit ticks, and interrupt deferral.

use super::*;

impl ChatWidget {
    pub(super) fn restore_reasoning_status_header(&mut self) {
        if self.config.hide_agent_reasoning {
            if self.bottom_pane.is_task_running() {
                self.status_state.terminal_title_status_kind = TerminalTitleStatusKind::Working;
                self.set_status_header(String::from("Working"));
            }
            return;
        }
        if self.reasoning_header.is_none() {
            self.reasoning_header = extract_first_bold(&self.reasoning_buffer);
        }
        if self.config.stream_reasoning_live
            && (self.reasoning_stream_controller.is_some()
                || !self.reasoning_buffer.trim().is_empty())
        {
            self.status_state.terminal_title_status_kind = TerminalTitleStatusKind::Thinking;
            self.set_status_header(String::from("Thinking"));
        } else if let Some(header) = self.reasoning_header.clone() {
            self.status_state.terminal_title_status_kind = TerminalTitleStatusKind::Thinking;
            self.set_status_header(header);
        } else if self.bottom_pane.is_task_running() {
            self.status_state.terminal_title_status_kind = TerminalTitleStatusKind::Working;
            self.set_status_header(String::from("Working"));
        }
    }

    pub(super) fn flush_answer_stream_with_separator(&mut self) {
        let had_stream_controller = self.stream_controller.is_some();
        if let Some(mut controller) = self.stream_controller.take() {
            let scrollback_reflow = if controller.has_live_tail() {
                crate::app_event::ConsolidationScrollbackReflow::Required
            } else {
                crate::app_event::ConsolidationScrollbackReflow::IfResizeReflowRan
            };
            self.clear_active_stream_tail();
            let (cell, source) = controller.finalize();
            // Match newline-committed streaming behavior: once assistant output is ready to be
            // committed into history, hide the inline status row so transcript content replaces it.
            if cell.is_some() {
                self.bottom_pane.hide_status_indicator();
            }
            let deferred_history_cell =
                if scrollback_reflow == crate::app_event::ConsolidationScrollbackReflow::Required {
                    cell
                } else {
                    if let Some(cell) = cell {
                        self.add_boxed_history(cell);
                    }
                    None
                };
            // Consolidate the run of streaming AgentMessageCells into a single AgentMarkdownCell
            // that can re-render from source on resize.
            if let Some(source) = source {
                let source =
                    parse_assistant_markdown(&source, self.config.cwd.as_path()).visible_markdown;
                let inline_visualization_context = self.thread_id.and_then(|thread_id| {
                    crate::inline_visualization::InlineVisualizationContext::from_config(
                        &self.config,
                        thread_id,
                    )
                });
                self.note_stream_consolidation_queued();
                self.app_event_tx.send(AppEvent::ConsolidateAgentMessage {
                    source,
                    cwd: self.config.cwd.to_path_buf(),
                    inline_visualization_context,
                    scrollback_reflow,
                    deferred_history_cell,
                });
            }
        }
        self.adaptive_chunking.reset();
        if had_stream_controller && self.stream_controllers_idle() {
            self.app_event_tx.send(AppEvent::StopCommitAnimation);
        }
        if had_stream_controller {
            self.request_pending_usage_output_insertion_after_stream_shutdown();
        }
    }

    pub(super) fn stream_controllers_idle(&self) -> bool {
        self.stream_controller
            .as_ref()
            .map(|controller| controller.queued_lines() == 0)
            .unwrap_or(true)
            && self
                .plan_stream_controller
                .as_ref()
                .map(|controller| controller.queued_lines() == 0)
                .unwrap_or(true)
            && self
                .reasoning_stream_controller
                .as_ref()
                .map(|controller| controller.queued_lines() == 0)
                .unwrap_or(true)
    }

    /// Restore the status indicator only after commentary completion is pending,
    /// the turn is still running, and all stream queues have drained.
    ///
    /// This gate prevents flicker while normal output is still actively
    /// streaming, but still restores a visible "working" affordance when a
    /// commentary block ends before the turn itself has completed.
    pub(super) fn maybe_restore_status_indicator_after_stream_idle(&mut self) {
        if !self.status_state.pending_status_indicator_restore
            || !self.bottom_pane.is_task_running()
            || !self.stream_controllers_idle()
        {
            return;
        }

        self.bottom_pane.ensure_status_indicator();
        self.set_status(
            self.status_state.current_status.header.clone(),
            self.status_state.current_status.details.clone(),
            StatusDetailsCapitalization::Preserve,
            self.status_state.current_status.details_max_lines,
        );
        self.status_state.pending_status_indicator_restore = false;
    }

    fn finalize_completed_assistant_message_for_item(
        &mut self,
        item_id: &str,
        message: Option<&str>,
    ) {
        // Use the fallback text only when no deltas arrived for this item. Checking
        // stream_controller.is_none() is not reliable because tool lifecycle handlers
        // call flush_answer_stream_with_separator (which takes the controller) before
        // ItemCompleted fires, making the controller appear absent even after deltas
        // were already streamed and committed — which would duplicate the content.
        let completes_different_item = self
            .transcript
            .agent_message_delta_item_id
            .as_deref()
            .is_some_and(|delta_item_id| !delta_item_id.is_empty() && delta_item_id != item_id);
        if completes_different_item {
            self.flush_answer_stream_with_separator();
            self.transcript.agent_message_delta_item_id = None;
        }
        let delta_seen_for_item = self
            .transcript
            .agent_message_delta_item_id
            .as_deref()
            .is_some_and(|delta_item_id| delta_item_id.is_empty() || delta_item_id == item_id);
        if !delta_seen_for_item
            && let Some(message) = message
            && !message.is_empty()
        {
            self.handle_streaming_delta(message.to_string());
        }
        if delta_seen_for_item {
            self.transcript.agent_message_delta_item_id = None;
        }
        self.flush_answer_stream_with_separator();
        self.handle_stream_finished();
        self.request_redraw();
    }

    #[cfg(test)]
    pub(super) fn finalize_completed_assistant_message(&mut self, message: Option<&str>) {
        self.finalize_completed_assistant_message_for_item("", message);
    }

    pub(super) fn on_agent_message_delta_for_item(&mut self, item_id: String, delta: String) {
        let starts_different_item = self
            .transcript
            .agent_message_delta_item_id
            .as_deref()
            .is_some_and(|delta_item_id| !delta_item_id.is_empty() && delta_item_id != item_id);
        if starts_different_item {
            self.flush_answer_stream_with_separator();
        }
        self.transcript.agent_message_delta_item_id = Some(item_id);
        self.handle_streaming_delta(delta);
    }

    #[cfg(test)]
    pub(super) fn on_agent_message_delta(&mut self, delta: String) {
        self.on_agent_message_delta_for_item(String::new(), delta);
    }

    pub(super) fn on_plan_delta(&mut self, delta: String) {
        // Always buffer the delta so on_plan_item_completed has content even if the
        // collaboration mode changed after the turn started.
        if !self.transcript.plan_item_active {
            self.transcript.plan_item_active = true;
            self.transcript.plan_delta_buffer.clear();
        }
        self.transcript.plan_delta_buffer.push_str(&delta);

        if self.active_mode_kind() != ModeKind::Plan {
            // Not in Plan mode — buffer is populated for ItemCompleted but skip live display.
            return;
        }
        if self.plan_stream_controller.is_none() {
            // Before starting a plan stream, flush any active exec cell group.
            self.flush_unified_exec_wait_streak();
            self.flush_active_cell();
            self.plan_stream_controller = Some(PlanStreamController::new(
                self.current_stream_width(/*reserved_cols*/ 4),
                &self.config.cwd,
                self.history_render_mode(),
            ));
        }
        if let Some(controller) = self.plan_stream_controller.as_mut()
            && controller.push(&delta)
        {
            self.app_event_tx.send(AppEvent::StartCommitAnimation);
            self.run_catch_up_commit_tick();
        }
        // Unterminated source is buffered by the controller and cannot change the visible tail.
        if delta.contains('\n') && self.sync_active_stream_tail() {
            self.request_redraw();
        }
    }

    pub(super) fn on_plan_item_completed(&mut self, text: String) {
        let streamed_plan = self.transcript.plan_delta_buffer.trim().to_string();
        let plan_text = if text.trim().is_empty() {
            streamed_plan
        } else {
            text
        };
        if !plan_text.trim().is_empty() {
            self.record_agent_markdown(&plan_text);
            self.transcript.latest_proposed_plan_markdown = Some(plan_text.clone());
        }
        // Plan commit ticks can hide the status row; remember whether we streamed plan output so
        // completion can restore it once stream queues are idle.
        let should_restore_after_stream = self.plan_stream_controller.is_some();
        self.transcript.plan_delta_buffer.clear();
        self.transcript.plan_item_active = false;
        self.transcript.saw_plan_item_this_turn = true;
        let (finalized_streamed_cell, consolidated_plan_source) =
            if let Some(mut controller) = self.plan_stream_controller.take() {
                let had_live_tail = controller.has_live_tail();
                self.clear_active_stream_tail();
                let (cell, source) = controller.finalize();
                if had_live_tail {
                    (None, source)
                } else {
                    (cell, source)
                }
            } else {
                (None, None)
            };
        if let Some(cell) = finalized_streamed_cell {
            self.add_boxed_history(cell);
            // TODO: Replace streamed output with the final plan item text if plan streaming is
            // removed or if we need to reconcile mismatches between streamed and final content.
            if let Some(source) = consolidated_plan_source {
                self.note_stream_consolidation_queued();
                self.app_event_tx
                    .send(AppEvent::ConsolidateProposedPlan(source));
            }
        } else if !plan_text.is_empty() {
            self.add_to_history(history_cell::new_proposed_plan(plan_text, &self.config.cwd));
        } else if let Some(source) = consolidated_plan_source {
            self.note_stream_consolidation_queued();
            self.app_event_tx
                .send(AppEvent::ConsolidateProposedPlan(source));
        }
        if should_restore_after_stream {
            self.status_state.pending_status_indicator_restore = true;
            self.maybe_restore_status_indicator_after_stream_idle();
            self.request_pending_usage_output_insertion_after_stream_shutdown();
        }
    }

    pub(super) fn on_agent_reasoning_delta(&mut self, delta: String) {
        if self.config.hide_agent_reasoning {
            self.clear_reasoning_for_hide();
            if self.bottom_pane.is_task_running() {
                self.status_state.terminal_title_status_kind = TerminalTitleStatusKind::Working;
                self.set_status_header(String::from("Working"));
            }
            return;
        }

        // For reasoning deltas, do not stream to history. Accumulate the
        // current reasoning block and extract the first bold element
        // (between **/**) as the chunk header. Show this header as status.
        self.reasoning_buffer.push_str(&delta);

        // Update the live cell before the exec-wait early return so all deltas
        // reach the streaming display regardless of exec state.
        if self.safety_buffering_is_waiting() {
            return;
        }

        if self.config.stream_reasoning_live {
            self.update_live_reasoning(&delta);
        }

        if self.unified_exec_wait_streak.is_some() {
            // Exec wait takes precedence over reasoning-derived status headers;
            // skip the header update but still trigger a redraw when not live streaming.
            if !self.config.stream_reasoning_live {
                self.request_redraw();
            }
            return;
        }

        if self.config.stream_reasoning_live {
            // In live mode the section header streams into scrollback as it is written,
            // so echoing it in the status spinner would show the same line twice (once
            // committed, once in the spinner). Keep the spinner a generic "Thinking"
            // working indicator; the actual headers/text are visible in scrollback.
            self.status_state.terminal_title_status_kind = TerminalTitleStatusKind::Thinking;
            if self.status_state.current_status.header != "Thinking" {
                self.set_status_header(String::from("Thinking"));
            }
        } else {
            if self.reasoning_header.is_none() {
                self.reasoning_header = extract_first_bold(&self.reasoning_buffer);
            }
            if let Some(header) = self.reasoning_header.clone() {
                let status = &self.status_state.current_status;
                let unchanged = self.status_state.terminal_title_status_kind
                    == TerminalTitleStatusKind::Thinking
                    && status.header == header
                    && status.details.is_none()
                    && status.details_max_lines == STATUS_DETAILS_DEFAULT_MAX_LINES
                    && self
                        .bottom_pane
                        .status_widget()
                        .is_none_or(|status| status.header() == header);
                if !unchanged {
                    self.status_state.terminal_title_status_kind =
                        TerminalTitleStatusKind::Thinking;
                    if !self.set_status_header(header) {
                        self.request_redraw();
                    }
                }
            } else if self.status_state.terminal_title_status_kind
                != TerminalTitleStatusKind::Thinking
            {
                self.status_state.terminal_title_status_kind = TerminalTitleStatusKind::Thinking;
                self.set_status_header(String::from("Thinking"));
            }
        }
    }

    pub(super) fn on_agent_reasoning_final(&mut self) {
        if self.config.hide_agent_reasoning {
            self.clear_reasoning_for_hide();
            return;
        }

        // Commit accumulated content to history and clear the live cell.
        self.commit_pending_reasoning_to_history();
        self.clear_live_reasoning();
        self.reasoning_header = None;
        self.reasoning_summary_parts.clear();
        self.request_redraw();
    }

    pub(super) fn on_reasoning_section_break(&mut self) {
        if self.config.hide_agent_reasoning {
            self.clear_reasoning_for_hide();
            return;
        }

        if self.reasoning_buffer.is_empty() {
            return;
        }

        // Start a new reasoning block for header extraction and accumulate transcript.
        let reasoning_part = std::mem::take(&mut self.reasoning_buffer);
        self.full_reasoning_buffer.push_str(&reasoning_part);
        self.full_reasoning_buffer.push_str("\n\n");
        self.reasoning_summary_parts.push(reasoning_part);
        self.reasoning_header = None;
        if self.config.stream_reasoning_live {
            // hide_agent_reasoning was already checked and returned early above.
            self.update_live_reasoning("\n\n");
        }
    }

    pub(super) fn on_stream_error(&mut self, message: String, additional_details: Option<String>) {
        self.status_state.remember_retry_status_header();
        self.bottom_pane.ensure_status_indicator();
        self.status_state.terminal_title_status_kind = TerminalTitleStatusKind::Thinking;
        self.set_status(
            message,
            additional_details,
            StatusDetailsCapitalization::CapitalizeFirst,
            STATUS_DETAILS_DEFAULT_MAX_LINES,
        );
    }

    /// Handle completion of an `AgentMessage` turn item.
    ///
    /// Commentary completion sets a deferred restore flag so the status row
    /// returns once stream queues are idle. Final-answer completion (or absent
    /// phase for legacy models) clears the flag to preserve historical behavior.
    pub(super) fn on_agent_message_item_completed(
        &mut self,
        item: AgentMessageItem,
        from_replay: bool,
    ) {
        let mut message = String::new();
        for content in &item.content {
            match content {
                AgentMessageContent::Text { text } => message.push_str(text),
            }
        }
        let parsed = parse_assistant_markdown(&message, self.config.cwd.as_path());
        self.finalize_completed_assistant_message_for_item(
            &item.id,
            (!parsed.visible_markdown.is_empty()).then_some(parsed.visible_markdown.as_str()),
        );
        if matches!(item.phase, Some(MessagePhase::FinalAnswer) | None)
            && !parsed.visible_markdown.is_empty()
        {
            self.record_agent_markdown(&parsed.visible_markdown);
        }
        if !from_replay
            && let Some(cwd) = parsed.last_created_branch_cwd()
            && let Some(thread_id) = self.thread_id
            && let Some(runner) = self.workspace_command_runner.clone()
        {
            let cwd = PathBuf::from(cwd);
            let tx = self.app_event_tx.clone();
            tokio::spawn(async move {
                if let Some(branch) =
                    crate::branch_summary::current_branch_name(runner.as_ref(), &cwd).await
                {
                    tx.send(AppEvent::SyncThreadGitBranch { thread_id, branch });
                }
            });
        }
        self.status_state.pending_status_indicator_restore = match item.phase {
            // Models that don't support preambles only output AgentMessageItems on turn completion.
            Some(MessagePhase::FinalAnswer) | None => !self.input_queue.pending_steers.is_empty(),
            Some(MessagePhase::Commentary) => true,
        };
        self.maybe_restore_status_indicator_after_stream_idle();
    }

    /// Periodic tick for stream commits. In smooth mode this preserves one-line pacing, while
    /// catch-up mode drains larger batches to reduce queue lag.
    pub(crate) fn on_commit_tick(&mut self) {
        self.run_commit_tick();
        self.request_redraw();
    }

    /// Runs a regular periodic commit tick.
    pub(super) fn run_commit_tick(&mut self) {
        self.run_commit_tick_with_scope(CommitTickScope::AnyMode);
    }

    /// Runs an opportunistic commit tick only if catch-up mode is active.
    pub(super) fn run_catch_up_commit_tick(&mut self) {
        self.run_commit_tick_with_scope(CommitTickScope::CatchUpOnly);
    }

    /// Runs a commit tick for the current stream queue snapshot.
    ///
    /// `scope` controls whether this call may commit in smooth mode or only when catch-up
    /// is currently active. While lines are actively streaming we hide the status row to avoid
    /// duplicate "in progress" affordances. Restoration is gated separately so we only re-show
    /// the row after commentary completion once stream queues are idle.
    pub(super) fn run_commit_tick_with_scope(&mut self, scope: CommitTickScope) {
        let now = Instant::now();
        let outcome = run_commit_tick(
            &mut self.adaptive_chunking,
            self.stream_controller.as_mut(),
            self.plan_stream_controller.as_mut(),
            self.reasoning_stream_controller.as_mut(),
            scope,
            now,
        );
        // While the agent message or a plan is streaming, the streamed text is the
        // activity indicator, so the "working" spinner is hidden to avoid a redundant
        // affordance. Live reasoning is different: it is still "thinking", so keep the
        // spinner (header + elapsed + "esc to interrupt") visible the user has a
        // working/interrupt affordance instead of a bare composer.
        let keep_thinking_indicator = self.reasoning_stream_controller.is_some()
            && self.stream_controller.is_none()
            && self.plan_stream_controller.is_none()
            && self.bottom_pane.is_task_running();
        for cell in outcome.cells {
            if keep_thinking_indicator {
                self.bottom_pane.ensure_status_indicator();
            } else {
                self.bottom_pane.hide_status_indicator();
            }
            self.add_boxed_history(cell);
        }
        if scope == CommitTickScope::AnyMode || outcome.has_controller {
            self.sync_active_stream_tail();
        }

        if outcome.has_controller && outcome.all_idle {
            self.maybe_restore_status_indicator_after_stream_idle();
            self.app_event_tx.send(AppEvent::StopCommitAnimation);
        }

        if self.turn_lifecycle.agent_turn_running {
            self.refresh_runtime_metrics();
        }
    }

    pub(super) fn flush_interrupt_queue(&mut self) {
        let mut mgr = std::mem::take(&mut self.interrupts);
        mgr.flush_all(self);
        self.interrupts = mgr;
    }

    #[inline]
    pub(super) fn defer_or_handle(
        &mut self,
        push: impl FnOnce(&mut InterruptManager),
        handle: impl FnOnce(&mut Self),
    ) {
        // Preserve deterministic FIFO across queued interrupts: once anything
        // is queued due to an active write cycle, continue queueing until the
        // queue is flushed to avoid reordering (e.g., ExecEnd before ExecBegin).
        if self.stream_controller.is_some() || !self.interrupts.is_empty() {
            push(&mut self.interrupts);
        } else {
            handle(self);
        }
    }

    pub(super) fn handle_stream_finished(&mut self) {
        if self.task_complete_pending {
            self.bottom_pane.hide_status_indicator();
            self.task_complete_pending = false;
        }
        // A completed stream indicates non-exec content was just inserted.
        self.flush_interrupt_queue();
    }

    #[inline]
    pub(super) fn handle_streaming_delta(&mut self, delta: String) {
        if !delta.is_empty() {
            self.mark_safety_buffering_agent_message_started();
        }
        if self.stream_controller.is_none() {
            // Before starting an agent stream, flush any active exec cell group.
            self.flush_unified_exec_wait_streak();
            self.flush_active_cell();
            // If the previous turn inserted non-stream history (exec output, patch status, MCP
            // calls), render a separator before starting the next streamed assistant message.
            if self.transcript.needs_final_message_separator && self.transcript.had_work_activity {
                self.add_to_history(history_cell::FinalMessageSeparator::new(
                    /*elapsed_seconds*/ None, /*runtime_metrics*/ None,
                ));
                self.transcript.needs_final_message_separator = false;
            } else if self.transcript.needs_final_message_separator {
                // Reset the flag even if we don't show separator (no work was done)
                self.transcript.needs_final_message_separator = false;
            }
            let inline_visualization_context = self.thread_id.and_then(|thread_id| {
                crate::inline_visualization::InlineVisualizationContext::from_config(
                    &self.config,
                    thread_id,
                )
            });
            self.stream_controller = Some(StreamController::new_with_inline_visualizations(
                self.current_stream_width(/*reserved_cols*/ 2),
                &self.config.cwd,
                self.history_render_mode(),
                inline_visualization_context,
            ));
        }
        if let Some(controller) = self.stream_controller.as_mut()
            && controller.push(&delta)
        {
            self.app_event_tx.send(AppEvent::StartCommitAnimation);
            self.run_catch_up_commit_tick();
        }
        // Unterminated source is buffered by the controller and cannot change the visible tail.
        if delta.contains('\n') && self.sync_active_stream_tail() {
            self.request_redraw();
        }
    }

    pub(super) fn schedule_live_reasoning_redraw(&mut self) {
        self.frame_requester.schedule_frame();
    }

    /// Stream the latest reasoning content live.
    ///
    /// Routes reasoning through a [`ReasoningStreamController`] so completed lines
    /// commit to scrollback as the model thinks and only the in-progress tail
    /// stays in the `active_cell` — the same two-region model as agent messages
    /// and plans. This replaces the old single growing `ReasoningSummaryCell`,
    /// which clipped/scrolled long reasoning out of view inside the bounded live
    /// region.
    ///
    /// `appended` is the new delta; on first creation the controller is seeded
    /// with everything accumulated so far so reasoning that arrived before the
    /// live stream could start is not lost.
    pub(super) fn update_live_reasoning(&mut self, appended: &str) {
        if self.config.hide_agent_reasoning {
            return;
        }

        // While a command is running (or a unified-exec terminal is being waited
        // on), its exec cell must own the active slot so the streamed output deltas
        // land in it (`on_exec_command_output_delta` only appends to an `ExecCell`).
        // If live reasoning streamed now it would steal the slot and that output
        // would be dropped, leaving the tool output trimmed. Keep reasoning
        // buffered; it resumes once the command's cell is flushed, via
        // `flush_active_cell`'s recreate path. The exec `begin` already flushed any
        // in-progress reasoning, so the buffers are not lost here.
        if !self.running_commands.is_empty() || self.unified_exec_wait_streak.is_some() {
            return;
        }

        // Don't displace a non-reasoning active cell (e.g., a tool spinner or exec
        // cell). Suppress the live tail rather than prematurely flushing it; the
        // committed lines still flow to scrollback below the other cell.
        if self.reasoning_stream_controller.is_none()
            && self.transcript.active_cell.is_some()
            && !self.active_cell_is_stream_tail()
        {
            return;
        }

        if self.full_reasoning_buffer.is_empty() && self.reasoning_buffer.trim().is_empty() {
            return;
        }

        let just_created = self.reasoning_stream_controller.is_none();
        if just_created {
            let width = self.current_stream_width(/*reserved_cols*/ 2);
            let render_mode = self.history_render_mode();
            self.reasoning_stream_controller = Some(ReasoningStreamController::new(
                width,
                &self.config.cwd,
                render_mode,
            ));
        }
        // On first creation push the full accumulated reasoning (which already
        // includes `appended`); afterwards push only the new delta.
        let to_push = if just_created {
            format!("{}{}", self.full_reasoning_buffer, self.reasoning_buffer)
        } else {
            appended.to_string()
        };
        let enqueued = self
            .reasoning_stream_controller
            .as_mut()
            .map(|controller| controller.push(&to_push))
            .unwrap_or(false);
        self.live_reasoning_active = true;
        if enqueued {
            self.app_event_tx.send(AppEvent::StartCommitAnimation);
            self.run_catch_up_commit_tick();
        }
        self.sync_active_stream_tail();
        self.schedule_live_reasoning_redraw();
    }

    /// Finalize the live reasoning stream, committing any not-yet-emitted tail to
    /// scrollback. Clears the live buffers/flag and the tail cell *before*
    /// inserting history so the re-entrant `add_boxed_history -> flush_active_cell`
    /// path cannot double-commit or recreate the stream.
    pub(super) fn finalize_reasoning_stream(&mut self) {
        let Some(mut controller) = self.reasoning_stream_controller.take() else {
            return;
        };
        self.live_reasoning_active = false;
        // Capture the full reasoning source before clearing the buffers so the
        // streamed (fixed-width) cells can be consolidated into a single
        // source-backed cell that re-wraps on resize.
        let source = format!("{}{}", self.full_reasoning_buffer, self.reasoning_buffer);
        self.reasoning_buffer.clear();
        self.full_reasoning_buffer.clear();
        self.clear_active_stream_tail();
        if let Some(cell) = controller.finalize() {
            self.add_boxed_history(cell);
        }
        // Replace the just-committed run of streamed reasoning cells with a
        // reflow-able cell. Sent after the tail's InsertHistoryCell so the handler
        // sees the complete run in transcript order. No-op when resize reflow is
        // disabled (handled app-side).
        if !source.trim().is_empty() {
            self.note_stream_consolidation_queued();
            self.app_event_tx
                .send(AppEvent::ConsolidateReasoning(source));
        }
        if self.stream_controllers_idle() {
            self.app_event_tx.send(AppEvent::StopCommitAnimation);
        }
    }

    pub(super) fn clear_live_reasoning(&mut self) {
        // Discard any live reasoning stream without committing: completed lines are
        // already in scrollback, and the in-progress tail is intentionally dropped
        // (callers that need to keep it call commit_pending_reasoning_to_history
        // first). clear_active_stream_tail removes the ReasoningStreamCell tail.
        if self.reasoning_stream_controller.take().is_some() {
            self.clear_active_stream_tail();
            if self.stream_controllers_idle() {
                self.app_event_tx.send(AppEvent::StopCommitAnimation);
            }
        }
        if self.live_reasoning_active
            && self
                .transcript
                .active_cell
                .as_ref()
                .is_some_and(|c| c.as_any().is::<ReasoningSummaryCell>())
        {
            // Legacy non-controller live cell: null the slot.
            self.transcript.active_cell = None;
            self.bump_active_cell_revision();
        }
        self.live_reasoning_active = false;
    }

    /// Discard all reasoning state when `hide_agent_reasoning` is active.
    ///
    /// Centralises the clear+clear+clear_live+redraw pattern that was previously
    /// copy-pasted across `on_agent_reasoning_delta`, `on_agent_reasoning_final`,
    /// and `on_reasoning_section_break`.
    fn clear_reasoning_for_hide(&mut self) {
        self.reasoning_buffer.clear();
        self.full_reasoning_buffer.clear();
        self.reasoning_summary_parts.clear();
        self.reasoning_header = None;
        self.clear_live_reasoning();
        self.request_redraw();
    }

    /// Commit any accumulated reasoning content to history and clear the buffers.
    ///
    /// This is called by `on_agent_reasoning_final` on the normal path and by
    /// turn-teardown paths (`finalize_turn`, `on_task_complete`) as a safety net
    /// when the turn ends before a final reasoning event is delivered.
    pub(super) fn commit_pending_reasoning_to_history(&mut self) {
        if self.config.hide_agent_reasoning {
            self.reasoning_buffer.clear();
            self.full_reasoning_buffer.clear();
            self.reasoning_summary_parts.clear();
            self.reasoning_header = None;
            return;
        }
        // Live streaming path: the controller already committed completed lines to
        // scrollback as they streamed, so just finalize its tail (which also clears
        // the buffers). Falls through to the buffer-commit path below only when no
        // live controller is active (non-live reasoning, or content that arrived
        // before a live stream could start).
        if self.reasoning_stream_controller.is_some() {
            self.finalize_reasoning_stream();
            self.reasoning_buffer.clear();
            self.full_reasoning_buffer.clear();
            self.reasoning_summary_parts.clear();
            self.reasoning_header = None;
            return;
        }
        if self.reasoning_buffer.is_empty() && self.reasoning_summary_parts.is_empty() {
            return;
        }
        if !self.reasoning_buffer.is_empty() {
            self.reasoning_summary_parts
                .push(std::mem::take(&mut self.reasoning_buffer));
        }
        self.full_reasoning_buffer.clear();
        self.reasoning_header = None;
        // Take the buffer before calling add_boxed_history so any re-entrant call to
        // commit_pending_reasoning_to_history hits the empty-buffer early-exit above
        // instead of recursing infinitely through flush_active_cell → add_boxed_history.
        let reasoning_parts = std::mem::take(&mut self.reasoning_summary_parts);
        if !reasoning_parts.is_empty() {
            let cell = history_cell::new_reasoning_summary_block(reasoning_parts, &self.config.cwd);
            self.add_boxed_history(cell);
        }
    }

    pub(super) fn active_cell_is_stream_tail(&self) -> bool {
        self.transcript.active_cell.as_ref().is_some_and(|cell| {
            cell.as_any().is::<history_cell::StreamingAgentTailCell>()
                || cell.as_any().is::<history_cell::StreamingPlanTailCell>()
                || cell.as_any().is::<history_cell::ReasoningStreamCell>()
        })
    }

    /// True while any streaming controller is mid-stream, even when its live tail
    /// cell is momentarily empty between deltas.
    ///
    /// A stream's live tail is materialized only while there is uncommitted content;
    /// it goes empty whenever every rendered line has been queued for commit. Callers
    /// that must distinguish "mid-stream" from "no stream" should use this rather than
    /// [`active_cell_is_stream_tail`], which only reports a currently-visible tail cell.
    pub(super) fn has_active_stream_controller(&self) -> bool {
        self.stream_controller.is_some()
            || self.plan_stream_controller.is_some()
            || self.reasoning_stream_controller.is_some()
    }

    pub(super) fn sync_active_stream_tail(&mut self) -> bool {
        if let Some(controller) = self.stream_controller.as_ref() {
            let tail_lines = controller.current_tail_lines();
            if tail_lines.is_empty() {
                return self.clear_active_stream_tail();
            }

            self.bottom_pane.hide_status_indicator();
            let cell = history_cell::StreamingAgentTailCell::new(
                tail_lines,
                controller.tail_starts_stream(),
            );
            if self
                .transcript
                .active_cell
                .as_ref()
                .and_then(|active| {
                    active
                        .as_any()
                        .downcast_ref::<history_cell::StreamingAgentTailCell>()
                })
                .is_some_and(|active| active == &cell)
            {
                return false;
            }
            self.transcript.active_cell = Some(Box::new(cell));
            self.bump_active_cell_revision();
            return true;
        }

        if let Some(controller) = self.plan_stream_controller.as_ref() {
            let tail_lines = controller.current_tail_display_lines();
            if tail_lines.is_empty() {
                return self.clear_active_stream_tail();
            }

            self.bottom_pane.hide_status_indicator();
            let cell = history_cell::StreamingPlanTailCell::new(
                tail_lines,
                !controller.tail_starts_stream(),
            );
            if self
                .transcript
                .active_cell
                .as_ref()
                .and_then(|active| {
                    active
                        .as_any()
                        .downcast_ref::<history_cell::StreamingPlanTailCell>()
                })
                .is_some_and(|active| active == &cell)
            {
                return false;
            }
            self.transcript.active_cell = Some(Box::new(cell));
            self.bump_active_cell_revision();
            return true;
        }

        if let Some(controller) = self.reasoning_stream_controller.as_ref() {
            let tail_lines = controller.current_tail_display_lines();
            if tail_lines.is_empty() {
                return self.clear_active_stream_tail();
            }

            // Don't displace a non-stream active cell (exec spinner, tool, MCP,
            // hook). The committed reasoning lines still flow into scrollback; only
            // the transient live tail preview is suppressed while another cell owns
            // the active slot.
            let can_show =
                self.transcript.active_cell.is_none() || self.active_cell_is_stream_tail();
            if !can_show {
                return false;
            }

            // Reasoning is still "thinking": keep the working spinner (header +
            // elapsed + interrupt hint) so the user retains feedback and an interrupt
            // affordance while the live tail preview streams above it. Agent-message
            // and plan tails (handled above) hide it because their streamed text is
            // itself the activity indicator.
            if self.bottom_pane.is_task_running() {
                self.bottom_pane.ensure_status_indicator();
            } else {
                self.bottom_pane.hide_status_indicator();
            }
            let tail_is_continuation = controller.tail_is_continuation();
            self.transcript.active_cell = Some(Box::new(history_cell::ReasoningStreamCell::new(
                tail_lines,
                tail_is_continuation,
            )));
            self.bump_active_cell_revision();
            return true;
        }

        self.clear_active_stream_tail()
    }

    pub(super) fn clear_active_stream_tail(&mut self) -> bool {
        if self.active_cell_is_stream_tail() {
            self.transcript.active_cell = None;
            self.bump_active_cell_revision();
            return true;
        }
        false
    }
}
