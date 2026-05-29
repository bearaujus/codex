use super::*;
use codex_extension_api::ExtensionData;
use codex_extension_api::TurnItemContributor;
use codex_protocol::items::AgentMessageContent;
use codex_protocol::items::AgentMessageItem;
use codex_protocol::items::HookPromptFragment;
use codex_protocol::items::HookPromptItem;
use codex_protocol::items::ReasoningItem;
use codex_protocol::items::UserMessageItem;
use codex_protocol::items::WebSearchItem;
use codex_protocol::models::WebSearchAction;
use pretty_assertions::assert_eq;
use std::sync::Arc;

struct RewriteAgentMessageContributor;

#[async_trait::async_trait]
impl TurnItemContributor for RewriteAgentMessageContributor {
    async fn contribute(
        &self,
        _thread_store: &ExtensionData,
        _turn_store: &ExtensionData,
        item: &mut TurnItem,
    ) -> Result<(), String> {
        if let TurnItem::AgentMessage(agent_message) = item {
            agent_message.content = vec![AgentMessageContent::Text {
                text: "plan contributed assistant text".to_string(),
            }];
        }
        Ok(())
    }
}

fn assistant_output_text(text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: Some("msg-1".to_string()),
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text: text.to_string(),
        }],
        phase: None,
    }
}

#[tokio::test]
async fn plan_mode_uses_contributed_turn_item_for_last_agent_message() {
    let (mut session, turn_context) = crate::session::tests::make_session_and_context().await;
    let mut builder = codex_extension_api::ExtensionRegistryBuilder::new();
    builder.turn_item_contributor(Arc::new(RewriteAgentMessageContributor));
    session.services.extensions = Arc::new(builder.build());
    let turn_store = ExtensionData::new(turn_context.sub_id.clone());
    let mut state = PlanModeStreamState::new(&turn_context.sub_id);
    let mut last_agent_message = None;
    let item = assistant_output_text("original assistant text");

    let handled = handle_assistant_item_done_in_plan_mode(
        &session,
        &turn_context,
        &turn_store,
        &item,
        &mut state,
        /*previously_active_item*/ None,
        &mut last_agent_message,
    )
    .await;

    assert!(handled);
    assert_eq!(
        last_agent_message.as_deref(),
        Some("plan contributed assistant text")
    );
}

#[test]
fn turn_item_blocks_account_pool_retry_ignores_only_user_originated_items() {
    assert_eq!(
        turn_item_blocks_account_pool_retry(&TurnItem::UserMessage(UserMessageItem {
            id: "user".to_string(),
            content: Vec::new(),
        })),
        false
    );
    assert_eq!(
        turn_item_blocks_account_pool_retry(&TurnItem::HookPrompt(HookPromptItem {
            id: "hook".to_string(),
            fragments: vec![HookPromptFragment {
                text: "hook".to_string(),
                hook_run_id: "run-1".to_string(),
            }],
        })),
        false
    );
    assert_eq!(
        turn_item_blocks_account_pool_retry(&TurnItem::AgentMessage(AgentMessageItem {
            id: "assistant".to_string(),
            content: vec![AgentMessageContent::Text {
                text: "hello".to_string(),
            }],
            phase: None,
            memory_citation: None,
        })),
        true
    );
    assert_eq!(
        turn_item_blocks_account_pool_retry(&TurnItem::Reasoning(ReasoningItem {
            id: "reasoning".to_string(),
            summary_text: vec!["thinking".to_string()],
            raw_content: Vec::new(),
        })),
        true
    );
    assert_eq!(
        turn_item_blocks_account_pool_retry(&TurnItem::WebSearch(WebSearchItem {
            id: "search".to_string(),
            query: "codex".to_string(),
            action: WebSearchAction::Search {
                query: Some("codex".to_string()),
                queries: None,
            },
        })),
        true
    );
}
