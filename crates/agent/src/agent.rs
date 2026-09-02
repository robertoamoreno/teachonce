//! The multi-turn tool loop.
//!
//! Every agent in this app has the same shape: a system brief, a set of read
//! tools, and exactly one **terminal tool** whose call ends the turn and carries
//! the result. `submit_analysis` for the describer, `propose_plan` and
//! `submit_skill` for the builder.
//!
//! Making the terminal tool explicit is what lets this loop work against weak
//! local models as well as strong hosted ones. When the model narrates instead
//! of calling it, the loop nudges. When it emits the call as bare JSON in a
//! content string — which small models do constantly — the loop recovers it.



use anyhow::{Context, Result};
use serde_json::Value;

use crate::client::{ChatMessage, Completion, ContentPart, LlmClient, ToolCall, ToolDef};

/// What a tool returns to the model.
pub enum ToolOutput {
    /// Plain text, delivered as a tool message.
    Text(String),
    /// Text plus images, delivered as a tool message followed by a user message
    /// carrying the image parts.
    Images { text: String, images: Vec<ContentPart> },
    /// The terminal tool fired: the turn is over and this is its payload.
    Terminal(Value),
}

impl ToolOutput {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text(text.into())
    }

    /// Serialize a value as the tool's reply.
    pub fn json<T: serde::Serialize>(value: &T) -> Self {
        Self::Text(serde_json::to_string(value).unwrap_or_else(|_| "{}".into()))
    }
}

/// Something the model can call.
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn definition(&self) -> ToolDef;
    /// True when calling this ends the turn.
    fn is_terminal(&self) -> bool {
        false
    }
    async fn call(&self, arguments: Value) -> Result<ToolOutput>;
}

/// Progress, streamed to the UI so a long local-model turn is not a frozen bar.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentProgress {
    pub session_id: String,
    pub phase: String,
    pub message: String,
}

/// How many completions one turn may take before we give up.
///
/// Generous, because a careful describer legitimately calls `get_timeline`,
/// `get_narration`, a few `get_events`, a few `get_frames`, then submits.
const MAX_ITERATIONS: usize = 24;

/// A conversation with a model, holding its tools.
pub struct Agent {
    client: LlmClient,
    tools: Vec<Box<dyn Tool>>,
    messages: Vec<ChatMessage>,
    session_id: String,
}

impl Agent {
    pub fn new(
        client: LlmClient,
        session_id: impl Into<String>,
        system: String,
        tools: Vec<Box<dyn Tool>>,
    ) -> Self {
        Self {
            client,
            tools,
            messages: vec![ChatMessage::system(system)],
            session_id: session_id.into(),
        }
    }

    pub fn model(&self) -> &str {
        &self.client.config().model
    }

    /// The conversation so far, so a follow-up turn can continue it.
    pub fn history(&self) -> &[ChatMessage] {
        &self.messages
    }

    /// Run one turn: send `prompt`, service tool calls, return the terminal
    /// tool's payload.
    pub async fn run_turn(
        &mut self,
        prompt: impl Into<String>,
        on_progress: &(dyn Fn(AgentProgress) + Send + Sync),
    ) -> Result<Value> {
        self.messages.push(ChatMessage::user(prompt.into()));
        let definitions: Vec<ToolDef> = self.tools.iter().map(|t| t.definition()).collect();
        let terminal: Vec<&str> =
            self.tools.iter().filter(|t| t.is_terminal()).map(|t| t.name()).collect();
        let mut nudged = false;
        // Set once the server tells us this model cannot do native tool calls.
        // From then on the schemas ride in the prompt and replies are recovered
        // from the content — see `is_tools_unsupported`.
        let mut prompted_tools = false;

        for iteration in 1..=MAX_ITERATIONS {
            // A local model can sit in one completion for minutes. Saying so
            // before each request keeps the progress line honest: the user sees
            // which turn is running, not a message frozen since the first one.
            on_progress(AgentProgress {
                session_id: self.session_id.clone(),
                phase: "model".into(),
                message: if iteration == 1 {
                    "Waiting for the model…".into()
                } else {
                    format!("Waiting for the model (turn {iteration})…")
                },
            });

            let sent: &[ToolDef] = if prompted_tools { &[] } else { &definitions };
            let completion = match self.client.complete(&self.messages, sent).await {
                Ok(completion) => completion,
                Err(err) if !prompted_tools && crate::client::is_tools_unsupported(&err) => {
                    tracing::info!(
                        "the model has no native tool support; describing the tools in the prompt"
                    );
                    prompted_tools = true;
                    self.messages.push(ChatMessage::user(render_tool_manual(&definitions)));
                    continue;
                }
                Err(err) => return Err(err),
            };

            // Without the tool channel, any tool may arrive as prose JSON — not
            // just the terminal one.
            let recoverable: Vec<&str> = if prompted_tools {
                definitions.iter().map(|d| d.function.name.as_str()).collect()
            } else {
                terminal.clone()
            };
            let calls = extract_calls(&completion, &recoverable);

            if calls.is_empty() {
                // The model replied in prose. Once, we tell it what we actually
                // need; a second failure means it is not going to comply and a
                // clear error beats burning the whole iteration budget.
                if nudged {
                    anyhow::bail!(
                        "the model answered with prose instead of calling {}: {}",
                        terminal.join(" or "),
                        truncate(&completion.message.text(), 300)
                    );
                }
                nudged = true;
                self.messages.push(completion.message);
                self.messages.push(ChatMessage::user(format!(
                    "Call the {} tool now with your best answer. Reply with the tool call only, \
                     not prose.",
                    terminal.join(" or ")
                )));
                continue;
            }

            self.messages.push(completion.message);

            for call in calls {
                let name = call.function.name.clone();
                let call_id = if call.id.is_empty() {
                    // Some servers omit the id; the tool reply must still carry
                    // one or the next request is rejected as unmatched.
                    format!("call_{iteration}_{name}")
                } else {
                    call.id.clone()
                };

                on_progress(AgentProgress {
                    session_id: self.session_id.clone(),
                    phase: "tool".into(),
                    message: format!("Running {name}…"),
                });

                let Some(tool) = self.tools.iter().find(|t| t.name() == name) else {
                    // Hallucinated tool. Answering with the real list recovers
                    // far more often than failing the turn.
                    self.messages.push(ChatMessage::tool_result(
                        &call_id,
                        format!(
                            "There is no tool called {name}. Available tools: {}.",
                            definitions
                                .iter()
                                .map(|d| d.function.name.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                    ));
                    continue;
                };

                let arguments = match call.function.parsed_arguments() {
                    Ok(arguments) => arguments,
                    Err(err) => {
                        self.messages.push(ChatMessage::tool_result(
                            &call_id,
                            format!("Those arguments were not valid JSON: {err}. Try again."),
                        ));
                        continue;
                    }
                };

                match tool.call(arguments).await {
                    Ok(ToolOutput::Terminal(value)) => return Ok(value),
                    Ok(ToolOutput::Text(text)) => {
                        self.messages.push(ChatMessage::tool_result(&call_id, text));
                    }
                    Ok(ToolOutput::Images { text, images }) => {
                        let count = images.len();
                        self.messages.push(ChatMessage::tool_result(
                            &call_id,
                            format!("{text} ({count} image(s) follow)"),
                        ));
                        self.messages.push(ChatMessage::tool_images(text, images));
                    }
                    Err(err) => {
                        // Tool failures are reported *to the model*, not thrown:
                        // "that time range has no frames" is information it can
                        // act on, and aborting the turn would waste the work.
                        tracing::warn!(tool = %name, "tool failed: {err:#}");
                        self.messages
                            .push(ChatMessage::tool_result(&call_id, format!("Error: {err:#}")));
                    }
                }
            }
        }

        anyhow::bail!(
            "the model did not finish within {MAX_ITERATIONS} steps without calling {}",
            terminal.join(" or ")
        )
    }
}

/// Describe the tools in prose, for a model whose server rejects the `tools`
/// field outright.
///
/// This is the whole fallback: the schemas the API would have carried are
/// rendered into a message, and the reply is recovered from the content by
/// [`extract_calls`]. It costs some tokens and some reliability, but it is the
/// difference between "vision models do not work" and "vision models work".
fn render_tool_manual(definitions: &[ToolDef]) -> String {
    let mut out = String::from(
        "This model cannot use the tools API, so use tools by replying with JSON instead.\n\n\
         To call a tool, reply with ONLY a JSON object of exactly this shape and nothing else:\n\
         {\"name\": \"<tool name>\", \"arguments\": { ... }}\n\n\
         You will then be given the tool's result and may call another tool. \
         Available tools:\n\n",
    );
    for definition in definitions {
        out.push_str(&format!(
            "- **{}** — {}\n  arguments: {}\n",
            definition.function.name,
            definition.function.description,
            serde_json::to_string(&definition.function.parameters).unwrap_or_default()
        ));
    }
    out.push_str("\nReply now with a single JSON tool call, no prose around it.");
    out
}

/// Pull tool calls out of a completion, recovering the ones a model wrote as
/// prose instead of emitting properly.
fn extract_calls(completion: &Completion, terminal: &[&str]) -> Vec<ToolCall> {
    if !completion.message.tool_calls.is_empty() {
        return completion.message.tool_calls.clone();
    }
    // Small local models very often write the terminal call as a bare JSON
    // object, or wrapped in a ```json fence, instead of using the tool channel.
    // Recovering it is the difference between "works on a 7B model" and not.
    let text = completion.message.text();
    for name in terminal {
        if let Some(arguments) = recover_json_call(&text, name) {
            tracing::info!(tool = name, "recovered a tool call the model wrote as text");
            return vec![ToolCall {
                id: String::new(),
                call_type: "function".into(),
                function: crate::client::FunctionCall { name: name.to_string(), arguments },
            }];
        }
    }
    Vec::new()
}

/// Find a JSON object in `text` that looks like a call to `name`.
///
/// Accepts three shapes seen in the wild:
/// `{"name":"submit_analysis","arguments":{…}}`, a fenced block containing the
/// arguments object, or a bare arguments object.
fn recover_json_call(text: &str, name: &str) -> Option<Value> {
    for candidate in json_candidates(text) {
        let Ok(value) = serde_json::from_str::<Value>(&candidate) else {
            continue;
        };
        if value["name"] == *name {
            let arguments = value.get("arguments").or_else(|| value.get("parameters"));
            if let Some(arguments) = arguments {
                return Some(arguments.clone());
            }
        }
        // A bare arguments object only counts if the text names the tool
        // somewhere — otherwise any JSON in a reply would be mistaken for a call.
        if value.is_object() && !value.as_object()?.is_empty() && text.contains(name) {
            return Some(value);
        }
    }
    None
}

/// Every balanced `{…}` span in the text, longest first.
fn json_candidates(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut spans = Vec::new();
    let mut starts = Vec::new();
    let mut in_string = false;
    let mut escaped = false;

    for (index, byte) in bytes.iter().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        match byte {
            b'\\' if in_string => escaped = true,
            b'"' => in_string = !in_string,
            b'{' if !in_string => starts.push(index),
            b'}' if !in_string => {
                if let Some(start) = starts.pop()
                    && let Some(span) = text.get(start..=index)
                {
                    spans.push(span.to_string());
                }
            }
            _ => {}
        }
    }
    spans.sort_by_key(|s| std::cmp::Reverse(s.len()));
    spans
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    text.chars().take(max).collect::<String>() + "…"
}

/// Build a JSON Schema object for a tool's parameters.
pub fn schema(properties: Value, required: &[&str]) -> Value {
    serde_json::json!({
        "type": "object",
        "properties": properties,
        "required": required,
    })
}

/// Read a millisecond field from tool arguments, tolerating the several ways a
/// model will spell it.
pub fn arg_i64(arguments: &Value, keys: &[&str]) -> Option<i64> {
    for key in keys {
        match &arguments[*key] {
            Value::Number(number) => return number.as_i64(),
            Value::String(text) => {
                if let Ok(parsed) = text.trim().parse::<i64>() {
                    return Some(parsed);
                }
            }
            _ => {}
        }
    }
    None
}

/// Read a string field, tolerating alternate spellings.
pub fn arg_str(arguments: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| arguments[*key].as_str())
        .map(str::to_string)
}

/// Deserialize tool arguments into a typed submission.
pub fn parse_arguments<T: serde::de::DeserializeOwned>(arguments: Value) -> Result<T> {
    serde_json::from_value(arguments).context("the tool arguments did not match the expected shape")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{ChatMessage, FunctionCall};

    fn completion(text: &str) -> Completion {
        Completion { message: ChatMessage::assistant(text), finish_reason: Some("stop".into()) }
    }

    #[test]
    fn a_proper_tool_call_is_used_as_is() {
        let mut message = ChatMessage::assistant("");
        message.tool_calls = vec![ToolCall {
            id: "call_1".into(),
            call_type: "function".into(),
            function: FunctionCall { name: "get_timeline".into(), arguments: Value::Null },
        }];
        let completion = Completion { message, finish_reason: None };
        let calls = extract_calls(&completion, &["submit_analysis"]);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "get_timeline");
    }

    #[test]
    fn a_terminal_call_written_as_prose_json_is_recovered() {
        let text = r#"I'm confident now.
{"name":"submit_analysis","arguments":{"title":"Check Pricing","intent":"Compare plans"}}"#;
        let calls = extract_calls(&completion(text), &["submit_analysis"]);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.arguments["title"], "Check Pricing");
    }

    #[test]
    fn a_fenced_json_block_is_recovered() {
        let text = "Here is my submit_analysis call:\n```json\n{\"title\":\"Extract Invoices\",\
                    \"intent\":\"Pull invoice totals\"}\n```";
        let calls = extract_calls(&completion(text), &["submit_analysis"]);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.arguments["intent"], "Pull invoice totals");
    }

    #[test]
    fn stray_json_that_does_not_name_the_tool_is_not_a_call() {
        // Otherwise any JSON the model quotes back would end the turn early with
        // whatever happened to be in it.
        let calls = extract_calls(&completion(r#"The page returned {"price": 99}."#), &["submit_analysis"]);
        assert!(calls.is_empty());
    }

    #[test]
    fn prose_with_no_json_yields_no_calls() {
        assert!(extract_calls(&completion("Let me think about this."), &["submit_analysis"]).is_empty());
        assert!(extract_calls(&completion(""), &["submit_analysis"]).is_empty());
    }

    #[test]
    fn braces_inside_strings_do_not_break_candidate_scanning() {
        let text = r#"{"name":"submit_analysis","arguments":{"intent":"Fix the {broken} template"}}"#;
        let calls = extract_calls(&completion(text), &["submit_analysis"]);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.arguments["intent"], "Fix the {broken} template");
    }

    #[test]
    fn millisecond_arguments_survive_being_sent_as_strings() {
        // Small models routinely quote numbers.
        let arguments = serde_json::json!({"fromMs": "1500", "to_ms": 3000});
        assert_eq!(arg_i64(&arguments, &["fromMs", "from_ms"]), Some(1500));
        assert_eq!(arg_i64(&arguments, &["toMs", "to_ms"]), Some(3000));
        assert_eq!(arg_i64(&arguments, &["missing"]), None);
        assert_eq!(arg_i64(&serde_json::json!({"fromMs": "abc"}), &["fromMs"]), None);
    }

    #[test]
    fn string_arguments_accept_either_spelling() {
        let arguments = serde_json::json!({"query": "pricing"});
        assert_eq!(arg_str(&arguments, &["q", "query"]).as_deref(), Some("pricing"));
        assert_eq!(arg_str(&arguments, &["other"]), None);
    }

    #[test]
    fn schemas_render_as_json_schema_objects() {
        let value = schema(serde_json::json!({"fromMs": {"type": "integer"}}), &["fromMs"]);
        assert_eq!(value["type"], "object");
        assert_eq!(value["required"][0], "fromMs");
    }

    #[test]
    fn long_error_text_is_truncated_for_the_message() {
        assert_eq!(truncate("abcdef", 3), "abc…");
        assert_eq!(truncate("ab", 5), "ab");
    }
}
