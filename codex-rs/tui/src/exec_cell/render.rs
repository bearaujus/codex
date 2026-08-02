use std::time::Instant;

#[path = "render/command.rs"]
mod command;
#[path = "render/exploration.rs"]
mod exploration;
#[path = "render/exploration_rows.rs"]
mod exploration_rows;

use super::model::CommandOutput;
use super::model::CommandPresentation;
use super::model::ExecCall;
use super::model::ExecCell;
use crate::exec_command::strip_bash_lc_and_escape;
use crate::history_cell::HistoryCell;
use crate::history_cell::plain_lines;
use crate::line_truncation::line_width;
use crate::line_truncation::truncate_line_with_ellipsis_if_overflow;
use crate::motion::MotionMode;
use crate::motion::ReducedMotionIndicator;
use crate::motion::activity_indicator;
use crate::render::highlight::highlight_bash_to_lines;
use crate::render::line_utils::prefix_lines;
use crate::render::line_utils::push_owned_lines;
use crate::wrapping::RtOptions;
use crate::wrapping::adaptive_wrap_line;
use crate::wrapping::adaptive_wrap_lines;
use codex_ansi_escape::ansi_escape;
use codex_ansi_escape::ansi_escape_line;
use codex_app_server_protocol::CommandExecutionSource as ExecCommandSource;
use codex_protocol::parse_command::ParsedCommand;
use codex_shell_command::bash::extract_bash_command;
use codex_utils_elapsed::format_duration;
use ratatui::prelude::*;
use ratatui::style::Modifier;
use ratatui::style::Stylize;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Wrap;
use textwrap::WordSplitter;
use unicode_width::UnicodeWidthStr;

pub(crate) const TOOL_CALL_MAX_LINES: usize = 5;
const USER_SHELL_TOOL_CALL_MAX_LINES: usize = 50;
const MAX_INTERACTION_PREVIEW_CHARS: usize = 80;
const AGENT_COMMAND_PREVIEW_MAX_WIDTH: usize = 140;

pub(crate) struct OutputLinesParams {
    pub(crate) line_limit: usize,
    pub(crate) only_err: bool,
    pub(crate) include_angle_pipe: bool,
    pub(crate) include_prefix: bool,
}

pub(crate) fn new_active_exec_command(
    call_id: String,
    command: Vec<String>,
    parsed: Vec<ParsedCommand>,
    presentation: CommandPresentation,
    source: ExecCommandSource,
    interaction_input: Option<String>,
    animations_enabled: bool,
) -> ExecCell {
    ExecCell::new(
        ExecCall {
            call_id,
            command,
            parsed,
            presentation,
            output: None,
            source,
            start_time: Some(Instant::now()),
            duration: None,
            interaction_input,
        },
        animations_enabled,
    )
}

fn format_unified_exec_interaction(command: &[String], input: Option<&str>) -> String {
    let command_display = if let Some((_, script)) = extract_bash_command(command) {
        script.to_string()
    } else {
        command.join(" ")
    };
    match input {
        Some(data) if !data.is_empty() => {
            let preview = summarize_interaction_input(data);
            format!("Interacted with `{command_display}`, sent `{preview}`")
        }
        _ => format!("Waited for `{command_display}`"),
    }
}

fn summarize_interaction_input(input: &str) -> String {
    let single_line = input.replace('\n', "\\n");
    let sanitized = single_line.replace('`', "\\`");
    if sanitized.chars().count() <= MAX_INTERACTION_PREVIEW_CHARS {
        return sanitized;
    }

    let mut preview = String::new();
    for ch in sanitized.chars().take(MAX_INTERACTION_PREVIEW_CHARS) {
        preview.push(ch);
    }
    preview.push_str("...");
    preview
}

#[derive(Clone)]
pub(crate) struct OutputLines {
    pub(crate) lines: Vec<Line<'static>>,
    pub(crate) omitted: Option<usize>,
}

pub(crate) fn output_lines(
    output: Option<&CommandOutput>,
    params: OutputLinesParams,
) -> OutputLines {
    let OutputLinesParams {
        line_limit,
        only_err,
        include_angle_pipe,
        include_prefix,
    } = params;
    let output = match output {
        Some(output) if only_err && output.exit_code == 0 => {
            return OutputLines {
                lines: Vec::new(),
                omitted: None,
            };
        }
        Some(output) => output,
        None => {
            return OutputLines {
                lines: Vec::new(),
                omitted: None,
            };
        }
    };

    let (total, retained) = output.line_counts();
    let mut out: Vec<Line<'static>> = Vec::new();

    let head_end = total.min(line_limit).min(retained);
    for (i, raw) in output.lines().take(head_end).enumerate() {
        let mut line = ansi_escape_line(raw.as_ref());
        let prefix = if !include_prefix {
            ""
        } else if i == 0 && include_angle_pipe {
            "  └ "
        } else {
            "    "
        };
        line.spans.insert(0, prefix.into());
        line.spans.iter_mut().for_each(|span| {
            span.style = span.style.add_modifier(Modifier::DIM);
        });
        out.push(line);
    }

    let tail_len = total
        .saturating_sub(head_end)
        .min(line_limit)
        .min(retained.saturating_sub(head_end));
    let omitted = total.saturating_sub(head_end + tail_len);
    let omitted = (omitted > 0).then_some(omitted);
    if let Some(omitted) = omitted {
        out.push(ExecCell::output_ellipsis_line(omitted));
    }

    let tail = output.lines().rev().take(tail_len).collect::<Vec<_>>();
    for raw in tail.into_iter().rev() {
        let mut line = ansi_escape_line(raw.as_ref());
        if include_prefix {
            line.spans.insert(0, "    ".into());
        }
        line.spans.iter_mut().for_each(|span| {
            span.style = span.style.add_modifier(Modifier::DIM);
        });
        out.push(line);
    }

    OutputLines {
        lines: out,
        omitted,
    }
}

fn sanitize_exploration_text(text: &str) -> String {
    let visible = ansi_escape(text).lines.into_iter().enumerate().fold(
        String::new(),
        |mut visible, (index, line)| {
            if index > 0 {
                visible.push('\n');
            }
            for span in line.spans {
                visible.push_str(span.content.as_ref());
            }
            visible
        },
    );
    let mut sanitized = String::with_capacity(visible.len());
    let mut pending_space = false;
    for ch in visible.chars() {
        if ch.is_whitespace() || ch.is_control() {
            pending_space |= !sanitized.is_empty();
        } else {
            if pending_space {
                sanitized.push(' ');
                pending_space = false;
            }
            sanitized.push(ch);
        }
    }
    sanitized
}

fn activity_marker(start_time: Option<Instant>, animations_enabled: bool) -> Span<'static> {
    activity_indicator(
        start_time,
        MotionMode::from_animations_enabled(animations_enabled),
        ReducedMotionIndicator::StaticBullet,
    )
    .unwrap_or_else(|| "•".dim())
}

impl HistoryCell for ExecCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        if self.is_exploring_cell() {
            self.exploring_display_lines(width)
        } else if self.is_parallel_cell() {
            self.parallel_display_lines(width)
        } else {
            self.command_display_lines(width)
        }
    }

    fn transcript_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut lines: Vec<Line<'static>> = vec![];
        for (i, call) in self.iter_calls().enumerate() {
            if i > 0 {
                lines.push("".into());
            }
            let script = strip_bash_lc_and_escape(&call.command);
            let highlighted_script = highlight_bash_to_lines(&script);
            let cmd_display = adaptive_wrap_lines(
                &highlighted_script,
                RtOptions::new(width as usize)
                    .initial_indent("$ ".magenta().into())
                    .subsequent_indent("    ".into()),
            );
            lines.extend(cmd_display);

            if let Some(output) = call.output.as_ref() {
                if !call.is_unified_exec_interaction() {
                    let wrap_width = width.max(1) as usize;
                    let wrap_opts = RtOptions::new(wrap_width);
                    for unwrapped in output
                        .transcript_lines()
                        .map(|line| ansi_escape_line(line.as_ref()))
                    {
                        let wrapped = adaptive_wrap_line(&unwrapped, wrap_opts.clone());
                        push_owned_lines(&wrapped, &mut lines);
                    }
                }
                if let Some(duration) = call.duration {
                    let duration = format_duration(duration);
                    let mut result: Line = if output.exit_code == 0 {
                        Line::from("✓".green().bold())
                    } else {
                        Line::from(vec![
                            "✗".red().bold(),
                            format!(" ({})", output.exit_code).into(),
                        ])
                    };
                    result.push_span(format!(" • {duration}").dim());
                    lines.push(result);
                }
            }
        }
        lines
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        plain_lines(self.transcript_lines(u16::MAX))
    }
}

impl ExecCell {
    fn output_ellipsis_text(omitted: usize) -> String {
        let lines = if omitted == 1 { "line" } else { "lines" };
        format!("… +{omitted} {lines}")
    }

    fn output_ellipsis_line(omitted: usize) -> Line<'static> {
        Line::from(vec![Self::output_ellipsis_text(omitted).dim()])
    }

    fn parallel_display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let active_count = self.active_call_count();
        let is_active = active_count > 0;
        let count = if is_active {
            active_count
        } else {
            self.calls.len()
        };
        let noun = if count == 1 { "command" } else { "commands" };
        let failed = !is_active
            && self.calls.iter().any(|call| {
                call.output
                    .as_ref()
                    .is_none_or(|output| output.exit_code != 0)
            });
        let warning = !is_active
            && !failed
            && self.calls.iter().any(|call| {
                call.output
                    .as_ref()
                    .is_some_and(CommandOutput::has_diagnostic_signal)
            });
        let (marker, title) = if is_active {
            (
                activity_marker(self.active_start_time(), self.animations_enabled()),
                format!("Running {count} {noun}").bold(),
            )
        } else if failed {
            ("•".red().bold(), format!("Failed {count} {noun}").bold())
        } else if warning {
            (
                "•".cyan().bold(),
                format!("Ran {count} {noun} with warnings").bold(),
            )
        } else {
            ("•".green().bold(), format!("Ran {count} {noun}").into())
        };
        let mut out = vec![Line::from(vec![marker, " ".into(), title])];

        const MAX_PARALLEL_ROWS: usize = 3;
        let content_width = usize::from(width).saturating_sub(4).max(1);
        let calls = if is_active {
            self.active_calls().collect::<Vec<_>>()
        } else {
            self.iter_calls().collect::<Vec<_>>()
        };
        let omitted = calls.len().saturating_sub(MAX_PARALLEL_ROWS);
        let mut command_lines = calls
            .into_iter()
            .take(MAX_PARALLEL_ROWS)
            .map(|call| {
                let command = strip_bash_lc_and_escape(&call.command);
                let line = highlight_bash_to_lines(&command)
                    .into_iter()
                    .next()
                    .unwrap_or_default();
                truncate_line_with_ellipsis_if_overflow(line, content_width)
            })
            .collect::<Vec<_>>();
        if omitted > 0 {
            command_lines.push(Line::from(format!("… +{omitted} commands")).dim());
        }
        out.extend(prefix_lines(command_lines, "  └ ".dim(), "    ".into()));
        out
    }

    fn limit_lines_from_start(lines: &[Line<'static>], keep: usize) -> Vec<Line<'static>> {
        if lines.len() <= keep {
            return lines.to_vec();
        }
        if keep == 0 {
            return vec![Self::ellipsis_line(lines.len())];
        }

        let mut out: Vec<Line<'static>> = lines[..keep].to_vec();
        out.push(Self::ellipsis_line(lines.len() - keep));
        out
    }

    /// Truncates a list of lines to fit within `max_rows` viewport rows,
    /// keeping a head portion and a tail portion with an ellipsis line
    /// in between.
    ///
    /// `max_rows` is measured in viewport rows (the actual space a line
    /// occupies after `Paragraph::wrap`), not logical lines. Each line's
    /// row cost is computed via `Paragraph::line_count` at the given
    /// `width`. This ensures that a single logical line containing a
    /// long URL (which wraps to several viewport rows) is properly
    /// accounted for.
    ///
    /// The ellipsis message reports the number of omitted *lines*
    /// (logical, not rows) to keep the count stable across terminal
    /// widths. `omitted_hint` carries forward any previously reported
    /// omitted count (from upstream truncation); `ellipsis_prefix`
    /// prepends the output gutter prefix to the ellipsis line.
    fn truncate_lines_middle(
        lines: &[Line<'static>],
        max_rows: usize,
        width: u16,
        omitted_hint: Option<usize>,
        ellipsis_prefix: Option<Line<'static>>,
    ) -> Vec<Line<'static>> {
        let width = width.max(1);
        if max_rows == 0 {
            return Vec::new();
        }
        let line_rows: Vec<usize> = lines
            .iter()
            .map(|line| {
                let is_whitespace_only = line
                    .spans
                    .iter()
                    .all(|span| span.content.chars().all(char::is_whitespace));
                if is_whitespace_only {
                    line.width().div_ceil(usize::from(width)).max(1)
                } else {
                    Paragraph::new(Text::from(vec![line.clone()]))
                        .wrap(Wrap { trim: false })
                        .line_count(width)
                        .max(1)
                }
            })
            .collect();
        let total_rows: usize = line_rows.iter().sum();
        if total_rows <= max_rows {
            return lines.to_vec();
        }
        // Reserve space for the omission row itself so the returned output still
        // respects the row budget on narrow terminals.
        let estimated_omitted = omitted_hint.unwrap_or(0)
            + lines
                .len()
                .saturating_sub(usize::from(omitted_hint.is_some()));
        let ellipsis_rows =
            Self::output_ellipsis_row_count(estimated_omitted, width, ellipsis_prefix.as_ref());
        if ellipsis_rows >= max_rows {
            return vec![Self::output_ellipsis_line_with_prefix(
                estimated_omitted,
                ellipsis_prefix.as_ref(),
            )];
        }

        let available_rows = max_rows - ellipsis_rows;
        let head_budget = available_rows / 2;
        let tail_budget = available_rows - head_budget;
        let mut head_lines: Vec<Line<'static>> = Vec::new();
        let mut head_rows = 0usize;
        let mut head_end = 0usize;
        while head_end < lines.len() {
            let line_row_count = line_rows[head_end];
            if head_rows + line_row_count > head_budget {
                break;
            }
            head_rows += line_row_count;
            head_lines.push(lines[head_end].clone());
            head_end += 1;
        }

        let mut tail_lines_reversed: Vec<Line<'static>> = Vec::new();
        let mut tail_rows = 0usize;
        let mut tail_start = lines.len();
        while tail_start > head_end {
            let idx = tail_start - 1;
            let line_row_count = line_rows[idx];
            if tail_rows + line_row_count > tail_budget {
                break;
            }
            tail_rows += line_row_count;
            tail_lines_reversed.push(lines[idx].clone());
            tail_start -= 1;
        }

        let mut out = head_lines;
        let base = omitted_hint.unwrap_or(0);
        let additional = lines
            .len()
            .saturating_sub(out.len() + tail_lines_reversed.len())
            .saturating_sub(usize::from(omitted_hint.is_some()));
        out.push(Self::output_ellipsis_line_with_prefix(
            base + additional,
            ellipsis_prefix.as_ref(),
        ));

        out.extend(tail_lines_reversed.into_iter().rev());

        out
    }

    fn ellipsis_line(omitted: usize) -> Line<'static> {
        let lines = if omitted == 1 { "line" } else { "lines" };
        Line::from(vec![format!("… +{omitted} command {lines}").dim()])
    }

    fn close_output_block(lines: &mut [Line<'static>]) {
        let Some(prefix) = lines.last_mut().and_then(|line| line.spans.first_mut()) else {
            return;
        };
        let style = prefix.style;
        *prefix = Span::styled("  └ ", style);
    }

    fn output_ellipsis_row_count(
        omitted: usize,
        width: u16,
        prefix: Option<&Line<'static>>,
    ) -> usize {
        Paragraph::new(Text::from(vec![Self::output_ellipsis_line_with_prefix(
            omitted, prefix,
        )]))
        .wrap(Wrap { trim: false })
        .line_count(width)
        .max(1)
    }

    /// Builds a compact output ellipsis line with an optional leading prefix so
    /// the ellipsis aligns with the output gutter.
    fn output_ellipsis_line_with_prefix(
        omitted: usize,
        prefix: Option<&Line<'static>>,
    ) -> Line<'static> {
        let mut line = prefix.cloned().unwrap_or_default();
        line.push_span(Self::output_ellipsis_text(omitted).dim());
        line
    }
}

#[derive(Clone, Copy)]
struct PrefixedBlock {
    initial_prefix: &'static str,
    subsequent_prefix: &'static str,
}

impl PrefixedBlock {
    const fn new(initial_prefix: &'static str, subsequent_prefix: &'static str) -> Self {
        Self {
            initial_prefix,
            subsequent_prefix,
        }
    }

    fn dimmed_spans(self) -> (Span<'static>, Span<'static>) {
        (
            Span::from(self.initial_prefix).dim(),
            Span::from(self.subsequent_prefix).dim(),
        )
    }

    fn wrap_width(self, total_width: u16) -> usize {
        let prefix_width = UnicodeWidthStr::width(self.initial_prefix)
            .max(UnicodeWidthStr::width(self.subsequent_prefix));
        usize::from(total_width).saturating_sub(prefix_width).max(1)
    }
}

#[derive(Clone, Copy)]
struct ExecDisplayLayout {
    command_continuation: PrefixedBlock,
    command_continuation_max_lines: usize,
    output_block: PrefixedBlock,
    output_max_lines: usize,
}

impl ExecDisplayLayout {
    const fn new(
        command_continuation: PrefixedBlock,
        command_continuation_max_lines: usize,
        output_block: PrefixedBlock,
        output_max_lines: usize,
    ) -> Self {
        Self {
            command_continuation,
            command_continuation_max_lines,
            output_block,
            output_max_lines,
        }
    }
}

const EXEC_DISPLAY_LAYOUT: ExecDisplayLayout = ExecDisplayLayout::new(
    PrefixedBlock::new("  │ ", "  │ "),
    /*command_continuation_max_lines*/ 2,
    PrefixedBlock::new("  │ ", "  │ "),
    /*output_max_lines*/ 5,
);

#[cfg(test)]
mod tests {
    use super::*;
    use codex_app_server_protocol::CommandExecutionSource as ExecCommandSource;
    use itertools::Itertools;
    use pretty_assertions::assert_eq;

    fn render_line_text(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>()
    }

    #[test]
    fn user_shell_output_is_limited_by_screen_lines() {
        let long_url_like = format!(
            "https://example.test/api/v1/projects/alpha-team/releases/2026-02-17/builds/1234567890/{}",
            "very-long-segment-".repeat(120),
        );
        let aggregated_output = format!("{long_url_like}\n{long_url_like}\n");

        // Baseline: how many screen lines would we get if we simply wrapped
        // all logical lines without any truncation?
        let output = CommandOutput::new(/*exit_code*/ 0, aggregated_output);
        let width = 20;
        let layout = EXEC_DISPLAY_LAYOUT;
        let raw_output = output_lines(
            Some(&output),
            OutputLinesParams {
                // Large enough to include all logical lines without
                // triggering the ellipsis in `output_lines`.
                line_limit: 100,
                only_err: false,
                include_angle_pipe: false,
                include_prefix: false,
            },
        );
        let output_wrap_width = layout.output_block.wrap_width(width);
        let output_opts =
            RtOptions::new(output_wrap_width).word_splitter(WordSplitter::NoHyphenation);
        let mut full_wrapped_output: Vec<Line<'static>> = Vec::new();
        for line in &raw_output.lines {
            push_owned_lines(
                &adaptive_wrap_line(line, output_opts.clone()),
                &mut full_wrapped_output,
            );
        }
        let full_prefixed_output = prefix_lines(
            full_wrapped_output,
            Span::from(layout.output_block.initial_prefix).dim(),
            Span::from(layout.output_block.subsequent_prefix),
        );
        let full_screen_lines = Paragraph::new(Text::from(full_prefixed_output))
            .wrap(Wrap { trim: false })
            .line_count(width);

        // Sanity check: this scenario should produce more screen lines than
        // the user shell per-call limit when no truncation is applied. If
        // this ever fails, the test no longer exercises the regression.
        assert!(
            full_screen_lines > USER_SHELL_TOOL_CALL_MAX_LINES,
            "expected unbounded wrapping to produce more than {USER_SHELL_TOOL_CALL_MAX_LINES} screen lines, got {full_screen_lines}",
        );

        let call = ExecCall {
            call_id: "call-id".to_string(),
            command: vec!["bash".into(), "-lc".into(), "echo long".into()],
            parsed: Vec::new(),
            presentation: CommandPresentation::Command,
            output: Some(output),
            source: ExecCommandSource::UserShell,
            start_time: None,
            duration: None,
            interaction_input: None,
        };

        let cell = ExecCell::new(call, /*animations_enabled*/ false);

        // Use a narrow width so each logical line wraps into many on-screen lines.
        let lines = cell.command_display_lines(width);
        let rendered_rows = Paragraph::new(Text::from(lines.clone()))
            .wrap(Wrap { trim: false })
            .line_count(width);
        let header_rows = Paragraph::new(Text::from(vec![lines[0].clone()]))
            .wrap(Wrap { trim: false })
            .line_count(width);
        let output_screen_rows = rendered_rows.saturating_sub(header_rows);

        let contains_ellipsis = lines
            .iter()
            .any(|line| line.spans.iter().any(|span| span.content.contains("… +")));

        // Regression guard: previously this scenario could render hundreds of
        // wrapped rows because truncation happened before final viewport
        // wrapping. The row-aware truncation now caps visible output rows.
        assert!(
            output_screen_rows <= USER_SHELL_TOOL_CALL_MAX_LINES,
            "expected at most {USER_SHELL_TOOL_CALL_MAX_LINES} output rows, got {output_screen_rows} (total rows: {rendered_rows})",
        );
        assert!(
            contains_ellipsis,
            "expected truncated output to include an ellipsis line"
        );
        assert!(
            lines
                .iter()
                .map(render_line_text)
                .any(|line| line.contains("… +")),
            "expected truncated output to report the omitted line count"
        );
    }

    #[test]
    fn truncate_lines_middle_keeps_omitted_count_in_line_units() {
        let lines = vec![
            Line::from("  └ short"),
            Line::from("    this-is-a-very-long-token-that-wraps-many-rows"),
            Line::from(format!(
                "    {}",
                ExecCell::output_ellipsis_text(/*omitted*/ 4)
            )),
            Line::from("    tail"),
        ];

        let truncated = ExecCell::truncate_lines_middle(
            &lines,
            /*max_rows*/ 2,
            /*width*/ 80,
            Some(4),
            Some(Line::from("    ".dim())),
        );
        let rendered: Vec<String> = truncated.iter().map(render_line_text).collect();

        assert!(
            rendered.iter().any(|line| line.contains("… +6 lines")),
            "expected omitted hint to count hidden lines (not wrapped rows), got: {rendered:?}"
        );
    }

    #[test]
    fn output_lines_ellipsis_reports_only_the_omitted_count() {
        let output = CommandOutput::new(
            /*exit_code*/ 0,
            (1..=7).map(|n| n.to_string()).join("\n"),
        );

        let rendered: Vec<String> = output_lines(
            Some(&output),
            OutputLinesParams {
                line_limit: 2,
                only_err: false,
                include_angle_pipe: false,
                include_prefix: false,
            },
        )
        .lines
        .iter()
        .map(render_line_text)
        .collect();

        assert_eq!(rendered, vec!["1", "2", "… +3 lines", "6", "7",]);
    }

    #[test]
    fn output_ellipsis_uses_singular_line_grammar() {
        assert_eq!(ExecCell::output_ellipsis_text(1), "… +1 line");
    }

    #[test]
    fn output_lines_handles_newline_dense_output_without_materializing_every_line() {
        let output = CommandOutput::new(/*exit_code*/ 0, "\n".repeat(100_000));

        let rendered = output_lines(
            Some(&output),
            OutputLinesParams {
                line_limit: 5,
                only_err: false,
                include_angle_pipe: false,
                include_prefix: false,
            },
        );

        assert_eq!(rendered.lines.len(), 11);
        assert_eq!(rendered.omitted, Some(99_990));
    }

    #[test]
    fn streamed_output_renders_head_tail_previews() {
        let mut cell = new_active_exec_command(
            "call-id".to_string(),
            vec!["bash".into(), "-lc".into(), "echo output".into()],
            Vec::new(),
            CommandPresentation::Command,
            ExecCommandSource::Agent,
            /*interaction_input*/ None,
            /*animations_enabled*/ false,
        );
        for line in 1..=160 {
            assert!(cell.append_output("call-id", &format!("line {line}\n")));
        }
        let output = cell.calls[0].output.as_ref().expect("streamed output");

        let agent = output_lines(
            Some(output),
            OutputLinesParams {
                line_limit: TOOL_CALL_MAX_LINES,
                only_err: false,
                include_angle_pipe: false,
                include_prefix: false,
            },
        );
        assert_eq!(agent.lines.len(), 11);
        assert_eq!(agent.omitted, Some(150));
        assert_eq!(render_line_text(&agent.lines[0]), "line 1");
        assert_eq!(render_line_text(&agent.lines[10]), "line 160");

        let user_shell = output_lines(
            Some(output),
            OutputLinesParams {
                line_limit: USER_SHELL_TOOL_CALL_MAX_LINES,
                only_err: false,
                include_angle_pipe: false,
                include_prefix: false,
            },
        );
        assert_eq!(user_shell.lines.len(), 101);
        assert_eq!(user_shell.omitted, Some(60));
        assert_eq!(render_line_text(&user_shell.lines[0]), "line 1");
        assert_eq!(render_line_text(&user_shell.lines[100]), "line 160");
    }

    #[test]
    fn truncated_live_output_preview_and_transcript_snapshot() {
        let mut cell = new_active_exec_command(
            "call-id".to_string(),
            vec!["bash".into(), "-lc".into(), "echo output".into()],
            Vec::new(),
            CommandPresentation::Command,
            ExecCommandSource::Agent,
            /*interaction_input*/ None,
            /*animations_enabled*/ false,
        );
        let hidden = "\x1b[2m".repeat(300_000);
        let output = format!(
            "\x1b[31mhead error that wraps onto the next row\x1b[0m{hidden}\x1b[32mtail output that also wraps\x1b[0m"
        );
        assert!(cell.append_output("call-id", &output));

        let preview = cell.display_lines(/*width*/ 60);
        cell.calls[0].start_time = None;
        cell.mark_failed();
        let transcript = cell.transcript_lines(/*width*/ 60);

        insta::assert_debug_snapshot!(
            "truncated_live_output_preview_and_transcript",
            (preview, transcript)
        );
    }

    #[test]
    fn command_truncation_ellipsis_does_not_include_transcript_hint() {
        let truncated = ExecCell::limit_lines_from_start(
            &[
                Line::from("first"),
                Line::from("second"),
                Line::from("third"),
            ],
            /*keep*/ 2,
        );
        let rendered: Vec<String> = truncated.iter().map(render_line_text).collect();

        assert_eq!(
            rendered,
            vec![
                "first".to_string(),
                "second".to_string(),
                "… +1 command line".to_string(),
            ]
        );
    }

    #[test]
    fn truncate_lines_middle_does_not_truncate_blank_prefixed_output_lines() {
        let mut lines = vec![Line::from("  └ start")];
        lines.extend(std::iter::repeat_n(Line::from("    "), 26));
        lines.push(Line::from("    end"));

        let truncated = ExecCell::truncate_lines_middle(
            &lines, /*max_rows*/ 28, /*width*/ 80, /*omitted_hint*/ None,
            /*ellipsis_prefix*/ None,
        );

        assert_eq!(truncated, lines);
    }

    #[test]
    fn command_display_does_not_split_long_url_token() {
        let url = "http://example.com/long-url-with-dashes-wider-than-terminal-window/blah-blah-blah-text/more-gibberish-text";

        let call = ExecCall {
            call_id: "call-id".to_string(),
            command: vec!["bash".into(), "-lc".into(), format!("echo {url}")],
            parsed: Vec::new(),
            presentation: CommandPresentation::Command,
            output: None,
            source: ExecCommandSource::UserShell,
            start_time: None,
            duration: None,
            interaction_input: None,
        };

        let cell = ExecCell::new(call, /*animations_enabled*/ false);
        let rendered: Vec<String> = cell
            .command_display_lines(/*width*/ 36)
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect();

        assert_eq!(
            rendered.iter().filter(|line| line.contains(url)).count(),
            1,
            "expected full URL in one rendered line, got: {rendered:?}"
        );
    }

    #[test]
    fn active_command_without_animations_is_stable() {
        let call = ExecCall {
            call_id: "call-id".to_string(),
            command: vec!["bash".into(), "-lc".into(), "echo done".into()],
            parsed: Vec::new(),
            presentation: CommandPresentation::Command,
            output: None,
            source: ExecCommandSource::Agent,
            start_time: Some(Instant::now()),
            duration: None,
            interaction_input: None,
        };

        let cell = ExecCell::new(call, /*animations_enabled*/ false);
        let first: Vec<String> = cell
            .command_display_lines(/*width*/ 80)
            .iter()
            .map(render_line_text)
            .collect();
        let second: Vec<String> = cell
            .command_display_lines(/*width*/ 80)
            .iter()
            .map(render_line_text)
            .collect();

        assert_eq!(first, second);
        assert_eq!(first, vec!["• Running echo done".to_string()]);
    }

    #[test]
    fn completed_command_output_keeps_a_continuous_closed_gutter() {
        let call = ExecCall {
            call_id: "call-id".to_string(),
            command: vec!["bash".into(), "-lc".into(), "printf output".into()],
            parsed: Vec::new(),
            presentation: CommandPresentation::Command,
            output: Some(CommandOutput::new(0, "first\nsecond\nthird\n".to_string())),
            source: ExecCommandSource::Agent,
            start_time: Some(Instant::now()),
            duration: Some(std::time::Duration::from_millis(10)),
            interaction_input: None,
        };

        let lines = ExecCell::new(call, /*animations_enabled*/ false)
            .command_display_lines(/*width*/ 80);

        for line in &lines[1..] {
            let gutter = line
                .spans
                .first()
                .expect("output line should have a gutter");
            assert!(
                gutter.style.add_modifier.contains(Modifier::DIM),
                "output gutter should stay dimmed: {line:?}"
            );
        }

        let rendered = lines.iter().map(render_line_text).collect::<Vec<_>>();

        assert_eq!(
            rendered,
            vec![
                "• Ran printf output",
                "  │ first",
                "  │ second",
                "  └ third",
            ]
        );
    }

    #[test]
    fn completed_verbose_success_keeps_five_row_head_tail_preview() {
        let call = ExecCall {
            call_id: "call-id".to_string(),
            command: vec!["bash".into(), "-lc".into(), "inspect lots-of-output".into()],
            parsed: Vec::new(),
            presentation: CommandPresentation::Command,
            output: Some(CommandOutput::new(
                0,
                (1..=20).map(|line| format!("output {line}")).join("\n"),
            )),
            source: ExecCommandSource::Agent,
            start_time: None,
            duration: Some(std::time::Duration::from_millis(10)),
            interaction_input: None,
        };

        let cell = ExecCell::new(call, /*animations_enabled*/ false);
        let rendered = cell
            .command_display_lines(/*width*/ 80)
            .iter()
            .map(render_line_text)
            .collect::<Vec<_>>();

        assert!(
            rendered.len() > 2,
            "successful commands should retain useful output feedback: {rendered:?}"
        );
        assert!(rendered.iter().any(|line| line.contains("output 1")));
        assert!(rendered.iter().any(|line| line.contains("output 20")));
        assert!(rendered.iter().any(|line| line.contains("… +")));
        let transcript = cell
            .transcript_lines(/*width*/ 80)
            .iter()
            .map(render_line_text)
            .collect::<Vec<_>>();
        assert!(transcript.iter().any(|line| line == "output 1"));
        assert!(transcript.iter().any(|line| line == "output 20"));
    }

    #[test]
    fn completed_verbose_failure_keeps_diagnostic_output() {
        let call = ExecCall {
            call_id: "call-id".to_string(),
            command: vec!["bash".into(), "-lc".into(), "check failing-target".into()],
            parsed: Vec::new(),
            presentation: CommandPresentation::Command,
            output: Some(CommandOutput::new(
                2,
                (1..=20).map(|line| format!("error {line}")).join("\n"),
            )),
            source: ExecCommandSource::Agent,
            start_time: None,
            duration: Some(std::time::Duration::from_millis(10)),
            interaction_input: None,
        };

        let rendered = ExecCell::new(call, /*animations_enabled*/ false)
            .command_display_lines(/*width*/ 80)
            .iter()
            .map(render_line_text)
            .collect::<Vec<_>>();

        assert!(
            rendered.len() > 2,
            "failures should retain diagnostic excerpts: {rendered:?}"
        );
        assert_eq!(rendered[0], "• Failed (exit 2) check failing-target");
        assert!(rendered.iter().any(|line| line.contains("error 1")));
        assert!(rendered.iter().any(|line| line.contains("error 20")));
        assert!(rendered.iter().any(|line| line.contains("… +")));
    }

    #[test]
    fn completed_verbose_success_keeps_warning_output() {
        let mut output = vec!["warning: generated chunk exceeds the configured size".to_string()];
        output.extend((2..=20).map(|line| format!("build output {line}")));
        let call = ExecCall {
            call_id: "call-id".to_string(),
            command: vec!["bash".into(), "-lc".into(), "build app".into()],
            parsed: Vec::new(),
            presentation: CommandPresentation::Command,
            output: Some(CommandOutput::new(0, output.join("\n"))),
            source: ExecCommandSource::Agent,
            start_time: None,
            duration: Some(std::time::Duration::from_millis(10)),
            interaction_input: None,
        };

        let rendered = ExecCell::new(call, /*animations_enabled*/ false)
            .command_display_lines(/*width*/ 80)
            .iter()
            .map(render_line_text)
            .collect::<Vec<_>>();

        assert!(
            rendered.len() > 2,
            "successful warnings should remain expanded: {rendered:?}"
        );
        assert!(
            rendered
                .iter()
                .any(|line| line.contains("warning: generated chunk"))
        );
        assert!(rendered[0].contains("Ran with warnings"));
    }

    #[test]
    fn wide_agent_command_uses_bounded_preview_with_plain_ellipsis() {
        let long_argument = "very-long-inspection-argument-".repeat(10);
        let command = format!("inspect {long_argument}");
        let call = ExecCall {
            call_id: "call-id".to_string(),
            command: vec!["bash".into(), "-lc".into(), command.clone()],
            parsed: Vec::new(),
            presentation: CommandPresentation::Command,
            output: Some(CommandOutput::new(0, String::new())),
            source: ExecCommandSource::Agent,
            start_time: None,
            duration: Some(std::time::Duration::from_millis(10)),
            interaction_input: None,
        };

        let cell = ExecCell::new(call, /*animations_enabled*/ false);
        let display = cell.command_display_lines(/*width*/ 220);
        let header = display.first().expect("command header");
        let header_text = render_line_text(header);

        assert!(line_width(header) <= AGENT_COMMAND_PREVIEW_MAX_WIDTH);
        assert!(header_text.ends_with('…'));
        assert!(!header_text.contains("transcript"));
        assert!(!header_text.contains(&command));
        assert!(
            cell.raw_lines()
                .iter()
                .map(render_line_text)
                .any(|line| line.contains(&command))
        );
    }

    #[test]
    fn completed_command_without_output_closes_the_gutter() {
        let call = ExecCall {
            call_id: "call-id".to_string(),
            command: vec!["bash".into(), "-lc".into(), "true".into()],
            parsed: Vec::new(),
            presentation: CommandPresentation::Command,
            output: Some(CommandOutput::new(0, String::new())),
            source: ExecCommandSource::Agent,
            start_time: Some(Instant::now()),
            duration: Some(std::time::Duration::from_millis(10)),
            interaction_input: None,
        };

        let rendered = ExecCell::new(call, /*animations_enabled*/ false)
            .command_display_lines(/*width*/ 80)
            .iter()
            .map(render_line_text)
            .collect::<Vec<_>>();

        assert_eq!(rendered, vec!["• Ran true", "  └ (no output)"]);
    }

    #[test]
    fn completed_ran_title_is_not_dimmed() {
        let call = ExecCall {
            call_id: "solid-title".to_string(),
            command: vec!["make".into(), "fmt".into()],
            parsed: Vec::new(),
            presentation: CommandPresentation::Command,
            output: Some(CommandOutput::new(0, String::new())),
            source: ExecCommandSource::Agent,
            start_time: Some(Instant::now()),
            duration: Some(std::time::Duration::from_millis(10)),
            interaction_input: None,
        };

        let lines = ExecCell::new(call, /*animations_enabled*/ false)
            .command_display_lines(/*width*/ 80);
        assert!(
            !lines[0].spans[2].style.add_modifier.contains(Modifier::DIM),
            "a completed Ran title should remain solid"
        );
    }

    #[test]
    fn exploring_display_does_not_split_long_url_like_search_query() {
        let url_like = "example.test/api/v1/projects/alpha-team/releases/2026-02-17/builds/1234567890/artifacts/reports/performance/summary/detail/with/a/very/long/path";
        let call = ExecCall {
            call_id: "call-id".to_string(),
            command: vec!["bash".into(), "-lc".into(), "rg foo".into()],
            parsed: vec![ParsedCommand::Search {
                cmd: format!("rg {url_like}"),
                query: Some(url_like.to_string()),
                path: None,
            }],
            presentation: CommandPresentation::Exploration,
            output: None,
            source: ExecCommandSource::Agent,
            start_time: None,
            duration: None,
            interaction_input: None,
        };

        let cell = ExecCell::new(call, /*animations_enabled*/ false);
        let rendered: Vec<String> = cell
            .display_lines(/*width*/ 36)
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect();

        assert_eq!(rendered.len(), 2);
        assert!(
            rendered[1].ends_with('…'),
            "exploration rows should stay single-line and truncate cleanly: {rendered:?}"
        );
    }

    #[test]
    fn exploring_display_caps_and_deduplicates_operation_rows() {
        let call = ExecCall {
            call_id: "search-1".to_string(),
            command: vec!["rg".into(), "one".into(), "src".into()],
            parsed: vec![ParsedCommand::Search {
                cmd: "rg one src".to_string(),
                query: Some("one".to_string()),
                path: Some("src".to_string()),
            }],
            presentation: CommandPresentation::Exploration,
            output: None,
            source: ExecCommandSource::Agent,
            start_time: Some(Instant::now()),
            duration: None,
            interaction_input: None,
        };
        let mut cell = ExecCell::new(call, /*animations_enabled*/ false);
        assert!(cell.add_call(
            "search-duplicate".to_string(),
            vec!["rg".into(), "one".into(), "src".into()],
            vec![ParsedCommand::Search {
                cmd: "rg one src".to_string(),
                query: Some("one".to_string()),
                path: Some("src".to_string()),
            }],
            CommandPresentation::Exploration,
            ExecCommandSource::Agent,
            /*interaction_input*/ None,
        ));
        for (id, query) in [
            ("search-2", "two"),
            ("search-3", "three"),
            ("search-4", "four"),
            ("search-5", "five"),
            ("search-6", "six"),
        ] {
            assert!(cell.add_call(
                id.to_string(),
                vec!["rg".into(), query.into(), "src".into()],
                vec![ParsedCommand::Search {
                    cmd: format!("rg {query} src"),
                    query: Some(query.to_string()),
                    path: Some("src".to_string()),
                }],
                CommandPresentation::Exploration,
                ExecCommandSource::Agent,
                /*interaction_input*/ None,
            ));
        }

        let rendered = cell
            .display_lines(/*width*/ 80)
            .iter()
            .map(render_line_text)
            .collect::<Vec<_>>();

        assert_eq!(rendered.len(), 6);
        assert_eq!(rendered[0], "• Exploring · 7 active · 0 done");
        assert_eq!(
            rendered
                .iter()
                .filter(|line| line.contains("Searching one in src ×2"))
                .count(),
            1
        );
        assert!(
            rendered
                .last()
                .is_some_and(|line| line.contains("+2 active · 2 searches"))
        );
    }

    #[test]
    fn exploration_prioritizes_active_rows_and_reports_exact_output_totals() {
        let read_call = ExecCall {
            call_id: "read".to_string(),
            command: vec!["cat".into(), "engine.ts".into()],
            parsed: vec![ParsedCommand::Read {
                cmd: "cat engine.ts".to_string(),
                name: "engine.ts".to_string(),
                path: "src/engine.ts".into(),
            }],
            presentation: CommandPresentation::Exploration,
            output: None,
            source: ExecCommandSource::Agent,
            start_time: Some(Instant::now()),
            duration: None,
            interaction_input: None,
        };
        let mut cell = ExecCell::new(read_call, /*animations_enabled*/ false);
        assert!(cell.complete_call(
            "read",
            CommandOutput::new(0, "alpha\nβeta\n".to_string()),
            std::time::Duration::from_millis(10),
        ));
        assert!(cell.add_call(
            "search".to_string(),
            vec!["rg".into(), "needle".into(), "src".into()],
            vec![ParsedCommand::Search {
                cmd: "rg needle src".to_string(),
                query: Some("needle".to_string()),
                path: Some("src".to_string()),
            }],
            CommandPresentation::Exploration,
            ExecCommandSource::Agent,
            /*interaction_input*/ None,
        ));
        assert!(cell.append_output("search", "one\n"));
        assert!(cell.append_output("search", "two"));

        let rendered = cell
            .display_lines(/*width*/ 100)
            .iter()
            .map(render_line_text)
            .collect::<Vec<_>>();

        assert_eq!(
            rendered,
            vec![
                "• Exploring · 1 active · 1 done",
                "  ├ Searching needle in src · 2 result lines",
                "  └ Read engine.ts · 2 lines · 11 chars",
            ]
        );
    }

    #[test]
    fn completed_exploration_aggregates_repeated_reads_and_totals() {
        let read_call = ExecCall {
            call_id: "read-1".to_string(),
            command: vec!["cat".into(), "engine.ts".into()],
            parsed: vec![ParsedCommand::Read {
                cmd: "cat engine.ts".to_string(),
                name: "engine.ts".to_string(),
                path: "src/engine.ts".into(),
            }],
            presentation: CommandPresentation::Exploration,
            output: None,
            source: ExecCommandSource::Agent,
            start_time: Some(Instant::now()),
            duration: None,
            interaction_input: None,
        };
        let mut cell = ExecCell::new(read_call, /*animations_enabled*/ false);
        assert!(cell.complete_call(
            "read-1",
            CommandOutput::new(0, "a\n".to_string()),
            std::time::Duration::from_millis(10),
        ));
        assert!(cell.add_call(
            "read-2".to_string(),
            vec!["cat".into(), "engine.ts".into()],
            vec![ParsedCommand::Read {
                cmd: "cat engine.ts".to_string(),
                name: "engine.ts".to_string(),
                path: "src/engine.ts".into(),
            }],
            CommandPresentation::Exploration,
            ExecCommandSource::Agent,
            /*interaction_input*/ None,
        ));
        assert!(cell.complete_call(
            "read-2",
            CommandOutput::new(0, "b\n".to_string()),
            std::time::Duration::from_millis(10),
        ));

        let rendered = cell
            .display_lines(/*width*/ 100)
            .iter()
            .map(render_line_text)
            .collect::<Vec<_>>();

        assert_eq!(
            rendered,
            vec![
                "• Explored · 1 file · 2 operations",
                "  └ Read engine.ts ×2 · 2 lines · 4 chars",
            ]
        );
    }

    #[test]
    fn completed_search_does_not_report_zero_files() {
        let call = ExecCall {
            call_id: "search".to_string(),
            command: vec!["rg".into(), "needle".into(), "src".into()],
            parsed: vec![ParsedCommand::Search {
                cmd: "rg needle src".to_string(),
                query: Some("needle".to_string()),
                path: Some("src".to_string()),
            }],
            presentation: CommandPresentation::Exploration,
            output: Some(CommandOutput::new(0, "src/lib.rs:1:needle\n".to_string())),
            source: ExecCommandSource::Agent,
            start_time: None,
            duration: Some(std::time::Duration::from_millis(10)),
            interaction_input: None,
        };

        let rendered = ExecCell::new(call, /*animations_enabled*/ false)
            .display_lines(/*width*/ 100)
            .iter()
            .map(render_line_text)
            .collect::<Vec<_>>();

        assert_eq!(rendered[0], "• Explored · 1 operation");
    }

    #[test]
    fn interrupted_exploration_reports_failure_without_merging_successful_rows() {
        let first = ExecCall {
            call_id: "completed".to_string(),
            command: vec!["rg".into(), "needle".into(), "src".into()],
            parsed: vec![ParsedCommand::Search {
                cmd: "rg needle src".to_string(),
                query: Some("needle".to_string()),
                path: Some("src".to_string()),
            }],
            presentation: CommandPresentation::Exploration,
            output: None,
            source: ExecCommandSource::Agent,
            start_time: Some(Instant::now()),
            duration: None,
            interaction_input: None,
        };
        let mut cell = ExecCell::new(first, /*animations_enabled*/ false);
        assert!(cell.complete_call(
            "completed",
            CommandOutput::new(0, "src/one.rs:1:needle\n".to_string()),
            std::time::Duration::from_millis(10),
        ));
        assert!(cell.add_call(
            "interrupted".to_string(),
            vec!["rg".into(), "needle".into(), "src".into()],
            vec![ParsedCommand::Search {
                cmd: "rg needle src".to_string(),
                query: Some("needle".to_string()),
                path: Some("src".to_string()),
            }],
            CommandPresentation::Exploration,
            ExecCommandSource::Agent,
            /*interaction_input*/ None,
        ));
        assert!(cell.append_output("interrupted", "src/two.rs:2:needle\n"));

        cell.mark_failed();
        let rendered = cell
            .display_lines(/*width*/ 100)
            .iter()
            .map(render_line_text)
            .collect::<Vec<_>>();

        assert_eq!(
            rendered,
            vec![
                "• Exploration failed · 1 failed · 1 done",
                "  ├ Failed searching needle in src · 1 result line",
                "  └ Searched needle in src · 1 result line",
            ]
        );
    }

    #[test]
    fn exploration_card_sanitizes_details_and_respects_narrow_widths() {
        let call = ExecCall {
            call_id: "search".to_string(),
            command: vec!["rg".into(), "needle".into(), "src".into()],
            parsed: vec![ParsedCommand::Search {
                cmd: "rg needle src".to_string(),
                query: Some("\u{1b}[31mneedle\nnext\u{1b}[0m".to_string()),
                path: Some("src\tmodule".to_string()),
            }],
            presentation: CommandPresentation::Exploration,
            output: None,
            source: ExecCommandSource::Agent,
            start_time: Some(Instant::now()),
            duration: None,
            interaction_input: None,
        };
        let cell = ExecCell::new(call, /*animations_enabled*/ false);

        for width in 1..=40 {
            for line in cell.display_lines(width) {
                let rendered = render_line_text(&line);
                assert!(
                    line_width(&line) <= usize::from(width),
                    "line exceeded width {width}: {rendered:?}"
                );
                assert!(
                    !rendered
                        .chars()
                        .any(|ch| matches!(ch, '\n' | '\r' | '\t' | '\u{1b}')),
                    "control text escaped the compact row: {rendered:?}"
                );
            }
        }

        let rendered = cell
            .display_lines(/*width*/ 40)
            .iter()
            .map(render_line_text)
            .collect::<Vec<_>>();
        assert!(rendered[1].contains("needle next"));
        assert!(rendered[1].contains("in src module"));
    }

    #[test]
    fn interrupted_parallel_commands_render_as_failed_not_running() {
        let first = ExecCall {
            call_id: "first".to_string(),
            command: vec!["echo".into(), "first".into()],
            parsed: Vec::new(),
            presentation: CommandPresentation::Command,
            output: None,
            source: ExecCommandSource::Agent,
            start_time: Some(Instant::now()),
            duration: None,
            interaction_input: None,
        };
        let mut cell = ExecCell::new(first, /*animations_enabled*/ false);
        assert!(cell.add_call(
            "second".to_string(),
            vec!["echo".into(), "second".into()],
            Vec::new(),
            CommandPresentation::Command,
            ExecCommandSource::Agent,
            /*interaction_input*/ None,
        ));

        cell.mark_failed();
        let rendered = cell
            .display_lines(/*width*/ 100)
            .iter()
            .map(render_line_text)
            .collect::<Vec<_>>();

        assert_eq!(rendered[0], "• Failed 2 commands");
        assert!(rendered.iter().all(|line| !line.contains("Running")));
    }

    #[test]
    fn exploration_truncates_query_before_context_and_stats() {
        let query = "very-long-query-token-".repeat(8);
        let call = ExecCall {
            call_id: "search".to_string(),
            command: vec!["rg".into(), query.clone(), "src/preserved/path".into()],
            parsed: vec![ParsedCommand::Search {
                cmd: format!("rg {query} src/preserved/path"),
                query: Some(query.clone()),
                path: Some("src/preserved/path".to_string()),
            }],
            presentation: CommandPresentation::Exploration,
            output: Some(CommandOutput::new(0, "one\ntwo\n".to_string())),
            source: ExecCommandSource::Agent,
            start_time: Some(Instant::now()),
            duration: None,
            interaction_input: None,
        };

        let lines =
            ExecCell::new(call, /*animations_enabled*/ false).display_lines(/*width*/ 72);
        let detail = render_line_text(&lines[1]);

        assert!(line_width(&lines[1]) <= 72);
        assert!(detail.contains('…'));
        assert!(!detail.contains(&query));
        assert!(detail.ends_with(" in src/preserved/path · 2 result lines"));
    }

    #[test]
    fn exploration_preserves_multi_search_count_and_stats_after_truncation() {
        let first_query = concat!(
            "createdAt\\s*[:=][^\\r\\n]*\\+\\s*1\\b|",
            "created_at\\s*[:=][^\\r\\n]*\\+\\s*1\\b|",
            "\\.createdAt"
        );
        let call = ExecCall {
            call_id: "search".to_string(),
            command: vec!["rg".into(), first_query.into(), "extension".into()],
            parsed: vec![
                ParsedCommand::Search {
                    cmd: format!("rg {first_query} extension"),
                    query: Some(first_query.to_string()),
                    path: Some("extension".to_string()),
                },
                ParsedCommand::Search {
                    cmd: "rg projectComposterAccelerated extension/tests".to_string(),
                    query: Some("projectComposterAccelerated".to_string()),
                    path: Some("extension/tests".to_string()),
                },
            ],
            presentation: CommandPresentation::Exploration,
            output: Some(CommandOutput::new(0, (0..23).map(|_| "match\n").collect())),
            source: ExecCommandSource::Agent,
            start_time: Some(Instant::now()),
            duration: Some(std::time::Duration::from_millis(10)),
            interaction_input: None,
        };

        let lines =
            ExecCell::new(call, /*animations_enabled*/ false).display_lines(/*width*/ 84);
        let detail = render_line_text(&lines[1]);

        assert!(line_width(&lines[1]) <= 84);
        assert!(detail.contains('…'));
        assert!(detail.contains(" +1 search"));
        assert!(detail.ends_with(" · 23 result lines"));
    }

    #[test]
    fn semantic_inspection_hides_the_pipeline_and_adapts_preview_depth() {
        let command = vec![
            "powershell.exe".into(),
            "-Command".into(),
            "$har=Get-Content capture.json -Raw|ConvertFrom-Json;$har.actions".into(),
        ];
        let clean = ExecCall {
            call_id: "clean".to_string(),
            command: command.clone(),
            parsed: vec![ParsedCommand::Read {
                cmd: "Get-Content capture.json".to_string(),
                name: "capture.json".to_string(),
                path: "capture.json".into(),
            }],
            presentation: CommandPresentation::Inspection {
                target: "capture.json".to_string(),
            },
            output: Some(CommandOutput::new(
                0,
                (1..=20).map(|line| format!("output {line}")).join("\n"),
            )),
            source: ExecCommandSource::Agent,
            start_time: None,
            duration: Some(std::time::Duration::from_millis(10)),
            interaction_input: None,
        };
        let clean_rendered = ExecCell::new(clean, /*animations_enabled*/ false)
            .display_lines(/*width*/ 100)
            .iter()
            .map(render_line_text)
            .collect::<Vec<_>>();
        assert_eq!(clean_rendered[0], "• Inspected capture.json");
        assert!(clean_rendered.len() <= 4);
        assert!(clean_rendered.iter().any(|line| line.contains("… +")));
        assert!(
            clean_rendered
                .iter()
                .all(|line| !line.contains("ConvertFrom-Json"))
        );

        let warning = ExecCall {
            call_id: "warning".to_string(),
            command: command.clone(),
            parsed: Vec::new(),
            presentation: CommandPresentation::Inspection {
                target: "capture.json".to_string(),
            },
            output: Some(CommandOutput::new(
                0,
                std::iter::once("warning: partial capture".to_string())
                    .chain((2..=20).map(|line| format!("output {line}")))
                    .join("\n"),
            )),
            source: ExecCommandSource::Agent,
            start_time: None,
            duration: Some(std::time::Duration::from_millis(10)),
            interaction_input: None,
        };
        let warning_rendered = ExecCell::new(warning, /*animations_enabled*/ false)
            .display_lines(/*width*/ 100)
            .iter()
            .map(render_line_text)
            .collect::<Vec<_>>();
        assert_eq!(warning_rendered[0], "• Inspected capture.json · warnings");
        assert!(warning_rendered.len() <= 6);

        let failed = ExecCall {
            call_id: "failed".to_string(),
            command,
            parsed: Vec::new(),
            presentation: CommandPresentation::Inspection {
                target: "capture.json".to_string(),
            },
            output: Some(CommandOutput::new(2, "error: invalid JSON".to_string())),
            source: ExecCommandSource::Agent,
            start_time: None,
            duration: Some(std::time::Duration::from_millis(10)),
            interaction_input: None,
        };
        let failed_rendered = ExecCell::new(failed, /*animations_enabled*/ false)
            .display_lines(/*width*/ 100)
            .iter()
            .map(render_line_text)
            .collect::<Vec<_>>();
        assert_eq!(
            failed_rendered[0],
            "• Inspection failed (exit 2) capture.json"
        );
    }

    #[test]
    fn output_display_does_not_split_long_url_like_token_without_scheme() {
        let url = "example.test/api/v1/projects/alpha-team/releases/2026-02-17/builds/1234567890/artifacts/reports/performance/summary/detail/session_id=abc123def456ghi789jkl012mno345pqr678";

        let call = ExecCall {
            call_id: "call-id".to_string(),
            command: vec!["bash".into(), "-lc".into(), "echo done".into()],
            parsed: Vec::new(),
            presentation: CommandPresentation::Command,
            output: Some(CommandOutput::new(/*exit_code*/ 0, url.to_string())),
            source: ExecCommandSource::UserShell,
            start_time: None,
            duration: None,
            interaction_input: None,
        };

        let cell = ExecCell::new(call, /*animations_enabled*/ false);
        let rendered: Vec<String> = cell
            .command_display_lines(/*width*/ 36)
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect();

        assert_eq!(
            rendered.iter().filter(|line| line.contains(url)).count(),
            1,
            "expected full URL-like token in one rendered line, got: {rendered:?}"
        );
    }

    #[test]
    fn desired_transcript_height_accounts_for_wrapped_url_like_rows() {
        let url = "https://example.test/api/v1/projects/alpha-team/releases/2026-02-17/builds/1234567890/artifacts/reports/performance/summary/detail/with/a/very/long/path/that/keeps/going/for/testing/purposes";
        let call = ExecCall {
            call_id: "call-id".to_string(),
            command: vec!["bash".into(), "-lc".into(), "echo done".into()],
            parsed: Vec::new(),
            presentation: CommandPresentation::Command,
            output: Some(CommandOutput::new(/*exit_code*/ 0, url.to_string())),
            source: ExecCommandSource::Agent,
            start_time: None,
            duration: None,
            interaction_input: None,
        };

        let cell = ExecCell::new(call, /*animations_enabled*/ false);
        let width: u16 = 36;
        let logical_height = cell.transcript_lines(width).len() as u16;
        let wrapped_height = cell.desired_transcript_height(width);

        assert!(
            wrapped_height > logical_height,
            "expected transcript height to account for wrapped URL-like rows, logical_height={logical_height}, wrapped_height={wrapped_height}"
        );
    }
}
