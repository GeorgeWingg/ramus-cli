use crate::codex::Session;
use crate::config_types::ReasoningEffort as ReasoningEffortConfig;
use crate::config_types::ReasoningSummary as ReasoningSummaryConfig;
use crate::environment_context::ENVIRONMENT_CONTEXT_START;
use crate::environment_context::EnvironmentContext;
use crate::error::Result;
use crate::model_family::ModelFamily;
use crate::models::ContentItem;
use crate::models::ResponseItem;
use crate::openai_tools::OpenAiTool;
use crate::protocol::TokenUsage;
use codex_apply_patch::APPLY_PATCH_TOOL_INSTRUCTIONS;
use futures::Stream;
use serde::Deserialize;
use serde::Serialize;
use std::borrow::Cow;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;
use strum_macros::Display as DeriveDisplay;
use tokio::sync::mpsc;

/// The `instructions` field in the payload sent to a model should always start
/// with this content.
const BASE_INSTRUCTIONS: &str = include_str!("../prompt.md");

/// wraps user instructions message in a tag for the model to parse more easily.
const USER_INSTRUCTIONS_START: &str = "<user_instructions>\n\n";
const USER_INSTRUCTIONS_END: &str = "\n\n</user_instructions>";

/// API request payload for a single model turn.
#[derive(Default, Debug, Clone)]
pub struct Prompt {
    /// Conversation context input items.
    pub input: Vec<ResponseItem>,
    /// Optional instructions from the user to amend to the built-in agent
    /// instructions.
    pub user_instructions: Option<String>,
    /// Whether to store response on server side (disable_response_storage = !store).
    pub store: bool,

    /// A list of key-value pairs that will be added as a developer message
    /// for the model to use
    pub environment_context: Option<EnvironmentContext>,

    /// Tools available to the model, including additional tools sourced from
    /// external MCP servers.
    pub tools: Vec<OpenAiTool>,

    /// Optional override for the built-in BASE_INSTRUCTIONS.
    pub base_instructions_override: Option<String>,
}

impl Prompt {
    pub(crate) fn get_full_instructions(&self, model: &ModelFamily) -> Cow<'_, str> {
        let base = self
            .base_instructions_override
            .as_deref()
            .unwrap_or(BASE_INSTRUCTIONS);
        let mut sections: Vec<&str> = vec![base];
        if model.needs_special_apply_patch_instructions {
            sections.push(APPLY_PATCH_TOOL_INSTRUCTIONS);
        }
        Cow::Owned(sections.join("\n"))
    }

    pub(crate) fn get_formatted_input(&self) -> Vec<ResponseItem> {
        // previously, we would inject user_instructions and environment_context
        // here, but now we do that in the codex.rs file. To avoid breaking old rollouts,
        // we still check if the input starts with the user_instructions and environment_context
        // tags. If not, we inject them here.
        let mut input = self.input.clone();
        if let Some(user_instructions) = &self.user_instructions {
            if !input
                .iter()
                .any(|item| does_message_start_with_tag(item, USER_INSTRUCTIONS_START))
            {
                input.insert(0, Self::make_user_instructions_message(user_instructions));
            }
        }
        if let Some(environment_context) = &self.environment_context {
            if !input
                .iter()
                .any(|item| does_message_start_with_tag(item, ENVIRONMENT_CONTEXT_START))
            {
                input.insert(0, environment_context.clone().into());
            }
        }

        input
    }

    pub(crate) fn format_user_instructions(ui: &str) -> String {
        format!("{USER_INSTRUCTIONS_START}{ui}{USER_INSTRUCTIONS_END}")
    }

    pub(crate) fn make_user_instructions_message(ui: &str) -> ResponseItem {
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: Self::format_user_instructions(ui),
            }],
        }
    }
}

#[derive(Debug)]
pub enum ResponseEvent {
    Created,
    OutputItemDone(ResponseItem),
    Completed {
        response_id: String,
        token_usage: Option<TokenUsage>,
    },
    OutputTextDelta(String),
    ReasoningSummaryDelta(String),
    ReasoningContentDelta(String),
    ReasoningSummaryPartAdded,
}

#[derive(Debug, Serialize)]
pub(crate) struct Reasoning {
    pub(crate) effort: OpenAiReasoningEffort,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) summary: Option<OpenAiReasoningSummary>,
}

/// See https://platform.openai.com/docs/guides/reasoning?api-mode=responses#get-started-with-reasoning
#[derive(Debug, Serialize, Default, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub(crate) enum OpenAiReasoningEffort {
    Low,
    #[default]
    Medium,
    High,
}

impl From<ReasoningEffortConfig> for Option<OpenAiReasoningEffort> {
    fn from(effort: ReasoningEffortConfig) -> Self {
        match effort {
            ReasoningEffortConfig::Low => Some(OpenAiReasoningEffort::Low),
            ReasoningEffortConfig::Medium => Some(OpenAiReasoningEffort::Medium),
            ReasoningEffortConfig::High => Some(OpenAiReasoningEffort::High),
            ReasoningEffortConfig::None => None,
        }
    }
}

/// A summary of the reasoning performed by the model. This can be useful for
/// debugging and understanding the model's reasoning process.
/// See https://platform.openai.com/docs/guides/reasoning?api-mode=responses#reasoning-summaries
#[derive(Debug, Serialize, Default, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub(crate) enum OpenAiReasoningSummary {
    #[default]
    Auto,
    Concise,
    Detailed,
}

impl From<ReasoningSummaryConfig> for Option<OpenAiReasoningSummary> {
    fn from(summary: ReasoningSummaryConfig) -> Self {
        match summary {
            ReasoningSummaryConfig::Auto => Some(OpenAiReasoningSummary::Auto),
            ReasoningSummaryConfig::Concise => Some(OpenAiReasoningSummary::Concise),
            ReasoningSummaryConfig::Detailed => Some(OpenAiReasoningSummary::Detailed),
            ReasoningSummaryConfig::None => None,
        }
    }
}

/// Request object that is serialized as JSON and POST'ed when using the
/// Responses API.
#[derive(Debug, Serialize)]
pub(crate) struct ResponsesApiRequest<'a> {
    pub(crate) model: &'a str,
    pub(crate) instructions: &'a str,
    // TODO(mbolin): ResponseItem::Other should not be serialized. Currently,
    // we code defensively to avoid this case, but perhaps we should use a
    // separate enum for serialization.
    pub(crate) input: &'a Vec<ResponseItem>,
    pub(crate) tools: &'a [serde_json::Value],
    pub(crate) tool_choice: &'static str,
    pub(crate) parallel_tool_calls: bool,
    pub(crate) reasoning: Option<Reasoning>,
    /// true when using the Responses API.
    pub(crate) store: bool,
    pub(crate) stream: bool,
    pub(crate) include: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) prompt_cache_key: Option<String>,
}

pub(crate) fn create_reasoning_param_for_request(
    model_family: &ModelFamily,
    effort: ReasoningEffortConfig,
    summary: ReasoningSummaryConfig,
) -> Option<Reasoning> {
    if model_family.supports_reasoning_summaries {
        let effort: Option<OpenAiReasoningEffort> = effort.into();
        let effort = effort?;
        Some(Reasoning {
            effort,
            summary: summary.into(),
        })
    } else {
        None
    }
}

pub(crate) struct ResponseStream {
    pub(crate) rx_event: mpsc::Receiver<Result<ResponseEvent>>,
}

impl Stream for ResponseStream {
    type Item = Result<ResponseEvent>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.rx_event.poll_recv(cx)
    }
}

fn does_message_start_with_tag(message: &ResponseItem, tag: &str) -> bool {
    match message {
        ResponseItem::Message { content, .. } => {
            if !content.is_empty() {
                match &content[0] {
                    ContentItem::InputText { text } => text.starts_with(tag),
                    // only match input text
                    _ => false,
                }
            } else {
                false
            }
        }
        // only match message type
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use crate::model_family::find_family_for_model;

    use super::*;

    #[test]
    fn get_full_instructions_no_user_content() {
        let prompt = Prompt {
            user_instructions: Some("custom instruction".to_string()),
            ..Default::default()
        };
        let expected = format!("{BASE_INSTRUCTIONS}\n{APPLY_PATCH_TOOL_INSTRUCTIONS}");
        let model_family = find_family_for_model("gpt-4.1").expect("known model slug");
        let full = prompt.get_full_instructions(&model_family);
        assert_eq!(full, expected);
    }
}
