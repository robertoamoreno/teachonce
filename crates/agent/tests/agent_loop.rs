//! End-to-end tests for the agent loop against a stub OpenAI-compatible server.
//!
//! These are the tests that matter most for this crate, because the loop's whole
//! job is surviving how differently real servers behave. A stub lets us
//! reproduce the exact wire shapes — a tool call with no `id`, arguments as an
//! object instead of a string, a model that answers in prose — that would
//! otherwise only show up against somebody's Ollama at runtime.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{json, Value};
use skillrec_agent::agent::{Agent, AgentProgress, Tool, ToolOutput, schema};
use skillrec_agent::client::{LlmClient, ToolDef};
use skillrec_core::config::LlmConfig;

/// A stub chat-completions server that replays canned responses in order.
struct StubServer {
    port: u16,
    requests: Arc<std::sync::Mutex<Vec<Value>>>,
}

impl StubServer {
    /// Serve `responses` in order, one per request. The last is repeated if the
    /// loop asks for more.
    fn start(responses: Vec<Value>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = Arc::clone(&requests);
        let counter = AtomicUsize::new(0);

        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut reader = BufReader::new(stream.try_clone().unwrap());

                // Minimal HTTP/1.1: read headers, then exactly Content-Length bytes.
                let mut length = 0usize;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        break;
                    }
                    if let Some(value) = line.to_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap_or(0);
                    }
                    if line == "\r\n" || line == "\n" {
                        break;
                    }
                }
                let mut body = vec![0u8; length];
                reader.read_exact(&mut body).ok();
                if let Ok(parsed) = serde_json::from_slice::<Value>(&body) {
                    seen.lock().unwrap().push(parsed);
                }

                let index = counter.fetch_add(1, Ordering::SeqCst).min(responses.len() - 1);
                // A canned response may carry `__status` to answer with an
                // error code instead of 200; the key itself is not sent.
                let mut canned = responses[index].clone();
                let status = canned
                    .as_object_mut()
                    .and_then(|o| o.remove("__status"))
                    .and_then(|s| s.as_u64())
                    .unwrap_or(200);
                let reason = if status == 200 { "OK" } else { "Error" };
                let payload = serde_json::to_string(&canned).unwrap();
                let response = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    payload.len(),
                    payload
                );
                stream.write_all(response.as_bytes()).ok();
                stream.flush().ok();
            }
        });

        Self { port, requests }
    }

    fn config(&self) -> LlmConfig {
        LlmConfig {
            base_url: format!("http://127.0.0.1:{}/v1", self.port),
            model: "stub-model".into(),
            api_key: "test".into(),
            request_timeout_secs: 10,
            ..Default::default()
        }
    }

    fn requests(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }
}

/// A response containing one tool call.
fn tool_call(name: &str, arguments: Value, id: Option<&str>) -> Value {
    let mut call = json!({ "function": { "name": name, "arguments": arguments } });
    if let Some(id) = id {
        call["id"] = json!(id);
        call["type"] = json!("function");
    }
    json!({ "choices": [{ "message": { "role": "assistant", "content": null, "tool_calls": [call] } }] })
}

/// A response containing prose.
fn prose(text: &str) -> Value {
    json!({ "choices": [{ "message": { "role": "assistant", "content": text } }] })
}

// --- Test tools --------------------------------------------------------------

struct Timeline {
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Tool for Timeline {
    fn name(&self) -> &'static str {
        "get_timeline"
    }
    fn definition(&self) -> ToolDef {
        ToolDef::new("get_timeline", "The timeline.", schema(json!({}), &[]))
    }
    async fn call(&self, _: Value) -> anyhow::Result<ToolOutput> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput::text(r#"{"steps":[{"id":"s1","app":"Safari"}]}"#))
    }
}

struct Failing;

#[async_trait::async_trait]
impl Tool for Failing {
    fn name(&self) -> &'static str {
        "get_frames"
    }
    fn definition(&self) -> ToolDef {
        ToolDef::new("get_frames", "View frames.", schema(json!({}), &[]))
    }
    async fn call(&self, _: Value) -> anyhow::Result<ToolOutput> {
        anyhow::bail!("no frames were captured in that window")
    }
}

struct Submit {
    captured: Arc<std::sync::Mutex<Option<Value>>>,
}

#[async_trait::async_trait]
impl Tool for Submit {
    fn name(&self) -> &'static str {
        "submit_analysis"
    }
    fn is_terminal(&self) -> bool {
        true
    }
    fn definition(&self) -> ToolDef {
        ToolDef::new("submit_analysis", "Final answer.", schema(json!({}), &["intent"]))
    }
    async fn call(&self, arguments: Value) -> anyhow::Result<ToolOutput> {
        *self.captured.lock().unwrap() = Some(arguments.clone());
        Ok(ToolOutput::Terminal(arguments))
    }
}

fn noop(_: AgentProgress) {}

fn build(config: LlmConfig, captured: Arc<std::sync::Mutex<Option<Value>>>, calls: Arc<AtomicUsize>) -> Agent {
    Agent::new(
        LlmClient::new(config).unwrap(),
        "test-session",
        "You are a test agent.".into(),
        vec![
            Box::new(Timeline { calls }),
            Box::new(Failing),
            Box::new(Submit { captured }),
        ],
    )
}

// --- Tests -------------------------------------------------------------------

#[tokio::test]
async fn a_server_that_rejects_reasoning_effort_gets_it_dropped_and_the_turn_completes() {
    let server = StubServer::start(vec![
        json!({ "__status": 400, "error": { "message": "Unrecognized request argument supplied: reasoning_effort" } }),
        tool_call("submit_analysis", json!({ "intent": "x" }), Some("c1")),
    ]);
    let mut config = server.config();
    config.reasoning_effort = "none".into();
    let captured = Arc::new(std::sync::Mutex::new(None));
    let mut agent = build(config, Arc::clone(&captured), Arc::new(AtomicUsize::new(0)));

    agent.run_turn("go", &noop).await.unwrap();

    let requests = server.requests();
    assert_eq!(requests.len(), 2, "one rejected request, one retry");
    assert_eq!(requests[0]["reasoning_effort"], "none");
    assert!(requests[1].get("reasoning_effort").is_none(), "dropped after the rejection");
    assert!(captured.lock().unwrap().is_some());
}

#[tokio::test]
async fn a_reasoning_model_that_only_takes_its_default_temperature_gets_none() {
    let server = StubServer::start(vec![
        json!({ "__status": 400, "error": { "message": "litellm.BadRequestError: OpenAIException - Unsupported value: 'temperature' does not support 0.1 with this model. Only the default (1) value is supported.. Received Model Group=gpt-5.5 Available Model Group Fallbacks=None" } }),
        tool_call("submit_analysis", json!({ "intent": "x" }), Some("c1")),
        tool_call("submit_analysis", json!({ "intent": "y" }), Some("c2")),
    ]);
    let config = server.config();
    let captured = Arc::new(std::sync::Mutex::new(None));
    let mut agent = build(config, Arc::clone(&captured), Arc::new(AtomicUsize::new(0)));

    agent.run_turn("go", &noop).await.unwrap();
    agent.run_turn("again", &noop).await.unwrap();

    let requests = server.requests();
    assert_eq!(requests.len(), 3, "one rejected request, its retry, and a later turn");
    assert!(requests[0].get("temperature").is_some());
    assert!(requests[1].get("temperature").is_none(), "dropped after the rejection");
    assert!(requests[2].get("temperature").is_none(), "and stays dropped for the client's life");
    assert!(captured.lock().unwrap().is_some());
}

#[tokio::test]
async fn progress_names_every_model_turn_and_tool_call() {
    let server = StubServer::start(vec![
        tool_call("get_timeline", json!({}), Some("c1")),
        tool_call("submit_analysis", json!({ "intent": "x" }), Some("c2")),
    ]);
    let seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let sink = {
        let seen = Arc::clone(&seen);
        move |p: AgentProgress| seen.lock().unwrap().push(format!("{}:{}", p.phase, p.message))
    };
    let mut agent = build(server.config(), Arc::new(std::sync::Mutex::new(None)), Arc::new(AtomicUsize::new(0)));
    agent.run_turn("go", &sink).await.unwrap();

    let seen = seen.lock().unwrap();
    assert_eq!(seen[0], "model:Waiting for the model…");
    assert!(seen.contains(&"tool:Running get_timeline…".to_string()));
    assert!(seen.contains(&"model:Waiting for the model (turn 2)…".to_string()));
}

#[tokio::test]
async fn a_read_tool_then_a_terminal_tool_completes_the_turn() {
    let server = StubServer::start(vec![
        tool_call("get_timeline", json!("{}"), Some("call_1")),
        tool_call(
            "submit_analysis",
            json!(r#"{"intent":"Compare the pricing tiers"}"#),
            Some("call_2"),
        ),
    ]);
    let captured = Arc::new(std::sync::Mutex::new(None));
    let calls = Arc::new(AtomicUsize::new(0));
    let mut agent = build(server.config(), Arc::clone(&captured), Arc::clone(&calls));

    let result = agent.run_turn("Reconstruct the recording.", &noop).await.unwrap();
    assert_eq!(result["intent"], "Compare the pricing tiers");
    assert_eq!(calls.load(Ordering::SeqCst), 1, "the read tool actually ran");
    assert!(captured.lock().unwrap().is_some());

    // The second request must carry the first tool's reply, correctly matched.
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    let messages = requests[1]["messages"].as_array().unwrap();
    let reply = messages.iter().find(|m| m["role"] == "tool").expect("a tool reply");
    assert_eq!(reply["tool_call_id"], "call_1");
    assert!(reply["content"].as_str().unwrap().contains("Safari"));
}

#[tokio::test]
async fn a_tool_call_with_no_id_still_round_trips() {
    // Several local servers omit `id` entirely. Without a synthesised one the
    // follow-up request would be rejected as an unmatched tool response.
    let server = StubServer::start(vec![
        tool_call("get_timeline", json!({}), None),
        tool_call("submit_analysis", json!({"intent": "Ship it"}), None),
    ]);
    let captured = Arc::new(std::sync::Mutex::new(None));
    let mut agent = build(server.config(), Arc::clone(&captured), Arc::new(AtomicUsize::new(0)));

    let result = agent.run_turn("Go.", &noop).await.unwrap();
    assert_eq!(result["intent"], "Ship it");

    let messages = server.requests()[1]["messages"].as_array().unwrap().clone();
    let reply = messages.iter().find(|m| m["role"] == "tool").unwrap();
    assert!(
        reply["tool_call_id"].as_str().is_some_and(|id| !id.is_empty()),
        "a tool reply must always carry an id"
    );
}

#[tokio::test]
async fn object_arguments_are_accepted_alongside_string_arguments() {
    // OpenAI sends a JSON string; Ollama and LM Studio send the object itself.
    let server = StubServer::start(vec![tool_call(
        "submit_analysis",
        json!({"intent": "Extract the invoice totals", "title": "Extract Invoices"}),
        Some("c1"),
    )]);
    let captured = Arc::new(std::sync::Mutex::new(None));
    let mut agent = build(server.config(), Arc::clone(&captured), Arc::new(AtomicUsize::new(0)));

    let result = agent.run_turn("Go.", &noop).await.unwrap();
    assert_eq!(result["title"], "Extract Invoices");
}

#[tokio::test]
async fn a_failing_tool_is_reported_to_the_model_and_the_turn_continues() {
    // A tool error is information the model can act on — "that window has no
    // frames" — not a reason to throw away the turn's work.
    let server = StubServer::start(vec![
        tool_call("get_frames", json!({"fromMs": 0, "toMs": 100}), Some("c1")),
        tool_call("submit_analysis", json!({"intent": "Worked it out anyway"}), Some("c2")),
    ]);
    let captured = Arc::new(std::sync::Mutex::new(None));
    let mut agent = build(server.config(), Arc::clone(&captured), Arc::new(AtomicUsize::new(0)));

    let result = agent.run_turn("Go.", &noop).await.unwrap();
    assert_eq!(result["intent"], "Worked it out anyway");

    let messages = server.requests()[1]["messages"].as_array().unwrap().clone();
    let reply = messages.iter().find(|m| m["role"] == "tool").unwrap();
    assert!(reply["content"].as_str().unwrap().contains("no frames were captured"));
}

#[tokio::test]
async fn a_hallucinated_tool_gets_the_real_list_back() {
    let server = StubServer::start(vec![
        tool_call("get_screenshots", json!({}), Some("c1")),
        tool_call("submit_analysis", json!({"intent": "Recovered"}), Some("c2")),
    ]);
    let captured = Arc::new(std::sync::Mutex::new(None));
    let mut agent = build(server.config(), Arc::clone(&captured), Arc::new(AtomicUsize::new(0)));

    assert_eq!(agent.run_turn("Go.", &noop).await.unwrap()["intent"], "Recovered");

    let messages = server.requests()[1]["messages"].as_array().unwrap().clone();
    let reply = messages.iter().find(|m| m["role"] == "tool").unwrap();
    let text = reply["content"].as_str().unwrap();
    assert!(text.contains("no tool called get_screenshots"));
    assert!(text.contains("get_timeline"), "the real tools must be listed");
}

#[tokio::test]
async fn prose_is_nudged_once_then_accepted_as_a_recovered_call() {
    // The behaviour that makes small local models usable: answer in prose, get
    // told to call the tool, then emit the call as bare JSON — and still work.
    let server = StubServer::start(vec![
        prose("I think the user was comparing prices."),
        prose(r#"{"name":"submit_analysis","arguments":{"intent":"Compare prices"}}"#),
    ]);
    let captured = Arc::new(std::sync::Mutex::new(None));
    let mut agent = build(server.config(), Arc::clone(&captured), Arc::new(AtomicUsize::new(0)));

    let result = agent.run_turn("Go.", &noop).await.unwrap();
    assert_eq!(result["intent"], "Compare prices");

    // The nudge must have been sent, and it must name the tool.
    let messages = server.requests()[1]["messages"].as_array().unwrap().clone();
    let last = messages.last().unwrap();
    assert_eq!(last["role"], "user");
    assert!(last["content"].as_str().unwrap().contains("submit_analysis"));
}

#[tokio::test]
async fn a_model_that_only_ever_talks_fails_with_a_clear_message() {
    let server = StubServer::start(vec![prose("Let me think about that some more.")]);
    let captured = Arc::new(std::sync::Mutex::new(None));
    let mut agent = build(server.config(), Arc::clone(&captured), Arc::new(AtomicUsize::new(0)));

    let err = agent.run_turn("Go.", &noop).await.unwrap_err().to_string();
    assert!(err.contains("prose instead of calling submit_analysis"), "{err}");
    // Exactly two attempts: the first, and one after the nudge.
    assert_eq!(server.requests().len(), 2, "it must not burn the whole budget");
}

#[tokio::test]
async fn the_request_carries_the_tools_and_the_system_brief() {
    let server = StubServer::start(vec![tool_call(
        "submit_analysis",
        json!({"intent": "x"}),
        Some("c1"),
    )]);
    let captured = Arc::new(std::sync::Mutex::new(None));
    let mut agent = build(server.config(), Arc::clone(&captured), Arc::new(AtomicUsize::new(0)));
    agent.run_turn("Go.", &noop).await.unwrap();

    let request = &server.requests()[0];
    assert_eq!(request["model"], "stub-model");
    assert_eq!(request["messages"][0]["role"], "system");
    assert_eq!(request["messages"][0]["content"], "You are a test agent.");
    assert_eq!(request["messages"][1]["content"], "Go.");

    let tools = request["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 3);
    assert_eq!(tools[0]["type"], "function");
    let names: Vec<&str> =
        tools.iter().map(|t| t["function"]["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"submit_analysis"));
    // Only "auto" is universally supported; "required" breaks strict servers.
    assert_eq!(request["tool_choice"], "auto");
}

#[tokio::test]
async fn a_model_without_tool_support_falls_back_to_prompted_tools() {
    // Ollama hard-400s any model whose template lacks a tool section, which is
    // most vision models — exactly the ones the describer wants for frames.
    let rejection = json!({
        "error": { "message": "registry.ollama.ai/library/qwen2.5vl:7b does not support tools" }
    });
    let server = StubServer::start(vec![
        rejection,
        prose(r#"{"name":"get_timeline","arguments":{}}"#),
        prose(r#"{"name":"submit_analysis","arguments":{"intent":"Compare the pricing tiers"}}"#),
    ]);
    let captured = Arc::new(std::sync::Mutex::new(None));
    let calls = Arc::new(AtomicUsize::new(0));
    let mut agent = build(server.config(), Arc::clone(&captured), Arc::clone(&calls));

    let result = agent.run_turn("Reconstruct the recording.", &noop).await.unwrap();
    assert_eq!(result["intent"], "Compare the pricing tiers");
    assert_eq!(calls.load(Ordering::SeqCst), 1, "a non-terminal tool also recovered");

    let requests = server.requests();
    // The first attempt offers tools; every attempt after the rejection must not.
    assert!(requests[0].get("tools").is_some());
    assert!(requests[1].get("tools").is_none(), "tools must not be re-sent after rejection");
    assert!(requests[2].get("tools").is_none());

    // The schemas the API would have carried are in the prompt instead.
    let manual = requests[1]["messages"].as_array().unwrap().last().unwrap()["content"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(manual.contains("cannot use the tools API"));
    assert!(manual.contains("get_timeline"));
    assert!(manual.contains("submit_analysis"));
}

#[tokio::test]
async fn other_bad_requests_are_not_mistaken_for_missing_tool_support() {
    // A genuinely malformed request must fail loudly, not silently downgrade to
    // prompted tools and hide the real problem.
    let server = StubServer::start(vec![json!({
        "error": { "message": "model 'nope' not found, try pulling it first" }
    })]);
    let captured = Arc::new(std::sync::Mutex::new(None));
    let mut agent = build(server.config(), Arc::clone(&captured), Arc::new(AtomicUsize::new(0)));

    let err = agent.run_turn("Go.", &noop).await.unwrap_err().to_string();
    assert!(err.contains("not found"), "{err}");
}

#[tokio::test]
async fn an_unreachable_endpoint_reports_the_address_it_tried() {
    let config = LlmConfig {
        // Port 1 is reserved and nothing will ever be listening.
        base_url: "http://127.0.0.1:1/v1".into(),
        request_timeout_secs: 3,
        ..Default::default()
    };
    let test = LlmClient::new(config).unwrap().test_connection().await;
    assert!(!test.reachable);
    assert!(test.message.contains("127.0.0.1:1"), "{}", test.message);
}
