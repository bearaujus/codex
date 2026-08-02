//! Presentation-only command classification for compact TUI exploration groups.
//!
//! Protocol command actions remain authoritative for execution and approval. This
//! module only recovers read/list/search intent that is lost when a top-level
//! PowerShell wrapper is conservatively reported as `Unknown`.

#[path = "command_display/powershell_fallback.rs"]
mod powershell_fallback;
#[path = "command_display/powershell_lexer.rs"]
mod powershell_lexer;

use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;

use codex_protocol::parse_command::ParsedCommand;
use codex_shell_command::parse_command::parse_command;
use codex_shell_command::powershell::UTF8_OUTPUT_PREFIX;
use codex_shell_command::powershell::extract_powershell_command;
use codex_shell_command::powershell::parse_powershell_command_into_plain_commands;

use crate::exec_cell::CommandPresentation;
use powershell_fallback::classify_powershell_script_fallback;

pub(super) struct PresentedCommand {
    pub(super) parsed: Vec<ParsedCommand>,
    pub(super) presentation: CommandPresentation,
}

pub(super) fn presentation_parsed_commands(
    command: &[String],
    parsed: Vec<ParsedCommand>,
) -> PresentedCommand {
    if let Some(recovered) = classify_powershell_command(command) {
        let presentation = structured_inspection_target(command, &recovered)
            .map(|target| CommandPresentation::Inspection { target })
            .unwrap_or(CommandPresentation::Exploration);
        return PresentedCommand {
            parsed: recovered,
            presentation,
        };
    }

    let presentation = if is_exploration(&parsed) {
        CommandPresentation::Exploration
    } else {
        CommandPresentation::Command
    };
    PresentedCommand {
        parsed,
        presentation,
    }
}

fn classify_powershell_command(command: &[String]) -> Option<Vec<ParsedCommand>> {
    if let Some(commands) = parse_powershell_command_into_plain_commands(command)
        && let Some(parsed) = classify_plain_commands(&commands)
    {
        return Some(parsed);
    }

    let (_, script) = extract_powershell_command(command)?;
    let script = script
        .strip_prefix(UTF8_OUTPUT_PREFIX)
        .unwrap_or(script)
        .trim();
    classify_powershell_script_fallback(script)
}

fn classify_plain_commands(commands: &[Vec<String>]) -> Option<Vec<ParsedCommand>> {
    let mut parsed = Vec::new();
    for words in commands {
        if words_have_unsafe_powershell_syntax(words) {
            return None;
        }
        if is_benign_transform(words) {
            if parsed.is_empty() {
                return None;
            }
            continue;
        }
        parsed.extend(classify_words(words)?);
    }
    dedupe_exploration(parsed)
}

fn classify_words(words: &[String]) -> Option<Vec<ParsedCommand>> {
    let command = words.first()?;
    let command_name = Path::new(command)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(command)
        .to_ascii_lowercase();
    let cmd = words.join(" ");

    match command_name.as_str() {
        "get-content" | "gc" | "type" => {
            let path = option_value(words, &["-literalpath", "-path"]).or_else(|| {
                first_positional(
                    words,
                    &[
                        "-encoding",
                        "-delimiter",
                        "-readcount",
                        "-totalcount",
                        "-tail",
                        "-filter",
                        "-include",
                        "-exclude",
                    ],
                )
            })?;
            Some(vec![ParsedCommand::Read {
                cmd,
                name: display_name(path),
                path: PathBuf::from(path),
            }])
        }
        "get-childitem" | "gci" | "dir" | "ls" => {
            let path = option_value(words, &["-literalpath", "-path"]).or_else(|| {
                first_positional(
                    words,
                    &["-filter", "-include", "-exclude", "-depth", "-attributes"],
                )
            });
            Some(vec![ParsedCommand::ListFiles {
                cmd,
                path: path.map(ToString::to_string),
            }])
        }
        "select-string" | "sls" => {
            let query = option_value(words, &["-pattern"])
                .or_else(|| first_positional(words, &["-path", "-literalpath"]));
            let path = option_value(words, &["-literalpath", "-path"]);
            Some(vec![ParsedCommand::Search {
                cmd,
                query: query.map(ToString::to_string),
                path: path.map(ToString::to_string),
            }])
        }
        "get-item" | "gi" | "get-filehash" | "test-path" => {
            let path = option_value(words, &["-literalpath", "-path"])
                .or_else(|| first_positional(words, &[]))?;
            Some(vec![ParsedCommand::Read {
                cmd,
                name: display_name(path),
                path: PathBuf::from(path),
            }])
        }
        "resolve-path" => {
            let path = option_value(words, &["-literalpath", "-path"])
                .or_else(|| first_positional(words, &[]));
            Some(vec![ParsedCommand::ListFiles {
                cmd,
                path: path.map(ToString::to_string),
            }])
        }
        "git" => classify_read_only_git(words),
        _ => {
            let parsed = parse_command(words);
            is_exploration(&parsed).then_some(parsed)
        }
    }
}

fn classify_read_only_git(words: &[String]) -> Option<Vec<ParsedCommand>> {
    let subcommand = words.get(1)?.to_ascii_lowercase();
    let cmd = words.join(" ");
    match subcommand.as_str() {
        "ls-files" => Some(vec![ParsedCommand::ListFiles {
            cmd,
            path: words
                .iter()
                .skip(2)
                .find(|word| !word.starts_with('-'))
                .cloned(),
        }]),
        "status" | "diff" | "log" | "show" | "grep" => Some(vec![ParsedCommand::Search {
            cmd,
            query: None,
            path: None,
        }]),
        _ => None,
    }
}

fn is_benign_transform(words: &[String]) -> bool {
    let Some(command) = words.first() else {
        return false;
    };
    matches!(
        command.to_ascii_lowercase().as_str(),
        "sort-object"
            | "sort"
            | "select-object"
            | "select"
            | "measure-object"
            | "measure"
            | "convertfrom-json"
            | "format-table"
            | "format-list"
            | "out-string"
    )
}

fn words_have_unsafe_powershell_syntax(words: &[String]) -> bool {
    words.iter().any(|word| {
        let word = word.trim();
        word.contains("$(")
            || word.contains("@(")
            || word.contains(['{', '}'])
            || word.contains('&')
            || word.find(['>', '<']).is_some_and(|index| {
                index == 0 || word[..index].chars().all(|ch| ch.is_ascii_digit())
            })
    })
}

fn option_value<'a>(words: &'a [String], options: &[&str]) -> Option<&'a str> {
    words.windows(2).find_map(|pair| {
        options
            .iter()
            .any(|option| pair[0].eq_ignore_ascii_case(option))
            .then_some(pair[1].as_str())
    })
}

fn first_positional<'a>(words: &'a [String], options_with_values: &[&str]) -> Option<&'a str> {
    let mut skip_next = false;
    for word in words.iter().skip(1) {
        if skip_next {
            skip_next = false;
            continue;
        }
        if options_with_values
            .iter()
            .any(|option| word.eq_ignore_ascii_case(option))
        {
            skip_next = true;
            continue;
        }
        if !word.starts_with('-') {
            return Some(word);
        }
    }
    None
}

fn display_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(path)
        .to_string()
}

fn structured_inspection_target(command: &[String], parsed: &[ParsedCommand]) -> Option<String> {
    let (_, script) = extract_powershell_command(command)?;
    if !script.to_ascii_lowercase().contains("convertfrom-json") {
        return None;
    }

    let mut names = Vec::new();
    for item in parsed {
        if let ParsedCommand::Read { name, .. } = item
            && !names.contains(name)
        {
            names.push(name.clone());
        }
    }
    match names.as_slice() {
        [] => None,
        [name] => Some(name.clone()),
        [first, second] => Some(format!("{first}, {second}")),
        [first, second, rest @ ..] => Some(format!("{first}, {second} +{} files", rest.len())),
    }
}

pub(super) fn is_exploration(parsed: &[ParsedCommand]) -> bool {
    !parsed.is_empty()
        && parsed.iter().all(|item| {
            matches!(
                item,
                ParsedCommand::Read { .. }
                    | ParsedCommand::ListFiles { .. }
                    | ParsedCommand::Search { .. }
            )
        })
}

fn dedupe_exploration(parsed: Vec<ParsedCommand>) -> Option<Vec<ParsedCommand>> {
    if !is_exploration(&parsed) {
        return None;
    }
    let mut seen = HashSet::new();
    let mut deduped = Vec::with_capacity(parsed.len());
    for item in parsed {
        let key = format!("{item:?}");
        if seen.insert(key) {
            deduped.push(item);
        }
    }
    Some(deduped)
}

#[cfg(test)]
#[path = "command_display_tests.rs"]
mod tests;
