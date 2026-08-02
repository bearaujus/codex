//! Row aggregation and width-aware formatting for compact exploration cards.

use std::collections::HashMap;

use super::*;
use unicode_width::UnicodeWidthChar;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum ExplorationKind {
    Read,
    Search,
    List,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum ExplorationState {
    Active,
    Completed,
    Failed,
}

impl ExplorationState {
    pub(super) fn from_call(call: &ExecCall) -> Self {
        if call.duration.is_none() {
            Self::Active
        } else if call
            .output
            .as_ref()
            .is_some_and(|output| output.exit_code == 0)
        {
            Self::Completed
        } else {
            Self::Failed
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExplorationStats {
    Read { lines: usize, characters: usize },
    Search { result_lines: usize },
    List { entries: usize },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ExplorationRow {
    pub(super) state: ExplorationState,
    pub(super) kind: ExplorationKind,
    variable: String,
    context: String,
    qualifier: String,
    occurrences: usize,
    pub(super) operation_count: usize,
    stats: Option<ExplorationStats>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ExplorationRowKey {
    state: ExplorationState,
    kind: ExplorationKind,
    variable: String,
    context: String,
    qualifier: String,
}

impl ExplorationRow {
    fn key(&self) -> ExplorationRowKey {
        ExplorationRowKey {
            state: self.state,
            kind: self.kind,
            variable: self.variable.clone(),
            context: self.context.clone(),
            qualifier: self.qualifier.clone(),
        }
    }

    fn verb(&self) -> &'static str {
        match (self.state, self.kind) {
            (ExplorationState::Active, ExplorationKind::Read) => "Reading",
            (ExplorationState::Completed, ExplorationKind::Read) => "Read",
            (ExplorationState::Failed, ExplorationKind::Read) => "Failed reading",
            (ExplorationState::Active, ExplorationKind::Search) => "Searching",
            (ExplorationState::Completed, ExplorationKind::Search) => "Searched",
            (ExplorationState::Failed, ExplorationKind::Search) => "Failed searching",
            (ExplorationState::Active, ExplorationKind::List) => "Listing",
            (ExplorationState::Completed, ExplorationKind::List) => "Listed",
            (ExplorationState::Failed, ExplorationKind::List) => "Failed listing",
        }
    }

    fn stats_text(&self) -> String {
        match self.stats {
            Some(ExplorationStats::Read { lines, characters }) => format!(
                " · {} {} · {} {}",
                format_count(lines),
                singular_or_plural(lines, "line", "lines"),
                format_count(characters),
                singular_or_plural(characters, "char", "chars"),
            ),
            Some(ExplorationStats::Search { result_lines }) => format!(
                " · {} result {}",
                format_count(result_lines),
                singular_or_plural(result_lines, "line", "lines"),
            ),
            Some(ExplorationStats::List { entries }) => format!(
                " · {} {}",
                format_count(entries),
                singular_or_plural(entries, "entry", "entries"),
            ),
            None => String::new(),
        }
    }
}

pub(super) fn singular_or_plural(
    value: usize,
    singular: &'static str,
    plural: &'static str,
) -> &'static str {
    if value == 1 { singular } else { plural }
}

pub(super) fn format_count(value: usize) -> String {
    let digits = value.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    grouped
}

fn truncate_text_to_width(text: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(text) <= max_width {
        return text.to_string();
    }
    if max_width == 0 {
        return String::new();
    }
    if max_width == 1 {
        return "…".to_string();
    }

    let content_width = max_width - 1;
    let mut truncated = String::new();
    let mut width = 0;
    for ch in text.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + ch_width > content_width {
            break;
        }
        truncated.push(ch);
        width += ch_width;
    }
    truncated.push('…');
    truncated
}

fn merge_exploration_stats(
    current: Option<ExplorationStats>,
    incoming: Option<ExplorationStats>,
) -> Option<ExplorationStats> {
    match (current, incoming) {
        (
            Some(ExplorationStats::Read {
                lines: current_lines,
                characters: current_characters,
            }),
            Some(ExplorationStats::Read { lines, characters }),
        ) => Some(ExplorationStats::Read {
            lines: current_lines + lines,
            characters: current_characters + characters,
        }),
        (
            Some(ExplorationStats::Search {
                result_lines: current,
            }),
            Some(ExplorationStats::Search { result_lines }),
        ) => Some(ExplorationStats::Search {
            result_lines: current + result_lines,
        }),
        (
            Some(ExplorationStats::List { entries: current }),
            Some(ExplorationStats::List { entries }),
        ) => Some(ExplorationStats::List {
            entries: current + entries,
        }),
        (Some(stats), None) | (None, Some(stats)) => Some(stats),
        (None, None) => None,
        _ => current,
    }
}

impl ExecCell {
    pub(super) fn exploration_rows(&self) -> Vec<ExplorationRow> {
        let mut rows = Vec::new();
        let mut row_indexes = HashMap::new();
        for state in [
            ExplorationState::Active,
            ExplorationState::Failed,
            ExplorationState::Completed,
        ] {
            for call in self
                .calls
                .iter()
                .filter(|call| ExplorationState::from_call(call) == state)
            {
                let mut read_names = Vec::new();
                let mut read_count = 0;
                let mut searches = Vec::new();
                let mut search_count = 0;
                let mut listings = Vec::new();
                let mut list_count = 0;
                let mut category_order = Vec::new();

                for parsed in &call.parsed {
                    match parsed {
                        ParsedCommand::Read { name, .. } => {
                            if read_count == 0 {
                                category_order.push(ExplorationKind::Read);
                            }
                            read_count += 1;
                            let name = sanitize_exploration_text(name);
                            if !read_names.contains(&name) {
                                read_names.push(name);
                            }
                        }
                        ParsedCommand::Search { cmd, query, path } => {
                            if search_count == 0 {
                                category_order.push(ExplorationKind::Search);
                            }
                            search_count += 1;
                            let value = (
                                sanitize_exploration_text(query.as_deref().unwrap_or(cmd)),
                                path.as_deref().map(sanitize_exploration_text),
                            );
                            if !searches.contains(&value) {
                                searches.push(value);
                            }
                        }
                        ParsedCommand::ListFiles { cmd, path } => {
                            if list_count == 0 {
                                category_order.push(ExplorationKind::List);
                            }
                            list_count += 1;
                            let value = sanitize_exploration_text(path.as_deref().unwrap_or(cmd));
                            if !listings.contains(&value) {
                                listings.push(value);
                            }
                        }
                        ParsedCommand::Unknown { .. } => {}
                    }
                }

                let category_count = usize::from(read_count > 0)
                    + usize::from(search_count > 0)
                    + usize::from(list_count > 0);
                let output_stats = call
                    .output
                    .as_ref()
                    .map(|output| {
                        let stats = output.stats();
                        (stats.lines(), stats.characters())
                    })
                    .or_else(|| (state != ExplorationState::Active).then_some((0, 0)));
                let mut call_rows = Vec::new();

                if read_count > 0 {
                    let (variable, qualifier) =
                        Self::summarize_exploration_values(&read_names, "file", "files");
                    let stats = if category_count == 1 {
                        output_stats
                            .map(|(lines, characters)| ExplorationStats::Read { lines, characters })
                    } else {
                        None
                    };
                    call_rows.push(ExplorationRow {
                        state,
                        kind: ExplorationKind::Read,
                        variable,
                        context: String::new(),
                        qualifier,
                        occurrences: 1,
                        operation_count: read_count,
                        stats,
                    });
                }

                if search_count > 0 {
                    let first = searches
                        .first()
                        .map(|(query, _)| query.clone())
                        .unwrap_or_default();
                    let qualifier = if search_count > 1 {
                        format!(
                            " +{} {}",
                            search_count - 1,
                            singular_or_plural(search_count - 1, "search", "searches"),
                        )
                    } else {
                        String::new()
                    };
                    let common_path =
                        searches
                            .first()
                            .and_then(|(_, path)| path.as_ref())
                            .filter(|path| {
                                searches
                                    .iter()
                                    .all(|(_, candidate)| candidate.as_ref() == Some(path))
                            });
                    let stats = if category_count == 1 {
                        output_stats
                            .map(|(result_lines, _)| ExplorationStats::Search { result_lines })
                    } else {
                        None
                    };
                    call_rows.push(ExplorationRow {
                        state,
                        kind: ExplorationKind::Search,
                        variable: first,
                        context: common_path
                            .map(|path| format!(" in {path}"))
                            .unwrap_or_default(),
                        qualifier,
                        occurrences: 1,
                        operation_count: search_count,
                        stats,
                    });
                }

                if list_count > 0 {
                    let (variable, qualifier) =
                        Self::summarize_exploration_values(&listings, "location", "locations");
                    let stats = if category_count == 1 {
                        output_stats.map(|(entries, _)| ExplorationStats::List { entries })
                    } else {
                        None
                    };
                    call_rows.push(ExplorationRow {
                        state,
                        kind: ExplorationKind::List,
                        variable,
                        context: String::new(),
                        qualifier,
                        occurrences: 1,
                        operation_count: list_count,
                        stats,
                    });
                }

                call_rows.sort_by_key(|row| {
                    category_order
                        .iter()
                        .position(|kind| *kind == row.kind)
                        .unwrap_or(usize::MAX)
                });
                for row in call_rows {
                    Self::push_exploration_row(&mut rows, &mut row_indexes, row);
                }
            }
        }
        rows
    }

    fn summarize_exploration_values(
        values: &[String],
        singular: &'static str,
        plural: &'static str,
    ) -> (String, String) {
        match values {
            [] => (String::new(), String::new()),
            [only] => (only.clone(), String::new()),
            [first, second] => (format!("{first}, {second}"), String::new()),
            [first, second, rest @ ..] => (
                format!("{first}, {second}"),
                format!(
                    " +{} {}",
                    rest.len(),
                    singular_or_plural(rest.len(), singular, plural),
                ),
            ),
        }
    }

    fn push_exploration_row(
        rows: &mut Vec<ExplorationRow>,
        row_indexes: &mut HashMap<ExplorationRowKey, usize>,
        incoming: ExplorationRow,
    ) {
        let key = incoming.key();
        if let Some(index) = row_indexes.get(&key).copied() {
            let existing = &mut rows[index];
            existing.occurrences += incoming.occurrences;
            existing.operation_count += incoming.operation_count;
            existing.stats = merge_exploration_stats(existing.stats, incoming.stats);
        } else {
            row_indexes.insert(key, rows.len());
            rows.push(incoming);
        }
    }

    pub(super) fn exploration_row_line(
        row: &ExplorationRow,
        content_width: usize,
    ) -> Line<'static> {
        let occurrence_text = if row.occurrences > 1 {
            format!(" ×{}", format_count(row.occurrences))
        } else {
            String::new()
        };
        let stats_text = row.stats_text();
        let prefix_width =
            UnicodeWidthStr::width(row.verb()) + usize::from(!row.variable.is_empty());
        let fixed_width = prefix_width
            + row.qualifier.width()
            + UnicodeWidthStr::width(occurrence_text.as_str())
            + stats_text.width();
        let variable_reserve = usize::from(!row.variable.is_empty());
        let context_budget = content_width
            .saturating_sub(fixed_width)
            .saturating_sub(variable_reserve);
        let context = truncate_text_to_width(&row.context, context_budget);
        let variable_budget = content_width
            .saturating_sub(fixed_width)
            .saturating_sub(context.width());
        let variable = truncate_text_to_width(&row.variable, variable_budget);

        let verb = if row.state == ExplorationState::Failed {
            row.verb().red().bold()
        } else {
            row.verb().cyan()
        };
        let mut line = Line::from(verb);
        if !variable.is_empty() {
            line.push_span(" ");
            line.push_span(variable);
        }
        if !context.is_empty() {
            line.push_span(context.dim());
        }
        if !row.qualifier.is_empty() {
            line.push_span(row.qualifier.clone().dim());
        }
        if !occurrence_text.is_empty() {
            line.push_span(occurrence_text.dim());
        }
        if !stats_text.is_empty() {
            line.push_span(stats_text.dim());
        }
        truncate_line_with_ellipsis_if_overflow(line, content_width)
    }
}
