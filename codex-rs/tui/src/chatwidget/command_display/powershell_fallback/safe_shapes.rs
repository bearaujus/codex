//! Side-effect-free PowerShell shapes emitted by generated inspection commands.

use std::collections::HashSet;

use super::super::powershell_lexer::split_top_level;
use super::super::powershell_lexer::tokenize_powershell_words;

pub(super) fn is_known_data_expression(statement: &str, data_vars: &HashSet<String>) -> bool {
    is_known_data_reference(statement.trim(), data_vars)
}

fn is_known_data_reference(reference: &str, data_vars: &HashSet<String>) -> bool {
    let Some(rest) = reference.strip_prefix('$') else {
        return false;
    };
    let name_end = rest
        .find(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_')
        .unwrap_or(rest.len());
    let name = &rest[..name_end];
    let suffix = rest[name_end..].trim();
    data_vars.contains(&name.to_ascii_lowercase()) && is_read_only_data_suffix(suffix)
}

pub(super) fn is_safe_indexed_read_loop(statement: &str, data_vars: &HashSet<String>) -> bool {
    let statement = statement.trim();
    let lower = statement.to_ascii_lowercase();
    if !(lower.starts_with("for(")
        || lower.starts_with("for (")
        || lower.starts_with("foreach(")
        || lower.starts_with("foreach ("))
        || !data_vars
            .iter()
            .any(|name| lower.contains(&format!("${name}[")))
    {
        return false;
    }

    let Some(unquoted) = source_without_quoted_content(statement) else {
        return false;
    };
    if !unquoted.contains("-f")
        || unquoted.contains('|')
        || unquoted.contains("$(")
        || unquoted.contains(['&', '<', '>'])
        || unquoted.contains("::")
        || contains_method_invocation(&unquoted)
        || contains_forbidden_command(&unquoted)
        || !assignments_target_local_scalars(&unquoted)
    {
        return false;
    }

    bare_loop_identifiers_are_safe(&unquoted)
}

pub(super) fn is_safe_data_projection(expression: &str, data_vars: &HashSet<String>) -> bool {
    let expression = strip_outer_data_wrapper(expression.trim());
    let segments = split_top_level(expression, '|');
    let Some((head, tail)) = segments.split_first() else {
        return false;
    };
    if !is_safe_scalar_data_expression(head.trim(), data_vars) {
        return false;
    }
    tail.iter()
        .all(|stage| is_safe_projection_stage(stage.trim()))
}

fn is_safe_scalar_data_expression(expression: &str, data_vars: &HashSet<String>) -> bool {
    if projection_fragment_has_side_effects(expression) {
        return false;
    }
    let Some(words) = tokenize_powershell_words(expression) else {
        return false;
    };
    let Some(reference) = words.first() else {
        return false;
    };
    let reference = reference.trim_matches(['(', ')']);
    if !is_known_data_reference(reference, data_vars) {
        return false;
    }
    match words.as_slice() {
        [_] => true,
        [_, operator, _value] => matches!(
            operator.to_ascii_lowercase().as_str(),
            "-join"
                | "-split"
                | "-contains"
                | "-notcontains"
                | "-in"
                | "-notin"
                | "-eq"
                | "-ne"
                | "-like"
                | "-notlike"
                | "-match"
                | "-notmatch"
        ),
        _ => false,
    }
}

fn is_safe_projection_stage(stage: &str) -> bool {
    if tokenize_powershell_words(stage).is_none() {
        return false;
    }
    let command = stage
        .split(|ch: char| ch.is_whitespace() || ch == '{')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if matches!(command.as_str(), "where-object" | "where" | "?") {
        return is_safe_where_object_stage(stage);
    }
    matches!(
        command.as_str(),
        "select-object"
            | "select"
            | "sort-object"
            | "sort"
            | "group-object"
            | "group"
            | "measure-object"
            | "measure"
            | "format-list"
            | "fl"
            | "format-table"
            | "ft"
            | "out-string"
    ) && !projection_fragment_has_side_effects(stage)
        && !stage.contains(['{', '}'])
}

fn is_safe_where_object_stage(stage: &str) -> bool {
    let Some(open) = stage.find('{') else {
        return false;
    };
    let Some(close) = stage.rfind('}') else {
        return false;
    };
    if !stage[close + 1..].trim().is_empty() {
        return false;
    }
    let body = stage[open + 1..close].trim();
    if body.is_empty()
        || !matches!(
            body.chars().next(),
            Some('$' | '!' | '(' | '\'' | '"' | '0'..='9')
        )
        || body.contains(['{', '}'])
        || projection_fragment_has_side_effects(body)
    {
        return false;
    }

    let Some(words) = tokenize_powershell_words(body) else {
        return false;
    };
    let mut needs_operand = true;
    for raw in words {
        let token = raw.trim_matches(['(', ')']);
        if token.is_empty() {
            continue;
        }
        let lower = token.to_ascii_lowercase();
        let is_unary = matches!(lower.as_str(), "!" | "-not");
        let is_binary = matches!(
            lower.as_str(),
            "-eq"
                | "-ne"
                | "-gt"
                | "-ge"
                | "-lt"
                | "-le"
                | "-like"
                | "-notlike"
                | "-match"
                | "-notmatch"
                | "-contains"
                | "-notcontains"
                | "-in"
                | "-notin"
                | "-and"
                | "-or"
        );
        if is_unary && needs_operand {
            continue;
        }
        if is_binary && !needs_operand {
            needs_operand = true;
            continue;
        }
        if needs_operand && safe_projection_operand(token) {
            needs_operand = false;
            continue;
        }
        return false;
    }
    !needs_operand
}

fn safe_projection_operand(token: &str) -> bool {
    !token.is_empty()
        && !token.contains("$(")
        && !token.contains(['&', '<', '>', '=', ';', '|', '{', '}'])
        && !token.contains("::")
        && !contains_forbidden_command(token)
}

fn projection_fragment_has_side_effects(fragment: &str) -> bool {
    let Some(unquoted) = source_without_quoted_content(fragment) else {
        return true;
    };
    unquoted.contains("$(")
        || unquoted.contains(['&', '<', '>', '=', ';'])
        || unquoted.contains("++")
        || unquoted.contains("--")
        || unquoted.contains("::")
        || contains_method_invocation(&unquoted)
        || contains_forbidden_command(&unquoted)
}

fn strip_outer_data_wrapper(expression: &str) -> &str {
    let expression = expression.trim();
    if let Some(inner) = expression
        .strip_prefix("@(")
        .and_then(|inner| inner.strip_suffix(')'))
    {
        inner.trim()
    } else if let Some(inner) = expression
        .strip_prefix('(')
        .and_then(|inner| inner.strip_suffix(')'))
    {
        inner.trim()
    } else {
        expression
    }
}

fn source_without_quoted_content(source: &str) -> Option<String> {
    let mut output = String::with_capacity(source.len());
    let mut quote = None;
    let mut escaped = false;
    for ch in source.chars() {
        if escaped {
            escaped = false;
            output.push(' ');
            continue;
        }
        if quote == Some('"') && ch == '`' {
            escaped = true;
            output.push(' ');
            continue;
        }
        if let Some(active_quote) = quote {
            if ch == active_quote {
                quote = None;
            }
            output.push(' ');
            continue;
        }
        match ch {
            '\'' | '"' => {
                quote = Some(ch);
                output.push(' ');
            }
            _ => output.push(ch),
        }
    }
    (quote.is_none() && !escaped).then_some(output)
}

fn contains_method_invocation(source: &str) -> bool {
    let chars = source.as_bytes();
    let mut index = 0;
    while index < chars.len() {
        if chars[index] != b'.' {
            index += 1;
            continue;
        }
        index += 1;
        while index < chars.len() && (chars[index].is_ascii_alphanumeric() || chars[index] == b'_')
        {
            index += 1;
        }
        while index < chars.len() && chars[index].is_ascii_whitespace() {
            index += 1;
        }
        if index < chars.len() && chars[index] == b'(' {
            return true;
        }
    }
    false
}

fn contains_forbidden_command(source: &str) -> bool {
    source
        .to_ascii_lowercase()
        .split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '-' && ch != '?')
        .any(|word| {
            matches!(
                word,
                "set-content"
                    | "add-content"
                    | "clear-content"
                    | "out-file"
                    | "tee-object"
                    | "remove-item"
                    | "move-item"
                    | "copy-item"
                    | "rename-item"
                    | "new-item"
                    | "invoke-expression"
                    | "invoke-command"
                    | "start-process"
                    | "stop-process"
                    | "export-csv"
                    | "export-clixml"
                    | "rm"
                    | "del"
                    | "erase"
                    | "mv"
                    | "cp"
                    | "ren"
                    | "iex"
                    | "icm"
            )
        })
}

fn assignments_target_local_scalars(source: &str) -> bool {
    source.match_indices('=').all(|(index, _)| {
        let lhs = source[..index]
            .rsplit(['(', ';', '{', '}'])
            .next()
            .unwrap_or_default()
            .trim();
        lhs.strip_prefix('$').is_some_and(|name| {
            !name.is_empty()
                && name
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        })
    })
}

fn bare_loop_identifiers_are_safe(source: &str) -> bool {
    let bytes = source.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if !(bytes[index].is_ascii_alphabetic() || bytes[index] == b'_') {
            index += 1;
            continue;
        }
        let start = index;
        while index < bytes.len() && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'_')
        {
            index += 1;
        }
        let previous = start.checked_sub(1).map(|i| bytes[i]);
        if matches!(previous, Some(b'$' | b'.')) {
            continue;
        }
        let identifier = source[start..index].to_ascii_lowercase();
        if !matches!(
            identifier.as_str(),
            "for"
                | "foreach"
                | "if"
                | "break"
                | "in"
                | "le"
                | "lt"
                | "ge"
                | "gt"
                | "eq"
                | "ne"
                | "f"
        ) {
            return false;
        }
    }
    true
}

fn is_read_only_data_suffix(suffix: &str) -> bool {
    if suffix.is_empty() {
        return true;
    }
    if !(suffix.starts_with('.') || suffix.starts_with('['))
        || suffix.chars().any(|ch| {
            matches!(
                ch,
                '(' | ')' | '=' | ';' | '|' | '{' | '}' | '>' | '<' | '&'
            )
        })
        || suffix.contains("++")
        || suffix.contains("--")
    {
        return false;
    }

    let mut quote = None;
    let mut escaped = false;
    let mut bracket_depth = 0usize;
    for ch in suffix.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        if quote == Some('"') && ch == '`' {
            escaped = true;
            continue;
        }
        if let Some(active_quote) = quote {
            if ch == active_quote {
                quote = None;
            }
            continue;
        }
        match ch {
            '\'' | '"' => quote = Some(ch),
            '[' => bracket_depth += 1,
            ']' if bracket_depth > 0 => bracket_depth -= 1,
            ']' => return false,
            ch if ch.is_ascii_alphanumeric()
                || matches!(
                    ch,
                    '_' | '.' | '$' | '-' | '+' | ':' | ',' | '*' | '?' | '/'
                ) => {}
            _ => return false,
        }
    }

    quote.is_none() && !escaped && bracket_depth == 0
}
