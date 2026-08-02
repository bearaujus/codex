//! Compact MCP invocation formatting for headers and grouped call rows.

use super::mcp_tool_call::McpInvocation;
use super::*;

pub(super) fn invocation_argument_summary(invocation: &McpInvocation) -> String {
    let Some(arguments) = invocation.arguments.as_ref() else {
        return String::new();
    };
    let serde_json::Value::Object(object) = arguments else {
        return serde_json::to_string(arguments).unwrap_or_else(|_| arguments.to_string());
    };
    let priority = ["path", "query", "url", "command", "ref_id", "name"];
    let mut parts = Vec::new();
    if let Some((key, value)) = priority
        .into_iter()
        .find_map(|key| object.get(key).map(|value| (key, value)))
    {
        parts.push(format!("{key}={}", json_scalar(value)));
    }
    for (key, value) in object {
        if parts.len() >= 3 || priority.contains(&key.as_str()) {
            continue;
        }
        if matches!(
            value,
            serde_json::Value::Number(_) | serde_json::Value::Bool(_)
        ) {
            parts.push(format!("{key}={}", json_scalar(value)));
        }
    }
    if parts.is_empty() {
        serde_json::to_string(arguments).unwrap_or_else(|_| arguments.to_string())
    } else {
        parts.join(" · ")
    }
}

fn json_scalar(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(value) => value.clone(),
        _ => value.to_string(),
    }
}

pub(super) fn format_mcp_invocation<'a>(invocation: McpInvocation) -> Line<'a> {
    let args_str = invocation
        .arguments
        .as_ref()
        .map(|value| serde_json::to_string(value).unwrap_or_else(|_| value.to_string()))
        .unwrap_or_default();
    vec![
        invocation.server.cyan(),
        ".".into(),
        invocation.tool.cyan(),
        "(".into(),
        args_str.dim(),
        ")".into(),
    ]
    .into()
}
