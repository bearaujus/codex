//! Rendering for a single command or structured inspection card.

use super::*;

impl ExecCell {
    pub(super) fn command_display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let [call] = &self.calls.as_slice() else {
            panic!("Expected exactly one call in a command display cell");
        };
        let layout = EXEC_DISPLAY_LAYOUT;
        let success = call
            .duration
            .and_then(|_| call.output.as_ref().map(|output| output.exit_code == 0));
        let warning = matches!(success, Some(true))
            && call
                .output
                .as_ref()
                .is_some_and(CommandOutput::has_diagnostic_signal);
        let bullet = match (success, warning) {
            (Some(true), true) => "•".cyan().bold(),
            (Some(true), false) => "•".green().bold(),
            (Some(false), _) => "•".red().bold(),
            (None, _) => activity_marker(call.start_time, self.animations_enabled()),
        };
        let is_interaction = call.is_unified_exec_interaction();
        let inspection_target = match &call.presentation {
            CommandPresentation::Inspection { target } => Some(sanitize_exploration_text(target)),
            CommandPresentation::Command | CommandPresentation::Exploration => None,
        };

        let mut header_line = if let Some(target) = inspection_target.as_deref() {
            let mut line = Line::from(vec![bullet.clone(), " ".into()]);
            match success {
                None => {
                    line.push_span("Inspecting ".bold());
                    line.push_span(target.to_string());
                }
                Some(true) if warning => {
                    line.push_span("Inspected ".bold());
                    line.push_span(target.to_string());
                    line.push_span(" · warnings".cyan().bold());
                }
                Some(true) => {
                    line.push_span(format!("Inspected {target}"));
                }
                Some(false) => {
                    let exit_code = call
                        .output
                        .as_ref()
                        .map(|output| output.exit_code)
                        .unwrap_or_default();
                    line.push_span(format!("Inspection failed (exit {exit_code})").bold());
                    line.push_span(" ");
                    line.push_span(target.to_string());
                }
            }
            line
        } else if is_interaction {
            Line::from(vec![bullet.clone(), " ".into()])
        } else {
            let title = if self.is_active() {
                "Running".to_string()
            } else if call.is_user_shell_command() {
                "You ran".to_string()
            } else if warning {
                "Ran with warnings".to_string()
            } else if matches!(success, Some(false)) {
                let exit_code = call
                    .output
                    .as_ref()
                    .map(|output| output.exit_code)
                    .unwrap_or_default();
                format!("Failed (exit {exit_code})")
            } else {
                "Ran".to_string()
            };
            let title = match success {
                Some(true) if !warning => title.into(),
                Some(_) | None => title.bold(),
            };
            Line::from(vec![bullet.clone(), " ".into(), title, " ".into()])
        };
        let header_prefix_width = header_line.width();

        let highlighted_lines = if inspection_target.is_some() {
            Vec::new()
        } else {
            let cmd_display = if call.is_unified_exec_interaction() {
                format_unified_exec_interaction(&call.command, call.interaction_input.as_deref())
            } else {
                strip_bash_lc_and_escape(&call.command)
            };
            highlight_bash_to_lines(&cmd_display)
        };

        let continuation_wrap_width = layout.command_continuation.wrap_width(width);
        let continuation_opts =
            RtOptions::new(continuation_wrap_width).word_splitter(WordSplitter::NoHyphenation);

        let mut continuation_lines: Vec<Line<'static>> = Vec::new();

        if let Some((first, rest)) = highlighted_lines.split_first() {
            let available_first_width = (width as usize).saturating_sub(header_prefix_width).max(1);
            let first_opts =
                RtOptions::new(available_first_width).word_splitter(WordSplitter::NoHyphenation);

            let mut first_wrapped: Vec<Line<'static>> = Vec::new();
            push_owned_lines(&adaptive_wrap_line(first, first_opts), &mut first_wrapped);
            let mut first_wrapped_iter = first_wrapped.into_iter();
            if let Some(first_segment) = first_wrapped_iter.next() {
                header_line.extend(first_segment);
            }
            continuation_lines.extend(first_wrapped_iter);

            for line in rest {
                push_owned_lines(
                    &adaptive_wrap_line(line, continuation_opts.clone()),
                    &mut continuation_lines,
                );
            }
        }

        let clamp_agent_command = !call.is_user_shell_command()
            && !is_interaction
            && usize::from(width) > AGENT_COMMAND_PREVIEW_MAX_WIDTH;
        let command_was_truncated = clamp_agent_command
            && (line_width(&header_line) > AGENT_COMMAND_PREVIEW_MAX_WIDTH
                || !continuation_lines.is_empty());
        if command_was_truncated {
            header_line = truncate_line_with_ellipsis_if_overflow(
                header_line,
                AGENT_COMMAND_PREVIEW_MAX_WIDTH,
            );
            continuation_lines.clear();
        }

        let continuation_lines = Self::limit_lines_from_start(
            &continuation_lines,
            layout.command_continuation_max_lines,
        );
        let mut lines: Vec<Line<'static>> = vec![header_line];
        if !continuation_lines.is_empty() {
            let (initial_prefix, subsequent_prefix) = layout.command_continuation.dimmed_spans();
            lines.extend(prefix_lines(
                continuation_lines,
                initial_prefix,
                subsequent_prefix,
            ));
        }

        if let Some(output) = call.output.as_ref() {
            let line_limit = if call.is_user_shell_command() {
                USER_SHELL_TOOL_CALL_MAX_LINES
            } else if inspection_target.is_some() && !warning && !matches!(success, Some(false)) {
                3
            } else {
                TOOL_CALL_MAX_LINES
            };
            let raw_output = output_lines(
                Some(output),
                OutputLinesParams {
                    line_limit,
                    only_err: false,
                    include_angle_pipe: false,
                    include_prefix: false,
                },
            );
            let display_limit = if call.is_user_shell_command() {
                USER_SHELL_TOOL_CALL_MAX_LINES
            } else if inspection_target.is_some() && !warning && !matches!(success, Some(false)) {
                3
            } else {
                layout.output_max_lines
            };

            if raw_output.lines.is_empty() {
                if !call.is_unified_exec_interaction() {
                    let (initial_prefix, subsequent_prefix) = layout.output_block.dimmed_spans();
                    let mut no_output = prefix_lines(
                        vec![Line::from("(no output)".dim())],
                        initial_prefix,
                        subsequent_prefix,
                    );
                    Self::close_output_block(&mut no_output);
                    lines.extend(no_output);
                }
            } else {
                // Wrap first so truncation is applied to on-screen rows. A
                // small number of very long logical lines must not flood the
                // viewport.
                let mut wrapped_output: Vec<Line<'static>> = Vec::new();
                let output_wrap_width = layout.output_block.wrap_width(width);
                let output_opts =
                    RtOptions::new(output_wrap_width).word_splitter(WordSplitter::NoHyphenation);
                for line in &raw_output.lines {
                    push_owned_lines(
                        &adaptive_wrap_line(line, output_opts.clone()),
                        &mut wrapped_output,
                    );
                }

                let (initial_prefix, subsequent_prefix) = layout.output_block.dimmed_spans();
                let prefixed_output =
                    prefix_lines(wrapped_output, initial_prefix, subsequent_prefix);
                let mut trimmed_output = Self::truncate_lines_middle(
                    &prefixed_output,
                    display_limit,
                    width,
                    raw_output.omitted,
                    Some(Line::from(
                        Span::from(layout.output_block.subsequent_prefix).dim(),
                    )),
                );
                Self::close_output_block(&mut trimmed_output);

                if !trimmed_output.is_empty() {
                    lines.extend(trimmed_output);
                }
            }
        }

        lines
    }
}
