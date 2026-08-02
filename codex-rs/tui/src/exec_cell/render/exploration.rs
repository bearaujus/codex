//! High-level rendering for compact read/search/list exploration cards.

use std::collections::HashSet;

use super::exploration_rows::ExplorationKind;
use super::exploration_rows::ExplorationRow;
use super::exploration_rows::ExplorationState;
use super::exploration_rows::format_count;
use super::exploration_rows::singular_or_plural;
use super::*;

const EXPLORATION_DETAIL_MAX_WIDTH: usize = 100;
const EXPLORATION_BODY_MAX_ROWS: usize = 5;

impl ExecCell {
    pub(super) fn exploring_display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let active_operations = self
            .calls
            .iter()
            .filter(|call| ExplorationState::from_call(call) == ExplorationState::Active)
            .map(|call| call.parsed.len())
            .sum::<usize>();
        let completed_operations = self
            .calls
            .iter()
            .filter(|call| ExplorationState::from_call(call) == ExplorationState::Completed)
            .map(|call| call.parsed.len())
            .sum::<usize>();
        let failed_operations = self
            .calls
            .iter()
            .filter(|call| ExplorationState::from_call(call) == ExplorationState::Failed)
            .map(|call| call.parsed.len())
            .sum::<usize>();
        let total_operations = active_operations + completed_operations + failed_operations;
        let files = self
            .calls
            .iter()
            .flat_map(|call| &call.parsed)
            .filter_map(|parsed| match parsed {
                ParsedCommand::Read { path, .. } => Some(path.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect::<HashSet<_>>()
            .len();

        let is_active = active_operations > 0;
        let has_failures = failed_operations > 0;
        let mut header = Line::from(vec![
            if is_active {
                activity_marker(self.active_start_time(), self.animations_enabled())
            } else if has_failures {
                "•".red().bold()
            } else {
                "•".dim()
            },
            " ".into(),
            if is_active {
                "Exploring".bold()
            } else if has_failures {
                "Exploration failed".bold()
            } else {
                "Explored".bold()
            },
        ]);
        let full_summary = if is_active {
            let mut parts = vec![
                format!("{} active", format_count(active_operations)),
                format!("{} done", format_count(completed_operations)),
            ];
            if has_failures {
                parts.push(format!("{} failed", format_count(failed_operations)));
            }
            format!(" · {}", parts.join(" · "))
        } else if has_failures {
            let mut parts = vec![format!("{} failed", format_count(failed_operations))];
            if completed_operations > 0 {
                parts.push(format!("{} done", format_count(completed_operations)));
            }
            format!(" · {}", parts.join(" · "))
        } else {
            let mut summary = Vec::new();
            if files > 0 {
                summary.push(format!(
                    "{} {}",
                    format_count(files),
                    singular_or_plural(files, "file", "files"),
                ));
            }
            summary.push(format!(
                "{} {}",
                format_count(total_operations),
                singular_or_plural(total_operations, "operation", "operations"),
            ));
            format!(" · {}", summary.join(" · "))
        };
        let mut summary_candidates = vec![full_summary];
        if is_active {
            if has_failures {
                summary_candidates.push(format!(
                    " · {} active · {} failed",
                    format_count(active_operations),
                    format_count(failed_operations),
                ));
            }
            summary_candidates.push(format!(" · {} active", format_count(active_operations)));
        } else if has_failures {
            summary_candidates.push(format!(" · {} failed", format_count(failed_operations)));
        } else {
            summary_candidates.push(format!(
                " · {} {}",
                format_count(total_operations),
                singular_or_plural(total_operations, "operation", "operations"),
            ));
            summary_candidates.push(format!(" · {} ops", format_count(total_operations)));
        }
        let max_width = usize::from(width);
        if let Some(summary) = summary_candidates
            .into_iter()
            .find(|summary| header.width() + summary.width() <= max_width)
        {
            header.push_span(summary.dim());
        }
        let header = truncate_line_with_ellipsis_if_overflow(header, max_width);
        let mut out = vec![header];
        if width <= 4 {
            return out;
        }

        let content_width = max_width
            .saturating_sub(4)
            .min(EXPLORATION_DETAIL_MAX_WIDTH);
        let mut rows = self.exploration_rows();
        let hidden_rows = if rows.len() > EXPLORATION_BODY_MAX_ROWS {
            rows.split_off(EXPLORATION_BODY_MAX_ROWS - 1)
        } else {
            Vec::new()
        };
        let mut body = rows
            .iter()
            .map(|row| Self::exploration_row_line(row, content_width))
            .collect::<Vec<_>>();
        if !hidden_rows.is_empty() {
            body.push(truncate_line_with_ellipsis_if_overflow(
                Line::from(exploration_overflow_text(&hidden_rows)).dim(),
                content_width,
            ));
        }

        let body_len = body.len();
        for (index, mut line) in body.into_iter().enumerate() {
            let connector = if index + 1 == body_len {
                "  └ "
            } else {
                "  ├ "
            };
            line.spans.insert(0, connector.dim());
            out.push(line);
        }
        out
    }
}

fn exploration_overflow_text(rows: &[ExplorationRow]) -> String {
    let active = rows
        .iter()
        .filter(|row| row.state == ExplorationState::Active)
        .map(|row| row.operation_count)
        .sum::<usize>();
    let failed = rows
        .iter()
        .filter(|row| row.state == ExplorationState::Failed)
        .map(|row| row.operation_count)
        .sum::<usize>();
    let completed = rows
        .iter()
        .filter(|row| row.state == ExplorationState::Completed)
        .map(|row| row.operation_count)
        .sum::<usize>();
    let reads = rows
        .iter()
        .filter(|row| row.kind == ExplorationKind::Read)
        .map(|row| row.operation_count)
        .sum::<usize>();
    let searches = rows
        .iter()
        .filter(|row| row.kind == ExplorationKind::Search)
        .map(|row| row.operation_count)
        .sum::<usize>();
    let listings = rows
        .iter()
        .filter(|row| row.kind == ExplorationKind::List)
        .map(|row| row.operation_count)
        .sum::<usize>();

    let mut state_parts = Vec::new();
    if active > 0 {
        state_parts.push(format!("{} active", format_count(active)));
    }
    if failed > 0 {
        state_parts.push(format!("{} failed", format_count(failed)));
    }
    if completed > 0 {
        state_parts.push(format!("{} completed", format_count(completed)));
    }

    let mut category_parts = Vec::new();
    if reads > 0 {
        category_parts.push(format!(
            "{} {}",
            format_count(reads),
            singular_or_plural(reads, "read", "reads"),
        ));
    }
    if searches > 0 {
        category_parts.push(format!(
            "{} {}",
            format_count(searches),
            singular_or_plural(searches, "search", "searches"),
        ));
    }
    if listings > 0 {
        category_parts.push(format!(
            "{} {}",
            format_count(listings),
            singular_or_plural(listings, "listing", "listings"),
        ));
    }

    format!(
        "+{} · {}",
        state_parts.join(" · "),
        category_parts.join(", ")
    )
}
