//! Compact display helpers for individual and grouped MCP calls.

use super::*;
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Copy)]
pub(super) enum GroupedCallRow {
    Call(usize),
    Omitted(usize),
}

pub(super) fn grouped_call_indices(call_count: usize, max_rows: usize) -> Vec<GroupedCallRow> {
    if call_count <= max_rows {
        return (0..call_count).map(GroupedCallRow::Call).collect();
    }
    let retained = max_rows.saturating_sub(1);
    let head = retained.div_ceil(2);
    let tail = retained - head;
    let mut rows = (0..head).map(GroupedCallRow::Call).collect::<Vec<_>>();
    rows.push(GroupedCallRow::Omitted(call_count - retained));
    rows.extend((call_count - tail..call_count).map(GroupedCallRow::Call));
    rows
}

pub(super) fn grouped_call_line(call: &McpToolCall, width: u16, prefix: &str) -> Line<'static> {
    let state = grouped_call_state(call);
    let argument = invocation_argument_summary(&call.invocation);
    let reserved = UnicodeWidthStr::width(prefix)
        + UnicodeWidthStr::width(state.as_str())
        + usize::from(!argument.is_empty()) * UnicodeWidthStr::width(" · ");
    let argument_budget = usize::from(width)
        .saturating_sub(reserved)
        .min(MCP_GROUP_ARGUMENT_MAX_WIDTH);
    let argument = truncate_plain_text(&argument, argument_budget);
    let mut line = Line::from(prefix.to_string().dim());
    if !argument.is_empty() {
        line.push_span(argument);
        line.push_span(" · ".dim());
    }
    let state_span = match call.success() {
        None => state.cyan(),
        Some(false) => state.red(),
        Some(true) => state.dim(),
    };
    line.push_span(state_span);
    line
}

fn grouped_call_state(call: &McpToolCall) -> String {
    match call.result.as_ref() {
        None => "running".to_string(),
        Some(Err(error)) => {
            let first = error.lines().next().unwrap_or("failed").trim();
            format!("failed: {}", truncate_plain_text(first, 60))
        }
        Some(Ok(_)) => {
            let stats = call.output_stats();
            let mut parts = Vec::new();
            if stats.lines > 1 {
                parts.push(format!("{} lines", stats.lines));
            }
            if stats.characters > 0 {
                parts.push(format!(
                    "{} chars",
                    crate::status::format_tokens_compact(stats.characters as i64)
                ));
            }
            if parts.is_empty() {
                parts.push("no output".to_string());
            }
            if let Some(duration) = call.duration {
                parts.push(format_duration(duration));
            }
            parts.join(" · ")
        }
    }
}

fn truncate_plain_text(text: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    let (prefix, suffix, _) = take_prefix_by_width(text, max_width.saturating_sub(1));
    if suffix.is_empty() {
        text.to_string()
    } else {
        format!("{prefix}…")
    }
}

pub(super) fn mcp_call_detail_lines(call: &McpToolCall, width: usize) -> Vec<Line<'static>> {
    let Some(result) = call.result.as_ref() else {
        return Vec::new();
    };
    let mut lines = Vec::new();
    for source in mcp_result_sources(result) {
        for raw in raw_lines_from_source(&source) {
            let text = raw
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>();
            let parsed = ansi_escape_line(&text).dim();
            lines.extend(
                adaptive_wrap_line(
                    &parsed,
                    RtOptions::new(width)
                        .initial_indent("".into())
                        .subsequent_indent("    ".into()),
                )
                .iter()
                .map(line_to_static),
            );
        }
    }
    truncate_mcp_detail_lines(lines, MCP_DETAIL_MAX_ROWS)
}

fn truncate_mcp_detail_lines(lines: Vec<Line<'static>>, max_rows: usize) -> Vec<Line<'static>> {
    if lines.len() <= max_rows {
        return lines;
    }
    if max_rows == 0 {
        return Vec::new();
    }
    if max_rows == 1 {
        return vec![Line::from(format!("… +{} lines", lines.len()).dim())];
    }
    let retained = max_rows - 1;
    let head = retained.div_ceil(2);
    let tail = retained - head;
    let omitted = lines.len() - retained;
    let mut out = lines[..head].to_vec();
    out.push(Line::from(format!("… +{omitted} lines").dim()));
    out.extend_from_slice(&lines[lines.len() - tail..]);
    out
}

pub(super) fn mcp_status_bullet(
    status: Option<bool>,
    start_time: Instant,
    animations_enabled: bool,
) -> Span<'static> {
    match status {
        Some(true) => "•".green().bold(),
        Some(false) => "•".red().bold(),
        None => activity_indicator(
            Some(start_time),
            MotionMode::from_animations_enabled(animations_enabled),
            ReducedMotionIndicator::StaticBullet,
        )
        .unwrap_or_else(|| "•".dim()),
    }
}
