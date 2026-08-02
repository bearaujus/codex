//! Minimal quote/depth-aware splitting for presentation-only PowerShell parsing.

pub(super) fn split_top_level_statements(script: &str) -> Vec<&str> {
    split_top_level_with(script, |ch| ch == ';' || ch == '\n' || ch == '\r')
}

pub(super) fn split_top_level(source: &str, delimiter: char) -> Vec<&str> {
    split_top_level_with(source, |ch| ch == delimiter)
}

fn split_top_level_with(source: &str, is_delimiter: impl Fn(char) -> bool) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut quote = None;
    let mut escaped = false;
    let mut depth = 0usize;
    for (index, ch) in source.char_indices() {
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
            '(' | '[' | '{' => depth = depth.saturating_add(1),
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            _ if depth == 0 && is_delimiter(ch) => {
                parts.push(&source[start..index]);
                start = index + ch.len_utf8();
            }
            _ => {}
        }
    }
    parts.push(&source[start..]);
    parts
}

pub(super) fn tokenize_powershell_words(command: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;
    for ch in command.chars() {
        if escaped {
            current.push(ch);
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
            } else {
                current.push(ch);
            }
            continue;
        }
        match ch {
            '\'' | '"' => quote = Some(ch),
            ch if ch.is_whitespace() => {
                if !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }
    if quote.is_some() || escaped {
        return None;
    }
    if !current.is_empty() {
        words.push(current);
    }
    (!words.is_empty()).then_some(words)
}
