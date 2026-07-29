use anyhow::{Context as _, Result, anyhow};
use cloud_llm_client::predict_edits_v3::{RawCompletionRequest, RawCompletionResponse};
use futures::AsyncReadExt as _;
use gpui::{App, AppContext as _, Entity, Global, SharedString, Task, http_client};
use language::language_settings::{
    OpenAiCompatibleApiType, OpenAiCompatibleEditPredictionSettings, all_language_settings,
};
use language_model::{ApiKeyState, EnvVar, env_var};
use serde::{Deserialize, Serialize};
use std::{borrow::Cow, sync::Arc};

pub fn open_ai_compatible_api_url(cx: &App) -> SharedString {
    all_language_settings(None, cx)
        .edit_predictions
        .open_ai_compatible_api
        .as_ref()
        .map(|settings| settings.api_url.clone())
        .unwrap_or_default()
        .into()
}

pub const OPEN_AI_COMPATIBLE_CREDENTIALS_USERNAME: &str = "openai-compatible-api-token";
pub static OPEN_AI_COMPATIBLE_TOKEN_ENV_VAR: std::sync::LazyLock<EnvVar> =
    env_var!("ZED_OPEN_AI_COMPATIBLE_EDIT_PREDICTION_API_KEY");

struct GlobalOpenAiCompatibleApiKey(Entity<ApiKeyState>);

impl Global for GlobalOpenAiCompatibleApiKey {}

pub fn open_ai_compatible_api_token(cx: &mut App) -> Entity<ApiKeyState> {
    if let Some(global) = cx.try_global::<GlobalOpenAiCompatibleApiKey>() {
        return global.0.clone();
    }

    let entity = cx.new(|cx| {
        ApiKeyState::new(
            open_ai_compatible_api_url(cx),
            OPEN_AI_COMPATIBLE_TOKEN_ENV_VAR.clone(),
        )
    });
    cx.set_global(GlobalOpenAiCompatibleApiKey(entity.clone()));
    entity
}

pub fn load_open_ai_compatible_api_token(
    cx: &mut App,
) -> Task<Result<(), language_model::AuthenticateError>> {
    let credentials_provider = zed_credentials_provider::global(cx);
    let api_url = open_ai_compatible_api_url(cx);
    open_ai_compatible_api_token(cx).update(cx, |key_state, cx| {
        key_state.load_if_needed(api_url, |s| s, credentials_provider, cx)
    })
}

pub fn load_open_ai_compatible_api_key_if_needed(
    provider: settings::EditPredictionProvider,
    cx: &mut App,
) -> Option<Arc<str>> {
    if provider != settings::EditPredictionProvider::OpenAiCompatibleApi {
        return None;
    }
    _ = load_open_ai_compatible_api_token(cx);
    let url = open_ai_compatible_api_url(cx);
    return open_ai_compatible_api_token(cx).read(cx).key(&url);
}

/// The prompt payload for a custom-server edit prediction request.
pub(crate) enum FimRequestPrompt {
    /// A pre-formatted FIM prompt using model-native FIM tokens, sent to a
    /// text completion API.
    Completion(String),
    /// Raw code around the cursor, sent to a chat completion API with an
    /// instruction-based fill-in-the-middle prompt.
    Chat {
        prefix: String,
        suffix: String,
        language_name: Option<String>,
    },
}

const CHAT_FIM_SYSTEM_PROMPT: &str = "You are a code completion engine. The user message \
contains code with the cursor position marked as <CURSOR>. Output ONLY the exact code that \
should be inserted at <CURSOR>. Rules: no explanations, no markdown code fences, no repeating \
the code before or after the cursor, no <CURSOR> marker in output. Just the raw code to \
insert. If the insertion should be multiple lines, output them with proper indentation.";

#[derive(Serialize)]
struct ChatCompletionRequest {
    model: String,
    messages: Vec<ChatCompletionMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    temperature: f32,
    stop: Vec<Cow<'static, str>>,
    // Disable chain-of-thought reasoning for reasoning models (e.g. served by
    // LM Studio), which would otherwise add significant latency.
    reasoning_effort: &'static str,
}

#[derive(Serialize)]
struct ChatCompletionMessage {
    role: &'static str,
    content: String,
}

#[derive(Deserialize)]
struct ChatCompletionResponse {
    id: String,
    choices: Vec<ChatCompletionChoice>,
}

#[derive(Deserialize)]
struct ChatCompletionChoice {
    message: ChatCompletionResponseMessage,
}

#[derive(Deserialize)]
struct ChatCompletionResponseMessage {
    content: Option<String>,
}

pub(crate) async fn send_custom_server_request(
    provider: settings::EditPredictionProvider,
    settings: &OpenAiCompatibleEditPredictionSettings,
    prompt: FimRequestPrompt,
    max_tokens: u32,
    stop_tokens: Vec<String>,
    api_key: Option<Arc<str>>,
    http_client: &Arc<dyn http_client::HttpClient>,
) -> Result<(String, String)> {
    match provider {
        settings::EditPredictionProvider::Ollama => {
            let FimRequestPrompt::Completion(prompt) = prompt else {
                return Err(anyhow!("chat completions are not supported for Ollama"));
            };
            let response = crate::ollama::make_request(
                settings.clone(),
                prompt,
                stop_tokens,
                http_client.clone(),
            )
            .await?;
            Ok((response.response, response.created_at))
        }
        _ if settings.api_type == OpenAiCompatibleApiType::ChatCompletions => {
            let FimRequestPrompt::Chat {
                prefix,
                suffix,
                language_name,
            } = prompt
            else {
                return Err(anyhow!(
                    "chat completions API requires a chat FIM prompt"
                ));
            };
            let mut user_message = String::new();
            if let Some(language_name) = language_name {
                user_message.push_str(&format!("Language: {language_name}\n\n"));
            }
            user_message.push_str("Code:\n");
            user_message.push_str(&prefix);
            user_message.push_str("<CURSOR>");
            user_message.push_str(&suffix);

            let request = ChatCompletionRequest {
                model: settings.model.clone(),
                messages: vec![
                    ChatCompletionMessage {
                        role: "system",
                        content: CHAT_FIM_SYSTEM_PROMPT.to_string(),
                    },
                    ChatCompletionMessage {
                        role: "user",
                        content: user_message,
                    },
                ],
                max_tokens: Some(max_tokens),
                temperature: 0.0,
                stop: stop_tokens.into_iter().map(Cow::Owned).collect(),
                reasoning_effort: "none",
            };

            let (body, status) = post_json(settings, &request, api_key, http_client).await?;
            if !status.is_success() {
                anyhow::bail!("custom server error: {} - {}", status, body);
            }
            let parsed: ChatCompletionResponse = serde_json::from_str(&body)
                .context("Failed to parse chat completion response")?;
            let text = parsed
                .choices
                .into_iter()
                .next()
                .and_then(|choice| choice.message.content)
                .unwrap_or_default();
            Ok((clean_chat_completion(&text), parsed.id))
        }
        _ => {
            let FimRequestPrompt::Completion(prompt) = prompt else {
                return Err(anyhow!(
                    "completions API requires a pre-formatted FIM prompt"
                ));
            };
            let request = RawCompletionRequest {
                model: settings.model.clone(),
                prompt,
                max_tokens: Some(max_tokens),
                temperature: None,
                stop: stop_tokens
                    .into_iter()
                    .map(std::borrow::Cow::Owned)
                    .collect(),
                environment: None,
            };

            let (body, status) = post_json(settings, &request, api_key, http_client).await?;
            if !status.is_success() {
                anyhow::bail!("custom server error: {} - {}", status, body);
            }
            let parsed: RawCompletionResponse =
                serde_json::from_str(&body).context("Failed to parse completion response")?;
            let text = parsed
                .choices
                .into_iter()
                .next()
                .map(|choice| choice.text)
                .unwrap_or_default();
            Ok((text, parsed.id))
        }
    }
}

async fn post_json<T: Serialize>(
    settings: &OpenAiCompatibleEditPredictionSettings,
    request: &T,
    api_key: Option<Arc<str>>,
    http_client: &Arc<dyn http_client::HttpClient>,
) -> Result<(String, http_client::StatusCode)> {
    let request_body = serde_json::to_string(request)?;
    let mut http_request_builder = http_client::Request::builder()
        .method(http_client::Method::POST)
        .uri(settings.api_url.as_ref())
        .header("Content-Type", "application/json");

    if let Some(api_key) = api_key {
        http_request_builder =
            http_request_builder.header("Authorization", format!("Bearer {}", api_key));
    }

    let http_request = http_request_builder.body(http_client::AsyncBody::from(request_body))?;

    let mut response = http_client.send(http_request).await?;
    let status = response.status();

    let mut body = String::new();
    response.body_mut().read_to_string(&mut body).await?;
    Ok((body, status))
}

/// Cleans up a completion produced by a chat model: strips markdown code
/// fences (chat models tend to wrap multi-line code in them despite
/// instructions) and removes any leaked `<CURSOR>` markers.
fn clean_chat_completion(text: &str) -> String {
    let mut text = text;

    let trimmed_start = text.trim_start();
    if trimmed_start.starts_with("```") {
        // Drop the opening fence line (e.g. "```rust").
        text = match trimmed_start.find('\n') {
            Some(newline) => &trimmed_start[newline + 1..],
            None => "",
        };
        // Drop the closing fence, if present.
        let trimmed_end = text.trim_end();
        if let Some(without_fence) = trimmed_end.strip_suffix("```") {
            text = without_fence.strip_suffix('\n').unwrap_or(without_fence);
        }
    }

    text.replace("<CURSOR>", "")
}

#[cfg(test)]
mod tests {
    use super::clean_chat_completion;

    #[test]
    fn strips_markdown_fences() {
        assert_eq!(
            clean_chat_completion("```rust\nlet x = 1;\n```"),
            "let x = 1;"
        );
        assert_eq!(clean_chat_completion("```\nfoo();\n```\n"), "foo();");
        assert_eq!(
            clean_chat_completion("```python\n    return a + b\n```"),
            "    return a + b"
        );
    }

    #[test]
    fn keeps_plain_completions_untouched() {
        assert_eq!(clean_chat_completion("total += item"), "total += item");
        assert_eq!(
            clean_chat_completion("    if x {\n        y();\n    }"),
            "    if x {\n        y();\n    }"
        );
    }

    #[test]
    fn removes_cursor_markers() {
        assert_eq!(clean_chat_completion("foo<CURSOR>"), "foo");
    }
}
