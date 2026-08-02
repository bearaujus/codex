//! Lifecycle-aware MCP tool-call history cells.
//!
//! Consecutive calls to the same `server.tool` stay in one compact card while
//! the transcript retains each invocation and its complete output.

use super::*;

use codex_ansi_escape::ansi_escape_line;
use codex_utils_elapsed::format_duration;
use unicode_width::UnicodeWidthStr;

use super::mcp_tool_call_args::format_mcp_invocation;
use super::mcp_tool_call_args::invocation_argument_summary;
use super::mcp_tool_call_output::McpOutputStats;
use super::mcp_tool_call_output::image_output_cell;
use super::mcp_tool_call_output::mcp_result_sources;
use crate::line_truncation::truncate_line_with_ellipsis_if_overflow;

#[path = "mcp_tool_call/render.rs"]
mod render;
use render::GroupedCallRow;
use render::grouped_call_indices;
use render::grouped_call_line;
use render::mcp_call_detail_lines;
use render::mcp_status_bullet;

const MCP_DETAIL_MAX_ROWS: usize = 5;
const MCP_GROUP_MAX_ROWS: usize = 5;
const MCP_GROUP_ARGUMENT_MAX_WIDTH: usize = 76;

#[derive(Debug)]
struct McpToolCall {
    call_id: String,
    invocation: McpInvocation,
    start_time: Instant,
    duration: Option<Duration>,
    result: Option<Result<codex_protocol::mcp::CallToolResult, String>>,
}

impl McpToolCall {
    fn success(&self) -> Option<bool> {
        match self.result.as_ref() {
            Some(Ok(result)) => Some(!result.is_error.unwrap_or(false)),
            Some(Err(_)) => Some(false),
            None => None,
        }
    }

    fn output_stats(&self) -> McpOutputStats {
        self.result
            .as_ref()
            .map(mcp_result_sources)
            .map(|sources| McpOutputStats::from_sources(&sources))
            .unwrap_or_default()
    }
}

#[derive(Debug)]
pub(crate) struct McpToolCallCell {
    calls: Vec<McpToolCall>,
    animations_enabled: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct McpInvocation {
    pub(crate) server: String,
    pub(crate) tool: String,
    pub(crate) arguments: Option<serde_json::Value>,
}

impl McpToolCallCell {
    pub(crate) fn new(
        call_id: String,
        invocation: McpInvocation,
        animations_enabled: bool,
    ) -> Self {
        Self {
            calls: vec![McpToolCall {
                call_id,
                invocation,
                start_time: Instant::now(),
                duration: None,
                result: None,
            }],
            animations_enabled,
        }
    }

    pub(crate) fn call_id(&self) -> &str {
        self.calls
            .last()
            .map(|call| call.call_id.as_str())
            .unwrap_or_default()
    }

    pub(crate) fn tracks_call(&self, call_id: &str) -> bool {
        self.calls.iter().any(|call| call.call_id == call_id)
    }

    pub(crate) fn add_call(&mut self, call_id: String, invocation: McpInvocation) -> bool {
        let Some(first) = self.calls.first() else {
            return false;
        };
        if first.invocation.server != invocation.server || first.invocation.tool != invocation.tool
        {
            return false;
        }
        self.calls.push(McpToolCall {
            call_id,
            invocation,
            start_time: Instant::now(),
            duration: None,
            result: None,
        });
        true
    }

    pub(crate) fn complete(
        &mut self,
        duration: Duration,
        result: Result<codex_protocol::mcp::CallToolResult, String>,
    ) -> Option<Box<dyn HistoryCell>> {
        let call_id = self.call_id().to_string();
        self.complete_call(&call_id, duration, result)
    }

    pub(crate) fn complete_call(
        &mut self,
        call_id: &str,
        duration: Duration,
        result: Result<codex_protocol::mcp::CallToolResult, String>,
    ) -> Option<Box<dyn HistoryCell>> {
        let call = self
            .calls
            .iter_mut()
            .rev()
            .find(|call| call.call_id == call_id)?;
        let image_cell = image_output_cell(&result);
        call.duration = Some(duration);
        call.result = Some(result);
        image_cell
    }

    pub(crate) fn has_hidden_transcript_detail(&self) -> bool {
        let grouped_output_is_summarized = self.calls.len() > 1
            && self
                .calls
                .iter()
                .any(|call| call.output_stats().characters > 0);
        let grouped_arguments_are_truncated = self.calls.len() > 1
            && self.calls.iter().any(|call| {
                UnicodeWidthStr::width(invocation_argument_summary(&call.invocation).as_str())
                    > MCP_GROUP_ARGUMENT_MAX_WIDTH
            });
        self.calls.len() > MCP_GROUP_MAX_ROWS
            || grouped_output_is_summarized
            || grouped_arguments_are_truncated
            || self.calls.iter().any(|call| {
                let stats = call.output_stats();
                stats.lines > MCP_DETAIL_MAX_ROWS || stats.characters > 160
            })
    }

    pub(crate) fn mark_failed(&mut self) {
        for call in self.calls.iter_mut().filter(|call| call.duration.is_none()) {
            call.duration = Some(call.start_time.elapsed());
            call.result = Some(Err("interrupted".to_string()));
        }
    }

    fn display_single_call(&self, call: &McpToolCall, width: u16) -> Vec<Line<'static>> {
        let status = call.success();
        let bullet = mcp_status_bullet(status, call.start_time, self.animations_enabled);
        let header_text = match status {
            None => "Calling",
            Some(true) => "Called",
            Some(false) => "Call failed",
        };

        let invocation_line = line_to_static(&format_mcp_invocation(call.invocation.clone()));
        let mut compact_spans = vec![bullet, " ".into(), header_text.bold(), " ".into()];
        let mut compact_header = Line::from(compact_spans.clone());
        let reserved = compact_header.width();
        let inline_invocation =
            invocation_line.width() <= usize::from(width).saturating_sub(reserved);
        let mut lines = Vec::new();

        if inline_invocation {
            compact_header.extend(invocation_line.spans);
            lines.push(compact_header);
        } else {
            compact_spans.pop();
            lines.push(Line::from(compact_spans));
            let wrap_width = usize::from(width).saturating_sub(4).max(1);
            let wrapped = adaptive_wrap_line(
                &invocation_line,
                RtOptions::new(wrap_width)
                    .initial_indent("".into())
                    .subsequent_indent("    ".into()),
            );
            lines.extend(prefix_lines(
                wrapped.iter().map(line_to_static).collect(),
                "  └ ".dim(),
                "    ".into(),
            ));
        }

        let detail_width = usize::from(width).saturating_sub(4).max(1);
        let detail_lines = mcp_call_detail_lines(call, detail_width);
        if !detail_lines.is_empty() {
            let initial_prefix = if inline_invocation {
                "  └ ".dim()
            } else {
                "    ".into()
            };
            lines.extend(prefix_lines(detail_lines, initial_prefix, "    ".into()));
        }
        lines
    }

    fn display_group(&self, width: u16) -> Vec<Line<'static>> {
        let active = self
            .calls
            .iter()
            .filter(|call| call.success().is_none())
            .count();
        let failed = self
            .calls
            .iter()
            .filter(|call| matches!(call.success(), Some(false)))
            .count();
        let status = if active > 0 { None } else { Some(failed == 0) };
        let start_time = self
            .calls
            .iter()
            .find(|call| call.success().is_none())
            .map(|call| call.start_time)
            .unwrap_or_else(Instant::now);
        let invocation = &self.calls[0].invocation;
        let mut header = Line::from(vec![
            mcp_status_bullet(status, start_time, self.animations_enabled),
            " ".into(),
            if active > 0 {
                "Calling".bold()
            } else {
                "Called".bold()
            },
            " ".into(),
            invocation.server.clone().cyan(),
            ".".into(),
            invocation.tool.clone().cyan(),
            format!(" · {} calls", self.calls.len()).dim(),
        ]);
        if active > 0 {
            header.push_span(format!(" · {active} running").cyan().bold());
        }
        if failed > 0 {
            header.push_span(format!(" · {failed} failed").red().bold());
        }
        let mut lines = vec![truncate_line_with_ellipsis_if_overflow(
            header,
            usize::from(width),
        )];

        let rows = grouped_call_indices(self.calls.len(), MCP_GROUP_MAX_ROWS);
        for (row_index, row) in rows.iter().enumerate() {
            let is_last = row_index + 1 == rows.len();
            let prefix = if is_last { "  └ " } else { "  ├ " };
            let line = match row {
                GroupedCallRow::Call(index) => {
                    grouped_call_line(&self.calls[*index], width, prefix)
                }
                GroupedCallRow::Omitted(count) => {
                    Line::from(vec![prefix.dim(), format!("… +{count} calls").dim()])
                }
            };
            lines.push(line);
        }
        lines
    }

    fn full_transcript_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        for (index, call) in self.calls.iter().enumerate() {
            if index > 0 {
                lines.push(Line::default());
            }
            let invocation = format_mcp_invocation(call.invocation.clone());
            lines.extend(
                adaptive_wrap_line(
                    &invocation,
                    RtOptions::new(usize::from(width).max(1))
                        .initial_indent("$ ".magenta().into())
                        .subsequent_indent("    ".into()),
                )
                .iter()
                .map(line_to_static),
            );

            if let Some(result) = &call.result {
                for source in mcp_result_sources(result) {
                    for raw in raw_lines_from_source(&source) {
                        let parsed = ansi_escape_line(
                            &raw.spans
                                .iter()
                                .map(|span| span.content.as_ref())
                                .collect::<String>(),
                        );
                        lines.extend(
                            adaptive_wrap_line(&parsed, RtOptions::new(usize::from(width).max(1)))
                                .iter()
                                .map(line_to_static),
                        );
                    }
                }
                let mut result_line = match call.success() {
                    Some(true) => Line::from("✓".green().bold()),
                    Some(false) => Line::from("✗".red().bold()),
                    None => Line::from("…".dim()),
                };
                if let Some(duration) = call.duration {
                    result_line.push_span(format!(" · {}", format_duration(duration)).dim());
                }
                lines.push(result_line);
            }
        }
        lines
    }
}

impl HistoryCell for McpToolCallCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        match self.calls.as_slice() {
            [] => Vec::new(),
            [call] => self.display_single_call(call, width),
            _ => self.display_group(width),
        }
    }

    fn transcript_lines(&self, width: u16) -> Vec<Line<'static>> {
        self.full_transcript_lines(width)
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        plain_lines(self.full_transcript_lines(u16::MAX))
    }

    fn transcript_animation_tick(&self) -> Option<u64> {
        if !self.animations_enabled {
            return None;
        }
        self.calls
            .iter()
            .find(|call| call.duration.is_none())
            .map(|call| (call.start_time.elapsed().as_millis() / 50) as u64)
    }
}

pub(crate) fn new_active_mcp_tool_call(
    call_id: String,
    invocation: McpInvocation,
    animations_enabled: bool,
) -> McpToolCallCell {
    McpToolCallCell::new(call_id, invocation, animations_enabled)
}
