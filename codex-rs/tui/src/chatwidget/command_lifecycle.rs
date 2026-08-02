//! Command execution lifecycle handlers for `ChatWidget`.
//!
//! This module owns command start/output/completion rendering, including active
//! exec-cell grouping and unified exec wait state.

use super::*;

impl ChatWidget {
    pub(super) fn flush_unified_exec_wait_streak(&mut self) {
        let Some(wait) = self.unified_exec_wait_streak.take() else {
            return;
        };
        let cell = history_cell::new_unified_exec_interaction(wait.command_display, String::new());
        self.app_event_tx
            .send(AppEvent::InsertHistoryCell(Box::new(cell)));
        self.restore_reasoning_status_header();
    }

    pub(super) fn on_command_execution_started(&mut self, item: ThreadItem) {
        let ThreadItem::CommandExecution {
            id,
            command,
            process_id,
            source,
            command_actions,
            ..
        } = &item
        else {
            return;
        };
        let (_command, parsed_cmd) = command_execution_command_and_parsed(command, command_actions);
        self.flush_answer_stream_with_separator();
        if is_unified_exec_source(*source) {
            if *source == ExecCommandSource::UnifiedExecStartup {
                self.track_unified_exec_process_begin(id, process_id.as_deref(), command);
            }
            if !self.bottom_pane.is_task_running() {
                return;
            }
            // Unified exec may be parsed as Unknown; keep the working indicator visible regardless.
            self.bottom_pane.ensure_status_indicator();
            if !is_standard_tool_call(&parsed_cmd) {
                return;
            }
        }
        self.defer_or_handle(
            item,
            InterruptManager::push_item_started,
            Self::handle_command_execution_started_now,
        );
    }

    pub(super) fn on_exec_command_output_delta(&mut self, call_id: &str, delta: &str) {
        self.track_unified_exec_output_chunk(call_id, delta.as_bytes());
        if !self.bottom_pane.is_task_running() {
            return;
        }

        if self.append_exec_output_to_active_cell(call_id, delta) {
            return;
        }

        // The active cell is not this call's exec cell, so a plain append would
        // silently drop the output and the rendered tool output ends up trimmed.
        // This shows up with `stream_reasoning_live` because the live reasoning
        // tail (a `ReasoningStreamCell`) can hold the active slot, and it can also
        // happen when this call's `begin` is still queued in the interrupt manager.
        // Recover so streamed output is never lost:
        //   1. Drain any deferred begin so its exec cell is materialized.
        if !self.interrupts.is_empty() {
            self.flush_interrupt_queue();
            if self.append_exec_output_to_active_cell(call_id, delta) {
                return;
            }
        }
        //   2. Otherwise re-establish the running command's exec cell as active —
        //      committing whatever currently owns the slot (e.g. live reasoning)
        //      to scrollback first so it is preserved, not discarded. Only do this
        //      when the active cell is NOT itself an exec cell: a different active
        //      exec cell means parallel/grouped calls are streaming, and flushing it
        //      mid-run would trim *its* output. In that (rare) case leave the output
        //      to the completion event's aggregated payload rather than risk worse.
        let active_is_exec = self
            .transcript
            .active_cell
            .as_ref()
            .is_some_and(|cell| cell.as_any().is::<ExecCell>());
        if !active_is_exec && let Some(running) = self.running_commands.get(call_id).cloned() {
            self.flush_active_cell();
            let mut cell = new_active_exec_command(
                call_id.to_string(),
                running.command,
                running.parsed_cmd,
                running.presentation,
                running.source,
                /*interaction_input*/ None,
                self.config.animations,
            );
            let appended = cell.append_output(call_id, delta);
            self.transcript.active_cell = Some(Box::new(cell));
            self.bump_active_cell_revision();
            if appended {
                self.request_redraw();
            }
        }
    }

    /// Appends `delta` to the active exec cell when it tracks `call_id`.
    ///
    /// Returns `false` (without mutating) when the active cell is absent or is not
    /// the exec cell for this call, so callers can fall back to recovery instead of
    /// dropping the output.
    fn append_exec_output_to_active_cell(&mut self, call_id: &str, delta: &str) -> bool {
        let Some(cell) = self
            .transcript
            .active_cell
            .as_mut()
            .and_then(|c| c.as_any_mut().downcast_mut::<ExecCell>())
        else {
            return false;
        };

        if cell.append_output(call_id, delta) {
            self.bump_active_cell_revision();
            self.request_redraw();
            true
        } else {
            false
        }
    }

    pub(super) fn on_terminal_interaction(&mut self, process_id: String, stdin: String) {
        if !self.bottom_pane.is_task_running() {
            return;
        }
        let command_display = self
            .unified_exec_processes
            .iter()
            .find(|process| process.key == process_id)
            .map(|process| process.command_display.clone());
        if stdin.is_empty() && command_display.is_none() {
            return;
        }

        self.flush_answer_stream_with_separator();
        if stdin.is_empty() {
            // Empty stdin means we are polling for background output.
            // Surface this in the status indicator (single "waiting" surface) instead of
            // the transcript. Keep the header short so the interrupt hint remains visible.
            self.bottom_pane.ensure_status_indicator();
            self.bottom_pane
                .set_interrupt_hint_visible(/*visible*/ true);
            self.status_state.terminal_title_status_kind =
                TerminalTitleStatusKind::WaitingForBackgroundTerminal;
            self.set_status(
                "Waiting for background terminal".to_string(),
                command_display.clone(),
                StatusDetailsCapitalization::Preserve,
                /*details_max_lines*/ 1,
            );
            match &mut self.unified_exec_wait_streak {
                Some(wait) if wait.process_id == process_id => {
                    wait.update_command_display(command_display);
                }
                Some(_) => {
                    self.flush_unified_exec_wait_streak();
                    self.unified_exec_wait_streak =
                        Some(UnifiedExecWaitStreak::new(process_id, command_display));
                }
                None => {
                    self.unified_exec_wait_streak =
                        Some(UnifiedExecWaitStreak::new(process_id, command_display));
                }
            }
            self.request_redraw();
        } else {
            if self
                .unified_exec_wait_streak
                .as_ref()
                .is_some_and(|wait| wait.process_id == process_id)
            {
                self.flush_unified_exec_wait_streak();
            }
            self.add_to_history(history_cell::new_unified_exec_interaction(
                command_display,
                stdin,
            ));
        }
    }

    pub(super) fn on_command_execution_completed(&mut self, item: ThreadItem) {
        let ThreadItem::CommandExecution {
            id,
            process_id,
            source,
            ..
        } = &item
        else {
            return;
        };
        if is_unified_exec_source(*source) {
            if let Some(process_id) = process_id.as_deref()
                && self
                    .unified_exec_wait_streak
                    .as_ref()
                    .is_some_and(|wait| wait.process_id == process_id)
            {
                self.flush_unified_exec_wait_streak();
            }
            self.track_unified_exec_process_end(id, process_id.as_deref());
            if !self.bottom_pane.is_task_running() {
                return;
            }
        }
        self.defer_or_handle(
            item,
            InterruptManager::push_item_completed,
            Self::handle_command_execution_completed_now,
        );
    }

    pub(super) fn track_unified_exec_process_begin(
        &mut self,
        call_id: &str,
        process_id: Option<&str>,
        command: &str,
    ) {
        let key = process_id.unwrap_or(call_id).to_string();
        let command = split_command_string(command);
        let command_display = strip_bash_lc_and_escape(&command);
        if let Some(existing) = self
            .unified_exec_processes
            .iter_mut()
            .find(|process| process.key == key)
        {
            existing.call_id = call_id.to_string();
            existing.command_display = command_display;
            existing.recent_chunks.clear();
        } else {
            self.unified_exec_processes.push(UnifiedExecProcessSummary {
                key,
                call_id: call_id.to_string(),
                command_display,
                recent_chunks: Vec::new(),
            });
        }
        self.sync_unified_exec_footer();
    }

    pub(super) fn track_unified_exec_process_end(
        &mut self,
        call_id: &str,
        process_id: Option<&str>,
    ) {
        let key = process_id.unwrap_or(call_id);
        let before = self.unified_exec_processes.len();
        self.unified_exec_processes
            .retain(|process| process.key != key);
        if self.unified_exec_processes.len() != before {
            self.sync_unified_exec_footer();
        }
    }

    pub(super) fn sync_unified_exec_footer(&mut self) {
        let processes = self
            .unified_exec_processes
            .iter()
            .map(|process| process.command_display.clone())
            .collect();
        self.bottom_pane.set_unified_exec_processes(processes);
    }

    /// Record recent stdout/stderr lines for the unified exec footer.
    pub(super) fn track_unified_exec_output_chunk(&mut self, call_id: &str, chunk: &[u8]) {
        let Some(process) = self
            .unified_exec_processes
            .iter_mut()
            .find(|process| process.call_id == call_id)
        else {
            return;
        };

        let text = String::from_utf8_lossy(chunk);
        for line in text
            .lines()
            .map(str::trim_end)
            .filter(|line| !line.is_empty())
        {
            process.recent_chunks.push(line.to_string());
        }

        const MAX_RECENT_CHUNKS: usize = 3;
        if process.recent_chunks.len() > MAX_RECENT_CHUNKS {
            let drop_count = process.recent_chunks.len() - MAX_RECENT_CHUNKS;
            process.recent_chunks.drain(0..drop_count);
        }
    }

    pub(crate) fn handle_command_execution_started_now(&mut self, item: ThreadItem) {
        let ThreadItem::CommandExecution {
            id,
            command,
            source,
            command_actions,
            ..
        } = item
        else {
            return;
        };
        let (command, parsed_cmd) =
            command_execution_command_and_parsed(&command, &command_actions);
        // Ensure the status indicator is visible while the command runs.
        self.bottom_pane.ensure_status_indicator();
        let presented = command_display::presentation_parsed_commands(&command, parsed_cmd);
        let presentation = if source == ExecCommandSource::UserShell {
            CommandPresentation::Command
        } else {
            presented.presentation
        };
        let parsed_cmd = self.annotate_skill_reads_in_parsed_cmd(presented.parsed);
        if source != ExecCommandSource::UserShell
            && (presentation != CommandPresentation::Command
                || strip_bash_lc_and_escape(&command).chars().count() > 140)
        {
            self.mark_hidden_transcript_detail();
        }
        self.running_commands.insert(
            id.clone(),
            RunningCommand {
                command: command.clone(),
                parsed_cmd: parsed_cmd.clone(),
                presentation: presentation.clone(),
                source,
            },
        );
        let is_wait_interaction = matches!(source, ExecCommandSource::UnifiedExecInteraction);
        let command_display = command.join(" ");
        let should_suppress_unified_wait = is_wait_interaction
            && self
                .last_unified_wait
                .as_ref()
                .is_some_and(|wait| wait.is_duplicate(&command_display));
        if is_wait_interaction {
            self.last_unified_wait = Some(UnifiedExecWaitState::new(command_display));
        } else {
            self.last_unified_wait = None;
        }
        if should_suppress_unified_wait {
            self.suppressed_exec_calls.insert(id);
            return;
        }
        if let Some(cell) = self
            .transcript
            .active_cell
            .as_mut()
            .and_then(|c| c.as_any_mut().downcast_mut::<ExecCell>())
            && cell.add_call(
                id.clone(),
                command.clone(),
                parsed_cmd.clone(),
                presentation.clone(),
                source,
                /*interaction_input*/ None,
            )
        {
            self.bump_active_cell_revision();
        } else {
            self.flush_active_cell();

            self.transcript.active_cell = Some(Box::new(new_active_exec_command(
                id,
                command,
                parsed_cmd,
                presentation,
                source,
                /*interaction_input*/ None,
                self.config.animations,
            )));
            self.bump_active_cell_revision();
        }

        self.request_redraw();
    }

    /// Finalizes an exec call while preserving the active exec cell grouping contract.
    ///
    /// Exec begin/end events usually pair through `running_commands`, but unified exec can emit an
    /// end event for a call that was never materialized as the current active `ExecCell` (for
    /// example, when another exploring group is still active). In that case we render the end as a
    /// standalone history entry instead of replacing or flushing the unrelated active exploring
    /// cell. If this method treated every unknown end as "complete the active cell", the UI could
    /// merge unrelated commands and hide still-running exploring work.
    pub(crate) fn handle_command_execution_completed_now(&mut self, item: ThreadItem) {
        enum ExecEndTarget {
            // Normal case: the active exec cell already tracks this call id.
            ActiveTracked,
            // We have an active exec group, but it does not contain this call id. Render the end
            // as a standalone finalized history cell so the active group remains intact.
            OrphanHistoryWhileActiveExec,
            // Resume history contains completed items rather than begin/end pairs. Consecutive
            // clean exploration items should rebuild the same compact group as the live flow.
            AppendCompletedExploration,
            // No active exec cell can safely own this end; build a new cell from the end payload.
            NewCell,
        }

        let ThreadItem::CommandExecution {
            id,
            command,
            process_id: _,
            source,
            command_actions,
            aggregated_output,
            exit_code,
            duration_ms,
            ..
        } = item
        else {
            return;
        };
        let event_command = split_command_string(&command);
        let event_parsed = command_actions
            .into_iter()
            .map(codex_app_server_protocol::CommandAction::into_core)
            .collect();
        let duration = Duration::from_millis(duration_ms.unwrap_or_default().max(0) as u64);
        let exit_code = exit_code.unwrap_or_default();
        let aggregated_output = aggregated_output.unwrap_or_default();

        let running = self.running_commands.remove(&id);
        if self.suppressed_exec_calls.remove(&id) {
            return;
        }
        let (command, parsed, presentation, source) = match running {
            Some(rc) => (rc.command, rc.parsed_cmd, rc.presentation, rc.source),
            None => {
                let presented =
                    command_display::presentation_parsed_commands(&event_command, event_parsed);
                (
                    event_command,
                    presented.parsed,
                    presented.presentation,
                    source,
                )
            }
        };
        let presentation = if source == ExecCommandSource::UserShell {
            CommandPresentation::Command
        } else {
            presentation
        };
        let parsed = self.annotate_skill_reads_in_parsed_cmd(parsed);
        let is_unified_exec_interaction =
            matches!(source, ExecCommandSource::UnifiedExecInteraction);
        let is_user_shell = source == ExecCommandSource::UserShell;
        if !is_user_shell
            && (presentation != CommandPresentation::Command
                || aggregated_output.lines().count() > crate::exec_cell::TOOL_CALL_MAX_LINES
                || aggregated_output
                    .lines()
                    .any(|line| line.chars().count() > 140)
                || strip_bash_lc_and_escape(&command).chars().count() > 140)
        {
            self.mark_hidden_transcript_detail();
        }

        // Unified exec interaction rows intentionally hide command output text in the exec cell and
        // instead render the interaction-specific content elsewhere in the UI.
        let output = if is_unified_exec_interaction {
            CommandOutput::new(exit_code, String::new())
        } else {
            CommandOutput::new(exit_code, aggregated_output)
        };
        let incoming_is_clean_exploration = presentation == CommandPresentation::Exploration
            && command_display::is_exploration(&parsed)
            && output.exit_code == 0
            && !output.has_diagnostic_signal();
        let end_target = match self.transcript.active_cell.as_ref() {
            Some(cell) => match cell.as_any().downcast_ref::<ExecCell>() {
                Some(exec_cell) if exec_cell.iter_calls().any(|call| call.call_id == id) => {
                    ExecEndTarget::ActiveTracked
                }
                Some(exec_cell) if exec_cell.is_active() => {
                    ExecEndTarget::OrphanHistoryWhileActiveExec
                }
                Some(exec_cell)
                    if exec_cell.is_exploring_cell() && incoming_is_clean_exploration =>
                {
                    ExecEndTarget::AppendCompletedExploration
                }
                Some(_) | None => ExecEndTarget::NewCell,
            },
            None => ExecEndTarget::NewCell,
        };
        // Completion means actual command work was observed, regardless of which history target
        // owns the finalized call below.
        self.transcript.had_work_activity = true;

        match end_target {
            ExecEndTarget::ActiveTracked => {
                enum CompletionAction {
                    KeepGrouped,
                    RefreshActive,
                    FlushSingle,
                    Extract {
                        call: crate::exec_cell::ExecCall,
                        remaining_is_empty: bool,
                        flush_completed_remainder: bool,
                    },
                    Orphan {
                        output: CommandOutput,
                    },
                }

                let action = match self
                    .transcript
                    .active_cell
                    .as_mut()
                    .and_then(|cell| cell.as_any_mut().downcast_mut::<ExecCell>())
                {
                    Some(cell) => {
                        let completed = cell.complete_call(&id, output, duration);
                        debug_assert!(completed, "active exec cell should contain {id}");
                        let keep_grouped =
                            cell.call_is_exploring(&id) && cell.call_is_clean_success(&id);
                        if keep_grouped {
                            CompletionAction::KeepGrouped
                        } else if cell.iter_calls().count() == 1 && !cell.call_is_exploring(&id) {
                            CompletionAction::FlushSingle
                        } else if let Some(call) = cell.take_call(&id) {
                            CompletionAction::Extract {
                                call,
                                remaining_is_empty: cell.is_empty(),
                                flush_completed_remainder: !cell.is_empty() && !cell.is_active(),
                            }
                        } else {
                            debug_assert!(
                                false,
                                "completed call {id} should still be present in the active exec cell"
                            );
                            CompletionAction::RefreshActive
                        }
                    }
                    None => CompletionAction::Orphan { output },
                };

                match action {
                    CompletionAction::KeepGrouped | CompletionAction::RefreshActive => {
                        self.bump_active_cell_revision();
                        self.request_redraw();
                    }
                    CompletionAction::FlushSingle => self.flush_active_cell(),
                    CompletionAction::Extract {
                        call,
                        remaining_is_empty,
                        flush_completed_remainder,
                    } => {
                        if remaining_is_empty {
                            self.transcript.active_cell = None;
                            self.bump_active_cell_revision();
                        } else if flush_completed_remainder {
                            self.flush_active_cell();
                        } else {
                            self.bump_active_cell_revision();
                        }
                        let cell = ExecCell::new_standalone(call, self.config.animations);
                        self.app_event_tx
                            .send(AppEvent::InsertHistoryCell(Box::new(cell)));
                        self.request_redraw();
                    }
                    CompletionAction::Orphan { output } => {
                        let mut orphan = ExecCell::new_standalone(
                            crate::exec_cell::ExecCall {
                                call_id: id.clone(),
                                command,
                                parsed,
                                presentation,
                                output: None,
                                source,
                                start_time: Some(Instant::now()),
                                duration: None,
                                interaction_input: None,
                            },
                            self.config.animations,
                        );
                        let completed = orphan.complete_call(&id, output, duration);
                        debug_assert!(completed, "new orphan exec cell should contain {id}");
                        self.app_event_tx
                            .send(AppEvent::InsertHistoryCell(Box::new(orphan)));
                        self.request_redraw();
                    }
                }
            }
            ExecEndTarget::OrphanHistoryWhileActiveExec => {
                let mut orphan = ExecCell::new_standalone(
                    crate::exec_cell::ExecCall {
                        call_id: id.clone(),
                        command,
                        parsed,
                        presentation,
                        output: None,
                        source,
                        start_time: Some(Instant::now()),
                        duration: None,
                        interaction_input: None,
                    },
                    self.config.animations,
                );
                let completed = orphan.complete_call(&id, output, duration);
                debug_assert!(completed, "new orphan exec cell should contain {id}");
                self.app_event_tx
                    .send(AppEvent::InsertHistoryCell(Box::new(orphan)));
                self.request_redraw();
            }
            ExecEndTarget::AppendCompletedExploration => {
                if let Some(cell) = self
                    .transcript
                    .active_cell
                    .as_mut()
                    .and_then(|cell| cell.as_any_mut().downcast_mut::<ExecCell>())
                {
                    let added = cell.add_call(
                        id.clone(),
                        command.clone(),
                        parsed.clone(),
                        presentation.clone(),
                        source,
                        /*interaction_input*/ None,
                    );
                    if added {
                        let completed = cell.complete_call(&id, output, duration);
                        debug_assert!(completed, "joined exploration call should contain {id}");
                        self.bump_active_cell_revision();
                        self.request_redraw();
                        debug_assert!(!is_user_shell, "user shell calls cannot join exploration");
                        return;
                    }
                }

                // The target is chosen from the current active cell immediately above, but
                // recover defensively if that invariant changes instead of dropping resume data.
                self.flush_active_cell();
                let mut cell = new_active_exec_command(
                    id.clone(),
                    command,
                    parsed,
                    presentation,
                    source,
                    /*interaction_input*/ None,
                    self.config.animations,
                );
                let completed = cell.complete_call(&id, output, duration);
                debug_assert!(completed, "fallback exploration cell should contain {id}");
                self.transcript.active_cell = Some(Box::new(cell));
                self.bump_active_cell_revision();
                self.request_redraw();
            }
            ExecEndTarget::NewCell => {
                self.flush_active_cell();
                let mut cell = new_active_exec_command(
                    id.clone(),
                    command,
                    parsed,
                    presentation,
                    source,
                    /*interaction_input*/ None,
                    self.config.animations,
                );
                let completed = cell.complete_call(&id, output, duration);
                debug_assert!(completed, "new exec cell should contain {id}");
                if cell.should_flush() {
                    self.add_to_history(cell);
                } else {
                    self.transcript.active_cell = Some(Box::new(cell));
                    self.bump_active_cell_revision();
                    self.request_redraw();
                }
            }
        }
        if is_user_shell {
            self.maybe_send_next_queued_input();
        }
    }
}
