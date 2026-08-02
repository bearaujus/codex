//! MCP inventory and status history cells.

use super::*;

fn mcp_auth_status_label(status: McpAuthStatus) -> &'static str {
    match status {
        McpAuthStatus::Unknown => "Unknown",
        McpAuthStatus::Unsupported => "Unsupported",
        McpAuthStatus::NotLoggedIn => "Not logged in",
        McpAuthStatus::BearerToken => "Bearer token",
        McpAuthStatus::OAuth => "OAuth",
    }
}
/// Render a summary of configured MCP servers from the current `Config`.
pub(crate) fn empty_mcp_output() -> WebHyperlinkHistoryCell {
    let mut docs_line = HyperlinkLine::new(Line::from("    See the "));
    docs_line.push_span(
        "MCP docs".underlined(),
        Some("https://developers.openai.com/codex/mcp"),
    );
    docs_line.push_span(" to configure them.".into(), /*destination*/ None);

    let lines = vec![
        HyperlinkLine::new("/mcp".magenta().into()),
        HyperlinkLine::from(""),
        HyperlinkLine::new(vec!["🔌  ".into(), "MCP Tools".bold()].into()),
        HyperlinkLine::from(""),
        HyperlinkLine::new("  • No MCP servers configured.".italic().into()),
        docs_line.style(Style::default().add_modifier(Modifier::DIM)),
    ];

    WebHyperlinkHistoryCell::new_hyperlink_lines(lines)
}

#[cfg(test)]
/// Render MCP tools grouped by connection using the fully-qualified tool names.
pub(crate) fn new_mcp_tools_output(
    config: &Config,
    tools: HashMap<String, codex_protocol::mcp::Tool>,
    resources: HashMap<String, Vec<Resource>>,
    resource_templates: HashMap<String, Vec<ResourceTemplate>>,
    auth_statuses: &HashMap<String, McpAuthStatus>,
) -> PlainHistoryCell {
    let mut lines: Vec<Line<'static>> = vec![
        "/mcp".magenta().into(),
        "".into(),
        vec!["🔌  ".into(), "MCP Tools".bold()].into(),
        "".into(),
    ];

    if tools.is_empty() {
        lines.push("  • No MCP tools available.".italic().into());
        lines.push("".into());
    }

    let effective_servers = config.mcp_servers.get().clone();
    let mut servers: Vec<_> = effective_servers.iter().collect();
    servers.sort_by_key(|(server, _)| *server);

    for (server, cfg) in servers {
        let prefix = qualified_mcp_tool_name_prefix(server);
        let mut names: Vec<String> = tools
            .keys()
            .filter(|k| k.starts_with(&prefix))
            .map(|k| k[prefix.len()..].to_string())
            .collect();
        names.sort();

        let auth_status = auth_statuses
            .get(server.as_str())
            .copied()
            .unwrap_or(McpAuthStatus::Unsupported);
        let mut header: Vec<Span<'static>> = vec!["  • ".into(), server.clone().into()];
        if !cfg.enabled {
            header.push(" ".into());
            header.push("(disabled)".red());
            lines.push(header.into());
            if let Some(reason) = cfg.disabled_reason.as_ref().map(ToString::to_string) {
                lines.push(vec!["    • Reason: ".into(), reason.dim()].into());
            }
            lines.push(Line::from(""));
            continue;
        }
        lines.push(header.into());
        lines.push(vec!["    • Status: ".into(), "enabled".green()].into());
        lines.push(
            vec![
                "    • Auth: ".into(),
                mcp_auth_status_label(auth_status).into(),
            ]
            .into(),
        );

        match &cfg.transport {
            McpServerTransportConfig::Stdio {
                command,
                args,
                env,
                env_vars,
                cwd,
            } => {
                let args_suffix = if args.is_empty() {
                    String::new()
                } else {
                    format!(" {}", args.join(" "))
                };
                let cmd_display = format!("{command}{args_suffix}");
                lines.push(vec!["    • Command: ".into(), cmd_display.into()].into());

                if let Some(cwd) = cwd.as_ref() {
                    lines.push(vec!["    • Cwd: ".into(), cwd.to_string().into()].into());
                }

                let env_display = format_env_display(env.as_ref(), env_vars);
                if env_display != "-" {
                    lines.push(vec!["    • Env: ".into(), env_display.into()].into());
                }
            }
            McpServerTransportConfig::StreamableHttp {
                url,
                http_headers,
                env_http_headers,
                ..
            } => {
                lines.push(vec!["    • URL: ".into(), url.clone().into()].into());
                if let Some(headers) = http_headers.as_ref()
                    && !headers.is_empty()
                {
                    let mut pairs: Vec<_> = headers.iter().collect();
                    pairs.sort_by_key(|(name, _)| *name);
                    let display = pairs
                        .into_iter()
                        .map(|(name, _)| format!("{name}=*****"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    lines.push(vec!["    • HTTP headers: ".into(), display.into()].into());
                }
                if let Some(headers) = env_http_headers.as_ref()
                    && !headers.is_empty()
                {
                    let mut pairs: Vec<_> = headers.iter().collect();
                    pairs.sort_by_key(|(name, _)| *name);
                    let display = pairs
                        .into_iter()
                        .map(|(name, var)| format!("{name}={var}"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    lines.push(vec!["    • Env HTTP headers: ".into(), display.into()].into());
                }
            }
        }

        if names.is_empty() {
            lines.push("    • Tools: (none)".into());
        } else {
            lines.push(vec!["    • Tools: ".into(), names.join(", ").into()].into());
        }

        let server_resources: Vec<Resource> =
            resources.get(server.as_str()).cloned().unwrap_or_default();
        if server_resources.is_empty() {
            lines.push("    • Resources: (none)".into());
        } else {
            let mut spans: Vec<Span<'static>> = vec!["    • Resources: ".into()];

            for (idx, resource) in server_resources.iter().enumerate() {
                if idx > 0 {
                    spans.push(", ".into());
                }

                let label = resource.title.as_ref().unwrap_or(&resource.name);
                spans.push(label.clone().into());
                spans.push(" ".into());
                spans.push(format!("({})", resource.uri).dim());
            }

            lines.push(spans.into());
        }

        let server_templates: Vec<ResourceTemplate> = resource_templates
            .get(server.as_str())
            .cloned()
            .unwrap_or_default();
        if server_templates.is_empty() {
            lines.push("    • Resource templates: (none)".into());
        } else {
            let mut spans: Vec<Span<'static>> = vec!["    • Resource templates: ".into()];

            for (idx, template) in server_templates.iter().enumerate() {
                if idx > 0 {
                    spans.push(", ".into());
                }

                let label = template.title.as_ref().unwrap_or(&template.name);
                spans.push(label.clone().into());
                spans.push(" ".into());
                spans.push(format!("({})", template.uri_template).dim());
            }

            lines.push(spans.into());
        }

        lines.push(Line::from(""));
    }

    PlainHistoryCell { lines }
}

/// Build the `/mcp` history cell from app-server `McpServerStatus` responses.
///
/// The server list comes directly from the app-server status response, sorted
/// alphabetically. The TUI deliberately does not enrich these rows from
/// client-local config because the app-server owns the remote MCP state.
///
/// This mirrors the layout of [`new_mcp_tools_output`] but sources data from
/// the paginated RPC response rather than the in-process `McpManager`. The
/// `detail` flag controls whether resources and resource templates are rendered.
pub(crate) fn new_mcp_tools_output_from_statuses(
    statuses: &[McpServerStatus],
    detail: McpServerStatusDetail,
) -> PlainHistoryCell {
    let mut lines: Vec<Line<'static>> = vec![
        "/mcp".magenta().into(),
        "".into(),
        vec!["🔌  ".into(), "MCP Tools".bold()].into(),
        "".into(),
    ];

    let mut statuses = statuses.iter().collect::<Vec<_>>();
    statuses.sort_by(|a, b| a.name.cmp(&b.name));

    let has_any_tools = statuses.iter().any(|status| !status.tools.is_empty());
    if !has_any_tools {
        lines.push("  • No MCP tools available.".italic().into());
        lines.push("".into());
    }

    for status in statuses {
        let header: Vec<Span<'static>> = vec!["  • ".into(), status.name.clone().into()];

        lines.push(header.into());
        let auth_status = match status.auth_status {
            codex_app_server_protocol::McpAuthStatus::Unknown => McpAuthStatus::Unknown,
            codex_app_server_protocol::McpAuthStatus::Unsupported => McpAuthStatus::Unsupported,
            codex_app_server_protocol::McpAuthStatus::NotLoggedIn => McpAuthStatus::NotLoggedIn,
            codex_app_server_protocol::McpAuthStatus::BearerToken => McpAuthStatus::BearerToken,
            codex_app_server_protocol::McpAuthStatus::OAuth => McpAuthStatus::OAuth,
        };
        lines.push(
            vec![
                "    • Auth: ".into(),
                mcp_auth_status_label(auth_status).into(),
            ]
            .into(),
        );

        let mut names = status.tools.keys().cloned().collect::<Vec<_>>();
        names.sort();
        if names.is_empty() {
            lines.push("    • Tools: (none)".into());
        } else {
            lines.push(vec!["    • Tools: ".into(), names.join(", ").into()].into());
        }

        if matches!(detail, McpServerStatusDetail::Full) {
            let server_resources = status.resources.clone();
            if server_resources.is_empty() {
                lines.push("    • Resources: (none)".into());
            } else {
                let mut spans: Vec<Span<'static>> = vec!["    • Resources: ".into()];

                for (idx, resource) in server_resources.iter().enumerate() {
                    if idx > 0 {
                        spans.push(", ".into());
                    }

                    let label = resource.title.as_ref().unwrap_or(&resource.name);
                    spans.push(label.clone().into());
                    spans.push(" ".into());
                    spans.push(format!("({})", resource.uri).dim());
                }

                lines.push(spans.into());
            }

            let server_templates = status.resource_templates.clone();
            if server_templates.is_empty() {
                lines.push("    • Resource templates: (none)".into());
            } else {
                let mut spans: Vec<Span<'static>> = vec!["    • Resource templates: ".into()];

                for (idx, template) in server_templates.iter().enumerate() {
                    if idx > 0 {
                        spans.push(", ".into());
                    }

                    let label = template.title.as_ref().unwrap_or(&template.name);
                    spans.push(label.clone().into());
                    spans.push(" ".into());
                    spans.push(format!("({})", template.uri_template).dim());
                }

                lines.push(spans.into());
            }
        }

        lines.push(Line::from(""));
    }

    PlainHistoryCell { lines }
}
/// A transient history cell that shows an animated spinner while the MCP
/// inventory RPC is in flight.
///
/// Inserted as the `active_cell` by `ChatWidget::add_mcp_output()` and removed
/// once the fetch completes. The app removes committed copies from transcript
/// history, while `ChatWidget::clear_mcp_inventory_loading()` only clears the
/// in-flight `active_cell`.
#[derive(Debug)]
pub(crate) struct McpInventoryLoadingCell {
    start_time: Instant,
    animations_enabled: bool,
}

impl McpInventoryLoadingCell {
    pub(crate) fn new(animations_enabled: bool) -> Self {
        Self {
            start_time: Instant::now(),
            animations_enabled,
        }
    }
}

impl HistoryCell for McpInventoryLoadingCell {
    fn display_lines(&self, _width: u16) -> Vec<Line<'static>> {
        vec![
            vec![
                activity_indicator(
                    Some(self.start_time),
                    MotionMode::from_animations_enabled(self.animations_enabled),
                    ReducedMotionIndicator::StaticBullet,
                )
                .unwrap_or_else(|| "•".dim()),
                " ".into(),
                "Loading MCP inventory".bold(),
                "…".dim(),
            ]
            .into(),
        ]
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        vec![Line::from("Loading MCP inventory...")]
    }

    fn transcript_animation_tick(&self) -> Option<u64> {
        if !self.animations_enabled {
            return None;
        }
        Some((self.start_time.elapsed().as_millis() / 50) as u64)
    }
}

/// Convenience constructor for [`McpInventoryLoadingCell`].
pub(crate) fn new_mcp_inventory_loading(animations_enabled: bool) -> McpInventoryLoadingCell {
    McpInventoryLoadingCell::new(animations_enabled)
}
