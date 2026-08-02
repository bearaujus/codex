use std::collections::BTreeMap;

use anyhow::Context;
use clap::Args;
use codex_login::default_client::default_headers;
use codex_login::default_client::get_codex_user_agent;
use codex_login::default_client::originator;
use codex_terminal_detection::Multiplexer;
use codex_terminal_detection::TerminalInfo;
use codex_terminal_detection::TerminalName;
use codex_terminal_detection::terminal_info;
use serde::Serialize;

#[derive(Debug, Args)]
pub(crate) struct InfoCommand {
    /// Render the info payload as JSON.
    #[arg(long, default_value_t = false)]
    pub(crate) json: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct CliInfo {
    originator: String,
    version: String,
    user_agent: String,
    headers: BTreeMap<String, String>,
    os: OsInfoPayload,
    terminal: TerminalInfoPayload,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct OsInfoPayload {
    r#type: String,
    version: String,
    arch: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct TerminalInfoPayload {
    name: String,
    user_agent_token: String,
    term_program: Option<String>,
    version: Option<String>,
    term: Option<String>,
    multiplexer: Option<MultiplexerPayload>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct MultiplexerPayload {
    name: String,
    version: Option<String>,
}

pub(crate) fn run_info(cmd: InfoCommand) -> anyhow::Result<()> {
    let info = collect_info().context("failed to collect CLI metadata")?;
    if cmd.json {
        println!("{}", serde_json::to_string_pretty(&info)?);
    } else {
        print!("{}", render_human(&info));
    }
    Ok(())
}

fn collect_info() -> anyhow::Result<CliInfo> {
    let os_info = os_info::get();
    let version = env!("CARGO_PKG_VERSION").to_string();
    let originator = originator().value;
    let user_agent = get_codex_user_agent();
    let default_headers = default_headers();
    let mut headers = BTreeMap::new();
    for (name, value) in &default_headers {
        headers.insert(
            name.as_str().to_string(),
            value
                .to_str()
                .context("default header value was not valid UTF-8")?
                .to_string(),
        );
    }
    let terminal = terminal_info();
    Ok(CliInfo {
        originator,
        version,
        user_agent,
        headers,
        os: OsInfoPayload {
            r#type: os_info.os_type().to_string(),
            version: os_info.version().to_string(),
            arch: os_info.architecture().unwrap_or("unknown").to_string(),
        },
        terminal: terminal_payload(&terminal),
    })
}

fn terminal_payload(terminal: &TerminalInfo) -> TerminalInfoPayload {
    TerminalInfoPayload {
        name: terminal_name(terminal.name).to_string(),
        user_agent_token: codex_terminal_detection::user_agent(),
        term_program: terminal.term_program.clone(),
        version: terminal.version.clone(),
        term: terminal.term.clone(),
        multiplexer: terminal.multiplexer.as_ref().map(multiplexer_payload),
    }
}

fn multiplexer_payload(multiplexer: &Multiplexer) -> MultiplexerPayload {
    match multiplexer {
        Multiplexer::Tmux { version } => MultiplexerPayload {
            name: "tmux".to_string(),
            version: version.clone(),
        },
        Multiplexer::Zellij { version } => MultiplexerPayload {
            name: "zellij".to_string(),
            version: version.clone(),
        },
    }
}

fn terminal_name(name: TerminalName) -> &'static str {
    match name {
        TerminalName::AppleTerminal => "apple_terminal",
        TerminalName::Ghostty => "ghostty",
        TerminalName::Iterm2 => "iterm2",
        TerminalName::WarpTerminal => "warp_terminal",
        TerminalName::VsCode => "vscode",
        TerminalName::WezTerm => "wezterm",
        TerminalName::Kitty => "kitty",
        TerminalName::Alacritty => "alacritty",
        TerminalName::Konsole => "konsole",
        TerminalName::GnomeTerminal => "gnome_terminal",
        TerminalName::Vte => "vte",
        TerminalName::WindowsTerminal => "windows_terminal",
        TerminalName::Dumb => "dumb",
        TerminalName::Unknown => "unknown",
    }
}

fn render_human(info: &CliInfo) -> String {
    let mut output = String::new();
    output.push_str(&format!("Originator: {}\n", info.originator));
    output.push_str(&format!("Version: {}\n", info.version));
    output.push_str(&format!(
        "OS: {} {} ({})\n",
        info.os.r#type, info.os.version, info.os.arch
    ));
    output.push_str(&format!("User-Agent: {}\n", info.user_agent));
    output.push_str(&format!(
        "Terminal: {} ({})\n",
        info.terminal.user_agent_token, info.terminal.name
    ));
    if let Some(term_program) = info.terminal.term_program.as_deref() {
        output.push_str(&format!("TERM_PROGRAM: {term_program}\n"));
    }
    if let Some(version) = info.terminal.version.as_deref() {
        output.push_str(&format!("Terminal Version: {version}\n"));
    }
    if let Some(term) = info.terminal.term.as_deref() {
        output.push_str(&format!("TERM: {term}\n"));
    }
    if let Some(multiplexer) = info.terminal.multiplexer.as_ref() {
        output.push_str("Multiplexer: ");
        output.push_str(&multiplexer.name);
        if let Some(version) = multiplexer.version.as_deref() {
            output.push('/');
            output.push_str(version);
        }
        output.push('\n');
    }
    output.push_str("Headers:\n");
    for (name, value) in &info.headers {
        output.push_str(&format!("  {name}: {value}\n"));
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn collect_info_matches_default_client_metadata() {
        let info = collect_info().expect("info should collect");
        assert_eq!(info.originator, originator().value);
        assert_eq!(info.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(info.user_agent, get_codex_user_agent());
        assert_eq!(
            info.headers.get("originator").map(String::as_str),
            Some(info.originator.as_str())
        );
        assert_eq!(
            info.headers.get("user-agent").map(String::as_str),
            Some(info.user_agent.as_str())
        );
        assert_eq!(
            info.terminal.user_agent_token,
            codex_terminal_detection::user_agent()
        );
    }

    #[test]
    fn render_human_includes_key_fields() {
        let info = collect_info().expect("info should collect");
        let rendered = render_human(&info);
        assert!(rendered.contains("Originator: "));
        assert!(rendered.contains("User-Agent: "));
        assert!(rendered.contains("Headers:\n"));
        assert!(rendered.contains("  originator: "));
        assert!(rendered.contains("  user-agent: "));
    }
}
