//! Conservative recovery for generated read-only PowerShell scripts.

#[path = "powershell_fallback/safe_shapes.rs"]
mod safe_shapes;

use std::collections::HashMap;
use std::collections::HashSet;

use codex_protocol::parse_command::ParsedCommand;

use self::safe_shapes::is_known_data_expression;
use self::safe_shapes::is_safe_data_projection;
use self::safe_shapes::is_safe_indexed_read_loop;
use super::classify_words;
use super::dedupe_exploration;
use super::is_benign_transform;
use super::powershell_lexer::split_top_level;
use super::powershell_lexer::split_top_level_statements;
use super::powershell_lexer::tokenize_powershell_words;

/// Classifies common generated inspection scripts containing literal variables
/// and indexed read expressions. If any statement is not provably display-only,
/// the caller leaves the command in the ordinary standalone presentation.
pub(super) fn classify_powershell_script_fallback(script: &str) -> Option<Vec<ParsedCommand>> {
    let mut literal_vars = HashMap::<String, String>::new();
    let mut data_vars = HashSet::<String>::new();
    let mut parsed = Vec::new();

    for statement in split_top_level_statements(script) {
        let statement = statement.trim();
        if statement.is_empty() {
            continue;
        }

        if is_safe_indexed_read_loop(statement, &data_vars) {
            continue;
        }

        if let Some((name, rhs)) = split_variable_assignment(statement) {
            if let Some(literal) = quoted_literal(rhs) {
                literal_vars.insert(name.to_ascii_lowercase(), literal);
                continue;
            }
            if is_safe_data_projection(rhs, &data_vars) {
                data_vars.insert(name.to_ascii_lowercase());
                continue;
            }
            let expression = classify_powershell_expression(rhs, &literal_vars)?;
            if expression.is_empty() {
                return None;
            }
            data_vars.insert(name.to_ascii_lowercase());
            parsed.extend(expression);
            continue;
        }

        if is_known_data_expression(statement, &data_vars) {
            continue;
        }
        if is_safe_data_projection(statement, &data_vars) {
            continue;
        }

        parsed.extend(classify_powershell_expression(statement, &literal_vars)?);
    }

    dedupe_exploration(parsed)
}

fn classify_powershell_expression(
    expression: &str,
    literal_vars: &HashMap<String, String>,
) -> Option<Vec<ParsedCommand>> {
    let expression = unwrap_parenthesized_command_expression(expression);
    if has_unsafe_powershell_expression_syntax(expression) {
        return None;
    }
    let mut parsed = Vec::new();
    for segment in split_top_level(expression, '|') {
        let mut words = tokenize_powershell_words(segment.trim())?;
        for word in &mut words {
            if let Some(name) = word.strip_prefix('$') {
                *word = literal_vars.get(&name.to_ascii_lowercase())?.clone();
            }
        }
        if is_benign_transform(&words) {
            if parsed.is_empty() {
                return None;
            }
            continue;
        }
        parsed.extend(classify_words(&words)?);
    }
    Some(parsed)
}

fn unwrap_parenthesized_command_expression(expression: &str) -> &str {
    let expression = expression.trim();
    if let Some(inner) = expression.strip_prefix('(')
        && let Some(close) = inner.rfind(')')
        && inner[close + 1..].trim_start().starts_with('.')
    {
        return inner[..close].trim();
    }
    expression
}

fn split_variable_assignment(statement: &str) -> Option<(&str, &str)> {
    let statement = statement.trim();
    let rest = statement.strip_prefix('$')?;
    let equals = rest.find('=')?;
    let name = rest[..equals].trim();
    if name.is_empty()
        || !name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
    {
        return None;
    }
    Some((name, rest[equals + 1..].trim()))
}

fn quoted_literal(value: &str) -> Option<String> {
    let value = value.trim();
    let quote = value.chars().next()?;
    if !matches!(quote, '\'' | '"') || !value.ends_with(quote) || value.len() < 2 {
        return None;
    }
    let inner = &value[1..value.len() - 1];
    // Single-quoted PowerShell strings are literal. Double-quoted strings can
    // interpolate variables and execute `$()` subexpressions, so only accept
    // the simple non-interpolated form in this presentation-only parser.
    if quote == '"' && (inner.contains('$') || inner.contains('`')) {
        return None;
    }
    Some(inner.to_string())
}

fn has_unsafe_powershell_expression_syntax(expression: &str) -> bool {
    let mut quote = None;
    let mut escaped = false;
    let mut chars = expression.chars().peekable();
    while let Some(ch) = chars.next() {
        if escaped {
            escaped = false;
            continue;
        }
        if quote == Some('"') && ch == '`' {
            escaped = true;
            continue;
        }
        if let Some(active_quote) = quote {
            if active_quote == '"' && ch == '$' && chars.peek() == Some(&'(') {
                return true;
            }
            if ch == active_quote {
                quote = None;
            }
            continue;
        }
        match ch {
            '\'' | '"' => quote = Some(ch),
            '&' | '<' | '>' | '{' | '}' | '(' | ')' => return true,
            '$' | '@' if chars.peek() == Some(&'(') => return true,
            _ => {}
        }
    }
    false
}
