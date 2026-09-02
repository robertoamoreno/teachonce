//! A chat-completions client for any OpenAI-compatible server.
//!
//! These types are hand-written rather than pulled from an SDK on purpose. The
//! whole point of this crate is that you can point it at Ollama, LM Studio,
//! llama.cpp's server, vLLM, OpenRouter or api.openai.com and have it work, and
//! those servers disagree in small ways that a strict SDK turns into hard
//! errors:
//!
//! * some omit `id` on a tool call, or reuse the same id across calls;
//! * some return `arguments` as a JSON *object* instead of a JSON string;
//! * some ignore `tool_choice` entirely;
//! * some reject unknown request fields, others require ones OpenAI treats as
//!   optional.
//!
//! Everything here is written to be permissive on the way in and conservative on
//! the way out.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use skillrec_core::config::LlmConfig;

/// One part of a multimodal message.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text { text: String },
    ImageUrl { image_url: ImageUrl },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageUrl {
    pub url: String,
}

impl ContentPart {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    /// Inline a JPEG as a data URL — the only image transport every
    /// OpenAI-compatible server accepts, and the only one that works for a local
    /// model that cannot reach our filesystem.
    pub fn jpeg(bytes: &[u8]) -> Self {
        use base64::Engine;
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        Self::ImageUrl { image_url: ImageUrl { url: format!("data:image/jpeg;base64,{encoded}") } }
    }
}

/// Message content: a plain string, or parts when images are involved.
///
/// Sent as a bare string whenever possible — some local servers choke on the
/// array form for text-only messages even though the spec allows it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Content {
    Text(String),
    Parts(Vec<ContentPart>),
}

impl Content {
    pub fn as_text(&self) -> String {
        match self {
            Self::Text(text) => text.clone(),
            Self::Parts(parts) => parts
                .iter()
                .filter_map(|part| match part {
                    ContentPart::Text { text } => Some(text.as_str()),
                    ContentPart::ImageUrl { .. } => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }
}

/// A tool call the model asked for.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    #[serde(default)]
    pub id: String,
    #[serde(default = "default_tool_type", rename = "type")]
    pub call_type: String,
    pub function: FunctionCall,
}

fn default_tool_type() -> String {
    "function".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    /// Spec says this is a JSON *string*. Several local servers send an object.
    #[serde(default)]
    pub arguments: Value,
}

impl FunctionCall {
    /// Parse the arguments, tolerating both encodings.
    pub fn parsed_arguments(&self) -> Result<Value> {
        match &self.arguments {
            Value::String(raw) if raw.trim().is_empty() => Ok(Value::Object(Default::default())),
            Value::String(raw) => serde_json::from_str(raw)
                .with_context(|| format!("tool call {} sent unparseable arguments", self.name)),
            Value::Null => Ok(Value::Object(Default::default())),
            other => Ok(other.clone()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// One message in the conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Content>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    pub fn system(text: impl Into<String>) -> Self {
        Self::simple(Role::System, Content::Text(text.into()))
    }

    pub fn user(text: impl Into<String>) -> Self {
        Self::simple(Role::User, Content::Text(text.into()))
    }

    pub fn user_parts(parts: Vec<ContentPart>) -> Self {
        Self::simple(Role::User, Content::Parts(parts))
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        Self::simple(Role::Assistant, Content::Text(text.into()))
    }

    /// The reply to a tool call. `tool_call_id` must echo the call's id or the
    /// server will reject the next request as an unmatched tool response.
    pub fn tool_result(call_id: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: Some(Content::Text(text.into())),
            tool_calls: Vec::new(),
            tool_call_id: Some(call_id.into()),
        }
    }

    /// A tool reply carrying images, for `get_frames`.
    ///
    /// The images ride in a *user* message immediately after the tool reply
    /// rather than inside it: the spec gives tool messages string content only,
    /// and servers that enforce that reject image parts on a tool role.
    pub fn tool_images(text: impl Into<String>, images: Vec<ContentPart>) -> Self {
        let mut parts = vec![ContentPart::text(text)];
        parts.extend(images);
        Self::user_parts(parts)
    }

    fn simple(role: Role, content: Content) -> Self {
        Self { role, content: Some(content), tool_calls: Vec::new(), tool_call_id: None }
    }

    pub fn text(&self) -> String {
        self.content.as_ref().map(Content::as_text).unwrap_or_default()
    }
}

/// A tool the model may call.
#[derive(Debug, Clone, Serialize)]
pub struct ToolDef {
    #[serde(rename = "type")]
    pub tool_type: &'static str,
    pub function: FunctionDef,
}

#[derive(Debug, Clone, Serialize)]
pub struct FunctionDef {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

impl ToolDef {
    pub fn new(name: &str, description: &str, parameters: Value) -> Self {
        Self {
            tool_type: "function",
            function: FunctionDef {
                name: name.to_string(),
                description: description.to_string(),
                parameters,
            },
        }
    }
}

#[derive(Debug, Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [ChatMessage],
    temperature: f32,
    max_tokens: u32,
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    tools: &'a [ToolDef],
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<&'a str>,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    #[serde(default)]
    choices: Vec<Choice>,
    #[serde(default)]
    error: Option<ApiError>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: ChatMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ApiError {
    message: String,
}

/// What one completion returned.
#[derive(Debug, Clone)]
pub struct Completion {
    pub message: ChatMessage,
    pub finish_reason: Option<String>,
}

/// Result of a connection test.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionTest {
    pub reachable: bool,
    pub message: String,
    /// Model ids the server advertises, when it implements `/models`.
    pub models: Vec<String>,
}

/// The one component in this workspace that opens a network connection.
pub struct LlmClient {
    http: reqwest::Client,
    config: LlmConfig,
    /// Set once the server has refused `reasoning_effort`, so it is not sent
    /// again for the life of this client.
    reasoning_rejected: AtomicBool,
}

impl LlmClient {
    pub fn new(config: LlmConfig) -> Result<Self> {
        config.validate()?;
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.request_timeout_secs))
            .build()
            .context("building the HTTP client")?;
        Ok(Self { http, config, reasoning_rejected: AtomicBool::new(false) })
    }

    /// The `reasoning_effort` to put on the wire right now.
    fn reasoning_to_send(&self) -> Option<&str> {
        if self.reasoning_rejected.load(Ordering::Relaxed) {
            return None;
        }
        self.config.reasoning_effort_to_send()
    }

    pub fn config(&self) -> &LlmConfig {
        &self.config
    }

    /// Check the endpoint answers, and list what it serves.
    ///
    /// A server without `/models` is not a failure — llama.cpp's server has no
    /// such route but completes chats perfectly well — so a 404 here reports
    /// reachable with an empty model list.
    pub async fn test_connection(&self) -> ConnectionTest {
        let response = self
            .http
            .get(self.config.models_url())
            .bearer_auth(&self.config.api_key)
            .timeout(Duration::from_secs(10))
            .send()
            .await;

        match response {
            Ok(response) if response.status().is_success() => {
                let body: Value = response.json().await.unwrap_or(Value::Null);
                let models: Vec<String> = body["data"]
                    .as_array()
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(|m| m["id"].as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                let known = models.iter().any(|m| m == &self.config.model);
                ConnectionTest {
                    reachable: true,
                    message: if models.is_empty() {
                        "Connected.".into()
                    } else if known {
                        format!("Connected. {} is available.", self.config.model)
                    } else {
                        format!(
                            "Connected, but {} is not in the server's model list.",
                            self.config.model
                        )
                    },
                    models,
                }
            }
            Ok(response) if response.status() == reqwest::StatusCode::NOT_FOUND => ConnectionTest {
                reachable: true,
                message: "Connected. The server does not list models, which is fine.".into(),
                models: Vec::new(),
            },
            Ok(response) => ConnectionTest {
                reachable: false,
                message: format!("The server answered {}.", response.status()),
                models: Vec::new(),
            },
            Err(err) => ConnectionTest {
                reachable: false,
                message: format!("Could not reach {}: {err}", self.config.base_url),
                models: Vec::new(),
            },
        }
    }

    /// One completion, with retries on transient failures.
    pub async fn complete(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolDef],
    ) -> Result<Completion> {
        const ATTEMPTS: usize = 3;
        let mut last_error = None;

        for attempt in 1..=ATTEMPTS {
            match self.complete_once(messages, tools).await {
                Ok(completion) => return Ok(completion),
                Err(err) => {
                    let retryable = is_retryable(&err);
                    tracing::warn!(attempt, retryable, "completion failed: {err:#}");
                    if !retryable || attempt == ATTEMPTS {
                        return Err(err);
                    }
                    // Backoff, because the common cause is a local server still
                    // loading a multi-gigabyte model into memory.
                    tokio::time::sleep(Duration::from_millis(500 * attempt as u64)).await;
                    last_error = Some(err);
                }
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("the completion failed")))
    }

    async fn complete_once(&self, messages: &[ChatMessage], tools: &[ToolDef]) -> Result<Completion> {
        loop {
            // Only `tool_choice: "auto"` is safe to send universally. `"required"`
            // is widely unimplemented, and a server that validates the field
            // rejects the whole request rather than ignoring it — so when the
            // agent needs a specific tool called it nudges in prose instead
            // (see `agent.rs`).
            let reasoning = self.reasoning_to_send();
            let request = ChatRequest {
                model: &self.config.model,
                messages,
                temperature: self.config.temperature,
                max_tokens: self.config.max_tokens,
                tools,
                tool_choice: (!tools.is_empty()).then_some("auto"),
                reasoning_effort: reasoning,
            };

            let response = self
                .http
                .post(self.config.chat_completions_url())
                .bearer_auth(&self.config.api_key)
                .json(&request)
                .send()
                .await
                .context("sending the completion request")?;

            let status = response.status();
            let body = response.text().await.context("reading the completion response")?;

            if !status.is_success() {
                let detail = serde_json::from_str::<ChatResponse>(&body)
                    .ok()
                    .and_then(|r| r.error.map(|e| e.message))
                    .unwrap_or_else(|| body.chars().take(400).collect());
                // `reasoning_effort` is the one optional field worth sending on
                // spec: it is what makes a thinking model usable locally. A
                // server that validates its request body rejects it outright,
                // so the field is dropped for good and the request goes again.
                if reasoning.is_some() && is_reasoning_unsupported(status, &detail) {
                    tracing::info!(
                        "the model server rejects reasoning_effort; sending without it from now on"
                    );
                    self.reasoning_rejected.store(true, Ordering::Relaxed);
                    continue;
                }
                anyhow::bail!("the model server answered {status}: {detail}");
            }

            let parsed: ChatResponse = serde_json::from_str(&body)
                .with_context(|| format!("parsing the completion response: {}", preview(&body)))?;
            if let Some(error) = parsed.error {
                anyhow::bail!("the model server reported an error: {}", error.message);
            }
            let choice = parsed
                .choices
                .into_iter()
                .next()
                .context("the model server returned no choices")?;

            return Ok(Completion { message: choice.message, finish_reason: choice.finish_reason });
        }
    }
}

/// Did a 4xx come from the server not knowing `reasoning_effort`?
///
/// OpenAI answers "Unrecognized request argument supplied: reasoning_effort"
/// or "Unsupported parameter" for models without reasoning; other servers say
/// "unknown field". Anything naming the field or the concept counts.
pub fn is_reasoning_unsupported(status: reqwest::StatusCode, detail: &str) -> bool {
    if !status.is_client_error() {
        return false;
    }
    let text = detail.to_lowercase();
    text.contains("reasoning")
        || ((text.contains("unrecognized") || text.contains("unsupported") || text.contains("unknown field"))
            && text.contains("effort"))
}

/// Did the server reject the request because this model has no tool support?
///
/// Ollama returns a hard 400 — "does not support tools" — for any model whose
/// template lacks a tool section, and that covers most vision models. Since a
/// vision model is exactly what the describer wants for frames, refusing to work
/// with them would be a real loss, so the agent detects this and falls back to
/// describing the tools in the prompt instead.
pub fn is_tools_unsupported(err: &anyhow::Error) -> bool {
    let text = format!("{err:#}").to_lowercase();
    text.contains("does not support tools")
        || text.contains("does not support tool")
        || text.contains("tools are not supported")
        || text.contains("tool use is not supported")
        || text.contains("tool calling is not supported")
        || text.contains("does not support function")
}

/// Worth another attempt? Network hiccups and 5xx/429 are; a 400 from a bad
/// request is not, and retrying it just triples the wait before the real error.
fn is_retryable(err: &anyhow::Error) -> bool {
    let text = format!("{err:#}");
    if text.contains("answered 4") && !text.contains("answered 429") {
        return false;
    }
    text.contains("answered 5")
        || text.contains("answered 429")
        || text.contains("timed out")
        || text.contains("connection")
        || text.contains("sending the completion request")
}

fn preview(body: &str) -> String {
    body.chars().take(200).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_arguments_parse_whether_string_or_object() {
        // OpenAI sends a JSON string; several local servers send the object.
        let as_string = FunctionCall {
            name: "get_events".into(),
            arguments: Value::String(r#"{"fromMs":0,"toMs":500}"#.into()),
        };
        assert_eq!(as_string.parsed_arguments().unwrap()["fromMs"], 0);

        let as_object = FunctionCall {
            name: "get_events".into(),
            arguments: serde_json::json!({"fromMs": 0, "toMs": 500}),
        };
        assert_eq!(as_object.parsed_arguments().unwrap()["toMs"], 500);
    }

    #[test]
    fn a_no_argument_tool_call_yields_an_empty_object() {
        for arguments in [Value::String(String::new()), Value::String("  ".into()), Value::Null] {
            let call = FunctionCall { name: "get_timeline".into(), arguments };
            assert!(call.parsed_arguments().unwrap().is_object());
        }
    }

    #[test]
    fn unparseable_arguments_are_an_error_not_a_silent_empty_call() {
        let call = FunctionCall {
            name: "get_frames".into(),
            arguments: Value::String("{not json".into()),
        };
        assert!(call.parsed_arguments().is_err());
    }

    #[test]
    fn text_only_messages_serialize_as_a_bare_string() {
        // The array form trips up some local servers, so text must stay a string.
        let json = serde_json::to_value(ChatMessage::user("hello")).unwrap();
        assert_eq!(json["content"], "hello");
        assert!(json.get("tool_calls").is_none(), "empty tool_calls must be omitted");
    }

    #[test]
    fn image_messages_serialize_as_parts_with_a_data_url() {
        let message = ChatMessage::tool_images("frame at 4200ms", vec![ContentPart::jpeg(&[0xFF, 0xD8])]);
        let json = serde_json::to_value(&message).unwrap();
        assert_eq!(json["content"][0]["type"], "text");
        assert_eq!(json["content"][1]["type"], "image_url");
        assert!(json["content"][1]["image_url"]["url"]
            .as_str()
            .unwrap()
            .starts_with("data:image/jpeg;base64,"));
    }

    #[test]
    fn a_tool_reply_echoes_the_call_id() {
        let json = serde_json::to_value(ChatMessage::tool_result("call_1", "{}")).unwrap();
        assert_eq!(json["role"], "tool");
        assert_eq!(json["tool_call_id"], "call_1");
    }

    #[test]
    fn responses_missing_optional_fields_still_deserialize() {
        // A tool call with no `id` and no `type` — both seen from local servers.
        let raw = r#"{"choices":[{"message":{"role":"assistant","content":null,
            "tool_calls":[{"function":{"name":"get_timeline","arguments":"{}"}}]}}]}"#;
        let parsed: ChatResponse = serde_json::from_str(raw).unwrap();
        let call = &parsed.choices[0].message.tool_calls[0];
        assert_eq!(call.function.name, "get_timeline");
        assert_eq!(call.call_type, "function");
        assert!(call.id.is_empty());
    }

    #[test]
    fn client_ids_of_retryable_failures() {
        assert!(is_retryable(&anyhow::anyhow!("the model server answered 503: busy")));
        assert!(is_retryable(&anyhow::anyhow!("the model server answered 429: slow down")));
        assert!(is_retryable(&anyhow::anyhow!("operation timed out")));
        // A malformed request will fail identically three times.
        assert!(!is_retryable(&anyhow::anyhow!("the model server answered 400: bad model")));
        assert!(!is_retryable(&anyhow::anyhow!("the model server answered 401: no key")));
    }

    #[test]
    fn reasoning_rejections_are_recognised_only_on_client_errors_that_name_it() {
        use reqwest::StatusCode;
        assert!(is_reasoning_unsupported(
            StatusCode::BAD_REQUEST,
            "Unrecognized request argument supplied: reasoning_effort"
        ));
        assert!(is_reasoning_unsupported(
            StatusCode::BAD_REQUEST,
            "Unsupported parameter: 'reasoning_effort' is not supported with this model."
        ));
        assert!(is_reasoning_unsupported(StatusCode::BAD_REQUEST, "this model does not support reasoning"));
        // A 400 about something else, or a 500, must surface as the real error.
        assert!(!is_reasoning_unsupported(StatusCode::BAD_REQUEST, "model 'nope' not found"));
        assert!(!is_reasoning_unsupported(StatusCode::INTERNAL_SERVER_ERROR, "reasoning_effort"));
    }

    #[test]
    fn reasoning_effort_rides_on_the_request_only_when_configured() {
        let messages = [ChatMessage::user("hi")];
        let with = ChatRequest {
            model: "m",
            messages: &messages,
            temperature: 0.0,
            max_tokens: 1,
            tools: &[],
            tool_choice: None,
            reasoning_effort: Some("none"),
        };
        let json = serde_json::to_value(&with).unwrap();
        assert_eq!(json["reasoning_effort"], "none");
        let without = ChatRequest { reasoning_effort: None, ..with };
        assert!(serde_json::to_value(&without).unwrap().get("reasoning_effort").is_none());
    }

    #[test]
    fn an_invalid_config_is_refused_before_any_request() {
        let config = LlmConfig { base_url: "not-a-url".into(), ..Default::default() };
        assert!(LlmClient::new(config).is_err());
    }
}
