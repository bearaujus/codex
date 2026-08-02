//! MCP result decoding and lossless transcript-source extraction.

use super::*;

#[derive(Debug)]
struct McpImageOutputCell;

impl HistoryCell for McpImageOutputCell {
    fn display_lines(&self, _width: u16) -> Vec<Line<'static>> {
        vec!["tool result (image output)".into()]
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        vec![Line::from("tool result (image output)")]
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct McpOutputStats {
    pub(super) lines: usize,
    pub(super) characters: usize,
}

impl McpOutputStats {
    pub(super) fn from_sources(sources: &[String]) -> Self {
        sources.iter().fold(Self::default(), |mut stats, source| {
            stats.characters += source.chars().count();
            stats.lines += source.lines().count().max(usize::from(!source.is_empty()));
            stats
        })
    }
}

pub(super) fn mcp_result_sources(
    result: &Result<codex_protocol::mcp::CallToolResult, String>,
) -> Vec<String> {
    match result {
        Err(error) => vec![format!("Error: {error}")],
        Ok(result) => {
            let mut sources = result
                .content
                .iter()
                .map(mcp_content_source)
                .collect::<Vec<_>>();
            if let Some(structured) = result.structured_content.as_ref() {
                let structured = serde_json::to_string_pretty(structured)
                    .unwrap_or_else(|_| structured.to_string());
                sources.push(format!("Structured output:\n{structured}"));
            }
            sources
        }
    }
}

fn mcp_content_source(block: &serde_json::Value) -> String {
    let Ok(content) = serde_json::from_value::<rmcp::model::ContentBlock>(block.clone()) else {
        return serde_json::to_string_pretty(block).unwrap_or_else(|_| block.to_string());
    };
    match content {
        rmcp::model::ContentBlock::Text(text) => text.text,
        rmcp::model::ContentBlock::Image(_) => "<image content>".to_string(),
        rmcp::model::ContentBlock::Audio(_) => "<audio content>".to_string(),
        rmcp::model::ContentBlock::Resource(resource) => {
            let uri = match resource.resource {
                rmcp::model::ResourceContents::TextResourceContents { uri, .. } => uri,
                rmcp::model::ResourceContents::BlobResourceContents { uri, .. } => uri,
                _ => return "<unknown embedded resource>".to_string(),
            };
            format!("embedded resource: {uri}")
        }
        rmcp::model::ContentBlock::ResourceLink(link) => format!("link: {}", link.uri),
        _ => serde_json::to_string_pretty(block).unwrap_or_else(|_| block.to_string()),
    }
}

pub(super) fn image_output_cell(
    result: &Result<codex_protocol::mcp::CallToolResult, String>,
) -> Option<Box<dyn HistoryCell>> {
    result
        .as_ref()
        .ok()?
        .content
        .iter()
        .find_map(decode_mcp_image)?;
    Some(Box::new(McpImageOutputCell))
}

fn decode_mcp_image(block: &serde_json::Value) -> Option<DynamicImage> {
    let content = serde_json::from_value::<rmcp::model::ContentBlock>(block.clone()).ok()?;
    let rmcp::model::ContentBlock::Image(image) = content else {
        return None;
    };
    let base64_data = if let Some(data_url) = image.data.strip_prefix("data:") {
        data_url.split_once(',')?.1
    } else {
        image.data.as_str()
    };
    let raw_data = base64::engine::general_purpose::STANDARD
        .decode(base64_data)
        .map_err(|error| {
            error!("Failed to decode image data: {error}");
            error
        })
        .ok()?;
    let reader = ImageReader::new(Cursor::new(raw_data))
        .with_guessed_format()
        .map_err(|error| {
            error!("Failed to guess image format: {error}");
            error
        })
        .ok()?;
    reader
        .decode()
        .map_err(|error| {
            error!("Image decoding failed: {error}");
            error
        })
        .ok()
}
