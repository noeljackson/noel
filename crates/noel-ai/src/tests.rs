//! noel-ai's unit tests, and the helpers only they use.

use super::*;
use std::collections::BTreeSet;

/// A whole SSE body's events through parser `P`, without the final
/// `Stop`: what the batch path emits before it. `None` for no events, or
/// for a stream that did not end properly.
fn sse_events<P: SseParser>(body: &str) -> Option<Vec<StreamEvent>> {
    let mut events = parse_sse_body::<P>(body).ok().flatten()?;
    events.pop();
    (!events.is_empty()).then_some(events)
}

fn parse_google_sse_events(body: &str) -> Option<Vec<StreamEvent>> {
    sse_events::<GoogleSseParser>(body)
}

fn parse_anthropic_sse_events(body: &str) -> Option<Vec<StreamEvent>> {
    sse_events::<AnthropicSseParser>(body)
}

fn parse_openai_responses_sse_events(body: &str) -> Option<Vec<StreamEvent>> {
    sse_events::<OpenAiResponsesSseParser>(body)
}

fn parse_openai_chat_completions_sse_events(body: &str) -> Option<Vec<StreamEvent>> {
    sse_events::<OpenAiChatCompletionsSseParser>(body)
}

fn parse_google_sse_text(body: &str) -> Option<String> {
    let events = parse_google_sse_events(body)?;
    let text = events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::Text(text) => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

fn openai_messages(request: &ProviderRequest) -> Vec<Value> {
    let mut messages = Vec::new();
    if let Some(system_prompt) = &request.system_prompt {
        messages.push(json!({ "role": "system", "content": system_prompt }));
    }
    messages.extend(request.messages.iter().map(|message| {
        let content = if message.media.is_empty() {
            json!(message.content)
        } else {
            let mut parts = vec![json!({
                "type": "text",
                "text": message.content,
            })];
            parts.extend(message.media.iter().map(|media| {
                json!({
                    "type": "image_url",
                    "image_url": {
                        "url": media_data_url(media),
                    },
                })
            }));
            Value::Array(parts)
        };
        json!({
            "role": role_name(&message.role),
            "content": content,
        })
    }));
    messages
}

fn parse_openai_responses_text(response: &Value) -> Option<String> {
    if let Some(text) = response.get("output_text").and_then(Value::as_str)
        && !text.is_empty()
    {
        return Some(text.to_string());
    }
    let text = response
        .get("output")
        .and_then(Value::as_array)?
        .iter()
        .filter_map(|item| item.get("content").and_then(Value::as_array))
        .flatten()
        .filter_map(|content| content.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("");
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

fn parse_openai_responses_sse_text(body: &str) -> Option<String> {
    let mut text = String::new();
    for line in body.lines() {
        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };
        if data == "[DONE]" {
            continue;
        }
        let Ok(event) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        match event.get("type").and_then(Value::as_str) {
            Some("response.output_text.delta") | Some("output_text.delta") => {
                if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                    text.push_str(delta);
                }
            }
            Some("response.output_item.done") | Some("output_item.done") if text.is_empty() => {
                if let Some(item_text) =
                    event.get("item").and_then(parse_openai_responses_item_text)
                {
                    text.push_str(&item_text);
                }
            }
            _ => {}
        }
    }
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

fn parse_openai_chat_completions_sse_text(body: &str) -> Option<String> {
    let mut text = String::new();
    for line in body.lines() {
        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };
        if data == "[DONE]" {
            continue;
        }
        let Ok(event) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        let Some(delta) = event
            .pointer("/choices/0/delta/content")
            .and_then(Value::as_str)
        else {
            continue;
        };
        text.push_str(delta);
    }
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

fn anthropic_messages(messages: &[ChatMessage]) -> Vec<Value> {
    anthropic_messages_with_cache_control(messages, false)
}

#[test]
fn request_local_output_budget_overrides_provider_defaults_without_changing_regular_requests() {
    let request = ProviderRequest {
        max_output_tokens: Some(512),
        system_prompt: None,
        messages: Vec::new(),
        tools: Vec::new(),
    };
    let config = ProviderConfig {
        model: ModelRef {
            provider: "openai".into(),
            id: "gpt-5.4".into(),
        },
        api: ProviderApi::OpenAi,
        base_url: None,
        auth: ProviderAuth::None,
        thinking_level: None,
        thinking_budget_tokens: None,
        session_id: None,
        traits: ModelTraits {
            context_window: Some(272_000),
            max_tokens: Some(128_000),
            ..ModelTraits::default()
        },
    };
    assert_eq!(
        openai_chat_completions_body(&config, &request)["max_tokens"],
        512
    );
    assert_eq!(
        openai_responses_body(&config, &request)["max_output_tokens"],
        512
    );
    assert_eq!(anthropic_body(&config, &request)["max_tokens"], 512);
    assert_eq!(
        google_body(&config, &request)["generationConfig"]["maxOutputTokens"],
        512
    );
    assert_eq!(mistral_chat_body(&config, &request)["max_tokens"], 512);
    assert_eq!(
        bedrock_body(&config, &request)["inferenceConfig"]["maxTokens"],
        512
    );
    let regular = ProviderRequest {
        max_output_tokens: None,
        ..request
    };
    assert_eq!(provider_max_tokens(&config, &regular), Some(128_000));
}

#[test]
fn provider_max_tokens_leaves_room_for_the_input() {
    let config = ProviderConfig {
        model: ModelRef {
            provider: "cloudflare-ai-gateway".to_string(),
            id: "workers-ai/@cf/moonshotai/kimi-k2.6".to_string(),
        },
        api: ProviderApi::OpenAi,
        base_url: None,
        auth: ProviderAuth::None,
        thinking_level: None,
        thinking_budget_tokens: None,
        session_id: None,
        traits: ModelTraits {
            context_window: Some(262_144),
            max_tokens: Some(256_000),
            ..ModelTraits::default()
        },
    };
    let request = |content: &str| ProviderRequest {
        max_output_tokens: None,
        system_prompt: Some("pi rust cli".to_string()),
        messages: vec![ChatMessage {
            role: ChatRole::User,
            content: content.to_string(),
            media: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
        }],
        tools: Vec::new(),
    };
    // Small input: the model's cap.
    assert_eq!(
        provider_max_tokens(&config, &request("hello")),
        Some(256_000)
    );
    // No known window: the cap as it is. No known cap: none is sent.
    let mut unknown = config.clone();
    unknown.traits.context_window = None;
    assert_eq!(
        provider_max_tokens(&unknown, &request("hello")),
        Some(256_000)
    );
    unknown.traits.max_tokens = None;
    assert_eq!(provider_max_tokens(&unknown, &request("hello")), None);
    // 300k characters estimate as 85_715 tokens; with the 4 for the system
    // prompt and the 4096 safety margin, 172_329 remain for the answer.
    let long = "x".repeat(300_000);
    assert_eq!(provider_max_tokens(&config, &request(&long)), Some(172_329));
    // An input past the window still asks for one token, as upstream does.
    let huge = "x".repeat(1_000_000);
    assert_eq!(provider_max_tokens(&config, &request(&huge)), Some(1));
}

#[test]
fn streaming_parsers_preserve_truncation_instead_of_accepting_partial_checkpoints() {
    let mut chat = OpenAiChatCompletionsSseParser::default();
    chat.parse_data(
        r#"{"choices":[{"finish_reason":"length","delta":{}}]}"#,
        &mut |_| Ok(()),
    )
    .unwrap();
    assert_eq!(chat.stop_reason.as_deref(), Some("length"));
    let mut anthropic = AnthropicSseParser::default();
    anthropic
        .parse_data(
            r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens"}}"#,
            &mut |_| Ok(()),
        )
        .unwrap();
    assert_eq!(anthropic.stop_reason.as_deref(), Some("max_tokens"));
    // Google's truncation is the stream's one Stop, last, with Google's
    // reason (#135: it used to come first, followed by a second "STOP").
    let google = parse_sse_body::<GoogleSseParser>(
        "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"cut\"}]},\"finishReason\":\"MAX_TOKENS\"}]}\n",
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        google,
        [
            StreamEvent::Text("cut".into()),
            StreamEvent::Stop {
                reason: "MAX_TOKENS".into()
            }
        ]
    );
    assert!(is_output_limit_stop("MAX_TOKENS"));
    for status in ["incomplete", "failed", "cancelled"] {
        assert!(
            parse_openai_responses_events(&json!({"status":status,"output_text":"partial"}))
                .is_none()
        );
    }
    for kind in ["response.incomplete", "response.failed", "error"] {
        assert!(OpenAiResponsesSseParser::default()
            .parse_data(&json!({"type":kind}).to_string(), &mut |_| Ok(()))
            .is_err());
    }
}

#[test]
fn output_limit_stop_reasons_cover_every_provider_spelling() {
    for reason in [
        "max_tokens",
        "MAX_TOKENS",
        "length",
        "max_output_tokens",
        "model_context_window_exceeded",
    ] {
        assert!(is_output_limit_stop(reason), "{reason}");
    }
    for reason in [
        "stop",
        "STOP",
        "end_turn",
        "tool_use",
        "toolUse",
        "completed",
    ] {
        assert!(!is_output_limit_stop(reason), "{reason}");
    }
}

/// Accepts one connection per reply, reads the request, writes the reply
/// (possibly nothing), and then keeps the connection open without
/// sending anything else.
fn spawn_stalling_server(replies: Vec<&'static str>) -> String {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind stub");
    let url = format!("http://{}", listener.local_addr().expect("stub addr"));
    std::thread::spawn(move || {
        let mut stalled = Vec::new();
        for reply in replies {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut request = Vec::new();
            let mut buffer = [0u8; 4096];
            while let Ok(read) = stream.read(&mut buffer) {
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                let text = String::from_utf8_lossy(&request);
                if let Some((headers, body)) = text.split_once("\r\n\r\n") {
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|value| value.trim().parse::<usize>().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    if body.len() >= length {
                        break;
                    }
                }
            }
            let _ = stream.write_all(reply.as_bytes());
            stalled.push(stream);
        }
        std::thread::sleep(Duration::from_secs(30));
    });
    url
}

#[tokio::test]
async fn stalled_provider_requests_fail_after_the_idle_timeout() {
    let url = spawn_stalling_server(vec![
        // No response headers at all.
        "",
        // Headers and one event, then nothing.
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\ndata: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n",
    ]);
    let provider = create_provider_with_client(
        ProviderConfig {
            model: ModelRef {
                provider: "local".to_string(),
                id: "test".to_string(),
            },
            api: ProviderApi::OpenAi,
            base_url: Some(url),
            auth: ProviderAuth::ApiKey("test-key".to_string()),
            thinking_level: None,
            thinking_budget_tokens: None,
            session_id: None,
            traits: ModelTraits::default(),
        },
        provider_http_client_with_idle_timeout(Duration::from_millis(300)),
    );

    for expected_texts in [vec![], vec!["partial".to_string()]] {
        let mut texts = Vec::new();
        let result = tokio::time::timeout(
            Duration::from_secs(20),
            provider.complete_streaming(uncapped_request("system"), &mut |event| {
                if let StreamEvent::Text(text) = event {
                    texts.push(text);
                }
                Ok(())
            }),
        )
        .await
        .expect("the idle timeout must end a stalled request");
        let error = result.expect_err("stalled request");
        assert!(
            matches!(&error, ProviderError::Request(source) if source.is_timeout()),
            "{error:?}"
        );
        assert!(error.to_string().ends_with("(timed out)"), "{error}");
        assert_eq!(texts, expected_texts);
    }
    assert_eq!(PROVIDER_IDLE_TIMEOUT, Duration::from_secs(300));
}

#[tokio::test]
async fn faux_provider_echoes_last_user_message() {
    let provider = create_provider(ProviderConfig {
        model: ModelRef {
            provider: "faux".to_string(),
            id: "echo".to_string(),
        },
        api: ProviderApi::Faux,
        base_url: None,
        auth: ProviderAuth::None,
        thinking_level: None,
        thinking_budget_tokens: None,
        session_id: None,
        traits: ModelTraits::default(),
    });

    let events = provider
        .complete(ProviderRequest {
            max_output_tokens: None,
            system_prompt: None,
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: "hello".to_string(),
                media: Vec::new(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: Vec::new(),
            }],
            tools: Vec::new(),
        })
        .await
        .expect("faux provider should complete");

    assert_eq!(events[0], StreamEvent::Text("[faux/echo] ".to_string()));
    assert_eq!(events[1], StreamEvent::Text("hello".to_string()));
}

fn faux_script_dir(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = env::temp_dir().join(format!(
        "noel-faux-script-{name}-{}-{nanos}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn user_request(text: &str) -> ProviderRequest {
    ProviderRequest {
        max_output_tokens: None,
        system_prompt: Some("system".into()),
        messages: vec![ChatMessage {
            role: ChatRole::User,
            content: text.into(),
            media: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
        }],
        tools: vec![ToolDefinition {
            name: "read".into(),
            description: "Read a file".into(),
            parameters: json!({"type": "object"}),
        }],
    }
}

async fn run_faux_script(
    script: &FauxScript,
    request: &ProviderRequest,
) -> Result<Vec<StreamEvent>, ProviderError> {
    let mut events = Vec::new();
    script
        .respond(request, &mut |event| {
            events.push(event);
            Ok(())
        })
        .await
        .map(|()| events)
}

#[tokio::test]
async fn faux_script_replays_responses_and_logs_each_request() {
    let dir = faux_script_dir("replay");
    fs::write(
        dir.join("script.json"),
        r#"[
            [{"thinking": "plan"}, {"tool_call": {"name": "read", "arguments": {"path": "a.txt"}}}],
            [{"text": "done"}, {"usage": {"input": 3, "output": 2}}, {"stop": "end_turn"}]
        ]"#,
    )
    .unwrap();
    let script = FauxScript {
        path: dir.join("script.json"),
        log: Some(dir.join("requests.jsonl")),
    };

    let first = run_faux_script(&script, &user_request("read a.txt"))
        .await
        .unwrap();
    assert_eq!(
        first,
        [
            StreamEvent::Thinking("plan".into()),
            StreamEvent::ToolCall {
                id: "call_1_1".into(),
                name: "read".into(),
                arguments: r#"{"path":"a.txt"}"#.into(),
            },
            StreamEvent::Stop {
                reason: "tool_use".into()
            },
        ]
    );

    let mut second_request = user_request("read a.txt");
    second_request.messages.extend([
        ChatMessage {
            role: ChatRole::Assistant,
            content: String::new(),
            media: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: vec![ChatToolCall {
                id: "call_1_1".into(),
                name: "read".into(),
                arguments: r#"{"path":"a.txt"}"#.into(),
            }],
        },
        ChatMessage {
            role: ChatRole::Tool,
            content: "alpha".into(),
            media: Vec::new(),
            tool_call_id: Some("call_1_1".into()),
            tool_name: Some("read".into()),
            tool_calls: Vec::new(),
        },
    ]);
    let second = run_faux_script(&script, &second_request).await.unwrap();
    assert_eq!(
        second,
        [
            StreamEvent::Text("done".into()),
            StreamEvent::Usage {
                input_tokens: 3,
                output_tokens: 2
            },
            StreamEvent::Stop {
                reason: "end_turn".into()
            },
        ]
    );

    let log = fs::read_to_string(dir.join("requests.jsonl")).unwrap();
    let entries = log
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(entries.len(), 2);
    assert_eq!(
        entries[1],
        json!({
            "system_prompt": "system",
            "messages": [
                {"role": "user", "content": "read a.txt"},
                {"role": "assistant", "content": "", "tool_calls": [
                    {"id": "call_1_1", "name": "read", "arguments": {"path": "a.txt"}}
                ]},
                {"role": "tool", "content": "alpha", "tool_call_id": "call_1_1", "tool_name": "read"}
            ],
            "tools": ["read"]
        })
    );

    let exhausted = run_faux_script(&script, &user_request("more"))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        exhausted.contains("is exhausted: request 3 has no response (the script has 2)"),
        "{exhausted}"
    );
    fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn faux_script_log_position_survives_a_new_provider() {
    let dir = faux_script_dir("resume");
    fs::write(
        dir.join("script.json"),
        r#"[[{"text": "one"}], [{"text": "two"}], [{"text": "three"}]]"#,
    )
    .unwrap();
    let script = FauxScript {
        path: dir.join("script.json"),
        log: Some(dir.join("requests.jsonl")),
    };
    run_faux_script(&script, &user_request("a")).await.unwrap();
    run_faux_script(&script, &user_request("b")).await.unwrap();

    // A restarted process holds no state beyond the log.
    let restarted = script.clone();
    let events = run_faux_script(&restarted, &user_request("c"))
        .await
        .unwrap();
    assert_eq!(events[0], StreamEvent::Text("three".into()));
    assert_eq!(
        fs::read_to_string(dir.join("requests.jsonl"))
            .unwrap()
            .lines()
            .count(),
        3
    );
    fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn faux_script_without_log_counts_requests_per_script() {
    let dir = faux_script_dir("counter");
    for name in ["a.json", "b.json"] {
        fs::write(
            dir.join(name),
            format!(r#"[[{{"text": "{name} 1"}}], [{{"text": "{name} 2"}}]]"#),
        )
        .unwrap();
    }
    let a = FauxScript {
        path: dir.join("a.json"),
        log: None,
    };
    let b = FauxScript {
        path: dir.join("b.json"),
        log: None,
    };
    let request = user_request("hi");
    assert_eq!(
        run_faux_script(&a, &request).await.unwrap()[0],
        StreamEvent::Text("a.json 1".into())
    );
    assert_eq!(
        run_faux_script(&b, &request).await.unwrap()[0],
        StreamEvent::Text("b.json 1".into())
    );
    assert_eq!(
        run_faux_script(&a, &request).await.unwrap()[0],
        StreamEvent::Text("a.json 2".into())
    );
    assert!(run_faux_script(&a, &request).await.is_err());
    assert!(!dir.join("requests.jsonl").exists());
    fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn faux_script_error_fails_before_output_and_the_retry_gets_the_next_response() {
    let dir = faux_script_dir("error");
    fs::write(
        dir.join("script.json"),
        r#"[[{"error": "overloaded"}], [{"text": "recovered"}]]"#,
    )
    .unwrap();
    let script = FauxScript {
        path: dir.join("script.json"),
        log: Some(dir.join("requests.jsonl")),
    };
    let mut events = Vec::new();
    let error = script
        .respond(&user_request("hi"), &mut |event| {
            events.push(event);
            Ok(())
        })
        .await
        .unwrap_err();
    assert!(events.is_empty());
    assert!(error.to_string().contains("overloaded"), "{error}");
    assert_eq!(
        run_faux_script(&script, &user_request("hi")).await.unwrap()[0],
        StreamEvent::Text("recovered".into())
    );
    fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn faux_script_error_can_name_its_status_and_follow_output() {
    let dir = faux_script_dir("status-error");
    fs::write(
        dir.join("script.json"),
        r#"[
            [{"error": {"status": 401, "body": "bad key"}}],
            [{"text": "Hel"}, {"error": "terminated"}],
            [{"error": {"status": 99, "body": ""}}]
        ]"#,
    )
    .unwrap();
    let script = FauxScript {
        path: dir.join("script.json"),
        log: None,
    };
    let request = user_request("hi");
    let error = run_faux_script(&script, &request).await.unwrap_err();
    assert_eq!(
        error.to_string(),
        "request failed with status 401 Unauthorized: bad key"
    );
    assert!(!error.is_retryable());
    // An error after other steps breaks the reply off where it is.
    let mut events = Vec::new();
    let error = script
        .respond(&request, &mut |event| {
            events.push(event);
            Ok(())
        })
        .await
        .unwrap_err();
    assert_eq!(events, [StreamEvent::Text("Hel".into())]);
    assert_eq!(
        error.to_string(),
        "request failed with status 503 Service Unavailable: terminated"
    );
    assert!(error.is_retryable());
    let error = run_faux_script(&script, &request).await.unwrap_err();
    assert!(
        error.to_string().contains("99 is not an HTTP status"),
        "{error}"
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn error_classification_matches_ts_patterns_and_verdicts() {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/ts-parity/error-classification.json"
    ))
    .unwrap();
    let patterns = |name: &str| -> Vec<String> {
        let patterns = fixture["patterns"][name].as_array().unwrap();
        let sources = patterns.iter().map(|pattern| pattern.as_str().unwrap());
        sources.map(str::to_owned).collect()
    };
    assert_eq!(RETRYABLE_ERROR_PATTERNS, patterns("retryable"));
    assert_eq!(NON_RETRYABLE_ERROR_PATTERNS, patterns("nonRetryable"));
    assert_eq!(OVERFLOW_PATTERNS, patterns("overflow"));
    assert_eq!(NON_OVERFLOW_PATTERNS, patterns("nonOverflow"));

    let cases = fixture["cases"].as_array().unwrap();
    assert!(cases.len() > 100, "{} cases", cases.len());
    for case in cases {
        let message = case["message"].as_str().unwrap();
        assert_eq!(
            is_retryable_error_text(message),
            case["retryable"].as_bool().unwrap(),
            "retryable: {message:?}"
        );
        assert_eq!(
            is_context_overflow_text(message),
            case["overflow"].as_bool().unwrap(),
            "overflow: {message:?}"
        );
    }
}

#[test]
fn provider_errors_are_retryable_by_kind_and_text() {
    let status = |status: u16, body: &str| ProviderError::Status {
        status: reqwest::StatusCode::from_u16(status).unwrap(),
        body: body.to_string(),
        retry_after: None,
    };
    let invalid = |text: &str| ProviderError::InvalidResponse(text.to_string());
    for (error, retryable) in [
        (status(429, "slow down"), true),
        (status(500, ""), true),
        (status(502, ""), true),
        (status(503, "overloaded"), true),
        (status(504, ""), true),
        (
            status(529, r#"{"error":{"type":"overloaded_error"}}"#),
            true,
        ),
        (status(408, ""), true),
        (invalid("stream ended before message_stop"), true),
        (
            status(400, r#"{"error":{"message":"bad parameter"}}"#),
            false,
        ),
        (status(401, r#"{"error":{"message":"bad key"}}"#), false),
        (status(403, ""), false),
        (
            status(404, r#"{"error":{"message":"no such model"}}"#),
            false,
        ),
        (status(422, ""), false),
        // A quota or billing limit, though the status says rate limit.
        (
            status(429, r#"{"error":{"code":"insufficient_quota"}}"#),
            false,
        ),
        (invalid("{}"), false),
        (ProviderError::Config("no credentials".into()), false),
        // noel's own errors are never the provider's answer, whatever
        // their text happens to contain.
        (
            ProviderError::Config("faux script /tmp/run-503/script.json is exhausted".into()),
            false,
        ),
        (
            ProviderError::MissingApiKey {
                provider: "anthropic".into(),
            },
            false,
        ),
        // Compaction answers a context overflow, whatever else it says.
        (
            status(400, r#"{"error":{"code":"context_length_exceeded"}}"#),
            false,
        ),
        (
            status(413, r#"{"error":{"type":"request_too_large"}}"#),
            false,
        ),
        (
            status(503, "prompt is too long: 9 tokens > 8 maximum"),
            false,
        ),
    ] {
        assert_eq!(error.is_retryable(), retryable, "{error}");
    }
}

#[test]
fn retry_after_reads_milliseconds_then_seconds() {
    let wait = |headers: &[(&'static str, &str)]| {
        let mut map = HeaderMap::new();
        for (name, value) in headers {
            map.insert(*name, HeaderValue::from_str(value).unwrap());
        }
        retry_after(&map)
    };
    assert_eq!(wait(&[]), None);
    assert_eq!(
        wait(&[("retry-after", "30")]),
        Some(Duration::from_secs(30))
    );
    assert_eq!(
        wait(&[("retry-after", " 1.5 ")]),
        Some(Duration::from_millis(1500))
    );
    // Milliseconds win, as upstream reads them first.
    assert_eq!(
        wait(&[("retry-after-ms", "250"), ("retry-after", "30")]),
        Some(Duration::from_millis(250))
    );
    assert_eq!(wait(&[("retry-after", "-5")]), Some(Duration::ZERO));
    // A date, and values no wait can hold, are not read.
    assert_eq!(
        wait(&[("retry-after", "Wed, 21 Oct 2015 07:28:00 GMT")]),
        None
    );
    assert_eq!(wait(&[("retry-after", "inf")]), None);
    assert_eq!(
        wait(&[("retry-after-ms", "soon"), ("retry-after", "30")]),
        None
    );
}

#[test]
fn context_overflow_is_read_from_the_error_text_and_cerebras_statuses() {
    let status = |status: u16, body: &str| ProviderError::Status {
        status: reqwest::StatusCode::from_u16(status).unwrap(),
        body: body.to_string(),
        retry_after: None,
    };
    let overflow = status(400, r#"{"error":{"code":"context_length_exceeded"}}"#);
    assert!(overflow.is_context_overflow("openai"));
    assert!(
        status(413, r#"{"error":{"type":"request_too_large"}}"#).is_context_overflow("anthropic")
    );
    // Throttling that mentions tokens is not an overflow.
    let throttled = status(429, r#"{"message":"Too many tokens, please wait."}"#);
    assert!(!throttled.is_context_overflow("amazon-bedrock"));
    assert!(throttled.is_retryable());
    assert!(!status(400, "bad parameter").is_context_overflow("openai"));
    // Cerebras answers an overflow without a body.
    for code in [400, 413] {
        assert!(status(code, "").is_context_overflow("cerebras"));
        assert!(!status(code, "").is_context_overflow("openai"));
    }
    assert!(!status(400, "bad parameter").is_context_overflow("cerebras"));
    assert!(!status(500, "").is_context_overflow("cerebras"));
}

#[tokio::test]
async fn a_connection_that_cannot_be_made_is_retryable() {
    // Nothing listens on the port, so the request fails in transport.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let error = reqwest::Client::new().get(url).send().await.unwrap_err();
    // Without the URL: its port is random, and a port such as 35033
    // contains one of upstream's patterns ("503").
    let error = ProviderError::Request(error.without_url());
    // The text carries the cause (upstream 4c992154), and the kind alone
    // makes it retryable.
    assert!(error.to_string().contains("Connection refused"), "{error}");
    assert!(error.is_retryable());
    assert!(!error.is_context_overflow("openai"));
}

/// The limit a cut-off reply is compared with is the catalog's
/// `maxTokens`, for every built-in model.
#[test]
fn output_limits_are_the_catalogs_for_every_built_in_model() {
    let models = catalog::builtin_models();
    assert!(models.len() > 1000, "{} models", models.len());
    for model in models {
        assert!(model.max_tokens > 0, "{}/{}", model.provider, model.id);
        assert_eq!(
            model_output_limit(&builtin_config(model)),
            Some(model.max_tokens),
            "{}/{}",
            model.provider,
            model.id
        );
    }

    // A model nothing is known about, as a `models.json` entry without
    // `maxTokens` is: the cap its Anthropic requests carry, else none.
    let unknown = |api: ProviderApi| ProviderConfig {
        model: ModelRef {
            provider: "local".to_string(),
            id: "llama".to_string(),
        },
        api,
        base_url: None,
        auth: ProviderAuth::None,
        thinking_level: None,
        thinking_budget_tokens: None,
        session_id: None,
        traits: ModelTraits::default(),
    };
    assert_eq!(
        model_output_limit(&unknown(ProviderApi::Anthropic)),
        Some(16_384)
    );
    assert_eq!(model_output_limit(&unknown(ProviderApi::OpenAi)), None);
    assert_eq!(
        model_output_limit(&ProviderConfig {
            traits: ModelTraits {
                max_tokens: Some(4_096),
                ..ModelTraits::default()
            },
            ..unknown(ProviderApi::OpenAi)
        }),
        Some(4_096)
    );
}

#[tokio::test]
async fn faux_script_delays_and_holds_a_response_until_a_file_exists() {
    let dir = faux_script_dir("wait");
    fs::write(
        dir.join("script.json"),
        r#"[[{"delay_ms": 50}, {"text": "before"}, {"wait_for_file": "go"}, {"text": "after"}]]"#,
    )
    .unwrap();
    let script = FauxScript {
        path: dir.join("script.json"),
        log: None,
    };
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let request = user_request("hi");
    let started = std::time::Instant::now();
    let run = async {
        script
            .respond(&request, &mut |event| {
                sender.send(event).unwrap();
                Ok(())
            })
            .await
    };
    let release = async {
        assert_eq!(
            receiver.recv().await,
            Some(StreamEvent::Text("before".into()))
        );
        assert!(started.elapsed() >= Duration::from_millis(50));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(receiver.try_recv().is_err(), "response ran past the wait");
        fs::write(dir.join("go"), "").unwrap();
    };
    let (result, ()) = tokio::join!(run, release);
    result.unwrap();
    assert_eq!(
        receiver.recv().await,
        Some(StreamEvent::Text("after".into()))
    );
    fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn faux_script_reports_unreadable_and_invalid_scripts() {
    let dir = faux_script_dir("invalid");
    let missing = FauxScript {
        path: dir.join("missing.json"),
        log: None,
    };
    let error = run_faux_script(&missing, &user_request("hi"))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("missing.json"), "{error}");

    fs::write(dir.join("script.json"), r#"[[{"txt": "typo"}]]"#).unwrap();
    let invalid = FauxScript {
        path: dir.join("script.json"),
        log: Some(dir.join("requests.jsonl")),
    };
    let error = run_faux_script(&invalid, &user_request("hi"))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("invalid faux script"), "{error}");
    assert!(error.contains("txt"), "{error}");
    assert!(!dir.join("requests.jsonl").exists());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn usage_is_decoded_for_all_response_apis_and_streams() {
    let expected = StreamEvent::Usage {
        input_tokens: 12,
        output_tokens: 5,
    };
    let responses = [
        parse_openai_chat_completions_events(
            &json!({"choices":[{"message":{"content":"ok"}}],"usage":{"prompt_tokens":12,"completion_tokens":5}}),
        ),
        parse_openai_responses_events(
            &json!({"output_text":"ok","usage":{"input_tokens":12,"output_tokens":5}}),
        ),
        parse_anthropic_response_events(
            &json!({"content":[],"usage":{"input_tokens":2,"cache_read_input_tokens":8,"cache_creation_input_tokens":2,"output_tokens":5}}),
        ),
        parse_google_response_events(
            &json!({"usageMetadata":{"promptTokenCount":12,"candidatesTokenCount":3,"thoughtsTokenCount":2}}),
        ),
        parse_bedrock_response_events(
            &json!({"output":{"message":{"content":[]}},"usage":{"inputTokens":12,"outputTokens":5}}),
        ),
    ];
    for events in responses {
        assert_eq!(events.unwrap().last(), Some(&expected));
    }
    assert!(response_usage(&json!({"usage": null})).is_none());
    let chat = r#"{"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":12,"completion_tokens":5}}"#;
    let responses = r#"{"type":"response.completed","response":{"usage":{"input_tokens":12,"output_tokens":5}}}"#;
    let google = r#"{"candidates":[{"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":12,"candidatesTokenCount":5}}"#;
    for (data, buffered) in [
        (
            chat,
            parse_openai_chat_completions_sse_events(&format!("data: {chat}\n")),
        ),
        (
            responses,
            parse_openai_responses_sse_events(&format!("data: {responses}\n")),
        ),
        (
            google,
            parse_google_sse_events(&format!("data: {google}\n")),
        ),
    ] {
        assert_eq!(buffered.unwrap(), std::slice::from_ref(&expected));
        let mut events = Vec::new();
        let mut callback = |event| {
            events.push(event);
            Ok(())
        };
        if data == chat {
            OpenAiChatCompletionsSseParser::default()
                .parse_data(data, &mut callback)
                .unwrap();
        } else if data == responses {
            OpenAiResponsesSseParser::default()
                .parse_data(data, &mut callback)
                .unwrap();
        } else {
            GoogleSseParser::default()
                .parse_data(data, &mut callback)
                .unwrap();
        }
        assert_eq!(events, std::slice::from_ref(&expected));
    }
    let start = r#"{"type":"message_start","message":{"usage":{"input_tokens":2,"cache_read_input_tokens":10,"output_tokens":1}}}"#;
    let delta = r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}}"#;
    let body = format!("data: {start}\ndata: {delta}\ndata: {{\"type\":\"message_stop\"}}\n");
    let buffered = parse_anthropic_sse_events(&body).unwrap();
    assert_eq!(buffered.last(), Some(&expected));
    let mut parser = AnthropicSseParser::default();
    let mut events = Vec::new();
    for data in [start, delta] {
        parser
            .parse_data(data, &mut |event| {
                events.push(event);
                Ok(())
            })
            .unwrap();
    }
    assert_eq!(events, buffered);
}

#[test]
fn thinking_events_are_separate_from_answer_text() {
    let anthropic = concat!(
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"first\"}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\" second\"}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"private-signature\"}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"answer\"}}\n\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    let expected = vec![
        StreamEvent::Thinking("first".into()),
        StreamEvent::Thinking(" second".into()),
        StreamEvent::Text("answer".into()),
    ];
    assert_eq!(
        parse_anthropic_sse_events(anthropic),
        Some(expected.clone())
    );
    let mut streamed = Vec::new();
    let mut parser = AnthropicSseParser::default();
    for line in anthropic
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
    {
        parser
            .parse_data(line, &mut |event| {
                streamed.push(event);
                Ok(())
            })
            .unwrap();
    }
    assert_eq!(streamed, expected);

    let openai = concat!(
        "data: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"first\"}\n\n",
        "data: {\"type\":\"response.reasoning_text.delta\",\"delta\":\" second\"}\n\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"answer\"}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n",
    );
    assert_eq!(
        parse_openai_responses_sse_events(openai),
        Some(expected.clone())
    );
    let mut streamed = Vec::new();
    let mut parser = OpenAiResponsesSseParser::default();
    for line in openai
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
    {
        parser
            .parse_data(line, &mut |event| {
                streamed.push(event);
                Ok(())
            })
            .unwrap();
    }
    assert_eq!(streamed, expected);
    assert_eq!(
        google_part_events(&[
            json!({"thought": true, "text": "first"}),
            json!({"text": "answer"})
        ]),
        Some(vec![
            StreamEvent::Thinking("first".into()),
            StreamEvent::Text("answer".into())
        ])
    );
}

#[test]
fn chatgpt_oauth_headers_include_bearer_and_account_id() {
    let headers = chatgpt_headers(&ProviderAuth::ChatGptOAuth {
        access_token: "access-token".to_string(),
        account_id: Some("account-id".to_string()),
    })
    .expect("headers");

    assert_eq!(
        headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok()),
        Some("Bearer access-token")
    );
    assert_eq!(
        headers
            .get("ChatGPT-Account-ID")
            .and_then(|value| value.to_str().ok()),
        Some("account-id")
    );
}

#[test]
fn parses_openai_responses_text_shapes() {
    let direct = json!({ "output_text": "hello" });
    assert_eq!(
        parse_openai_responses_text(&direct),
        Some("hello".to_string())
    );

    let nested = json!({
        "output": [{
            "content": [
                { "type": "output_text", "text": "hel" },
                { "type": "output_text", "text": "lo" }
            ]
        }]
    });
    assert_eq!(
        parse_openai_responses_text(&nested),
        Some("hello".to_string())
    );
}

#[test]
fn parses_openai_responses_sse_delta_text() {
    let body = concat!(
        "event: response.output_text.delta\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hel\"}\n\n",
        "event: response.output_text.delta\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"lo\"}\n\n",
        "data: [DONE]\n"
    );

    assert_eq!(
        parse_openai_responses_sse_text(body),
        Some("hello".to_string())
    );
}

#[test]
fn parses_openai_responses_function_call_events() {
    let body = concat!(
        r#"data: {"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_1","name":"read","arguments":"{\"path\":\"a.txt\"}"}}"#,
        "\n\n",
        r#"data: {"type":"response.completed","response":{"status":"completed"}}"#,
        "\n\n",
    );
    let events = parse_openai_responses_sse_events(body).expect("events");

    assert_eq!(
        events,
        vec![StreamEvent::ToolCall {
            id: "call_1".to_string(),
            name: "read".to_string(),
            arguments: r#"{"path":"a.txt"}"#.to_string(),
        }]
    );
}

#[test]
fn parses_openai_chat_completions_sse_delta_text() {
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"hel\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\n",
        "data: [DONE]\n"
    );

    assert_eq!(
        parse_openai_chat_completions_sse_text(body),
        Some("hello".to_string())
    );
}

#[test]
fn parses_openai_chat_completions_tool_call_events() {
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"a.txt\\\"}\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]\n"
    );

    assert_eq!(
        parse_openai_chat_completions_sse_events(body),
        Some(vec![StreamEvent::ToolCall {
            id: "call_1".to_string(),
            name: "read".to_string(),
            arguments: r#"{"path":"a.txt"}"#.to_string(),
        }])
    );
}

#[test]
fn openai_chat_stream_parser_emits_text_before_finish() {
    let mut parser = OpenAiChatCompletionsSseParser::default();
    let mut events = Vec::new();
    parser
        .parse_data(
            r#"{"choices":[{"delta":{"content":"hel"}}]}"#,
            &mut |event| {
                events.push(event);
                Ok(())
            },
        )
        .expect("parse text");
    assert_eq!(events, [StreamEvent::Text("hel".to_string())]);

    parser
        .parse_data(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read","arguments":"{\"path\":"}}]}}]}"#,
            &mut |event| {
                events.push(event);
                Ok(())
            },
        )
        .expect("parse tool start");
    parser
        .parse_data(
            // #160: some providers send the id and name with every chunk.
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read","arguments":"\"a.txt\"}"}}]}}]}"#,
            &mut |event| {
                events.push(event);
                Ok(())
            },
        )
        .expect("parse tool end");
    assert_eq!(events, [StreamEvent::Text("hel".to_string())]);
    parser
        .parse_data(
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            &mut |_| Ok(()),
        )
        .expect("parse finish");

    let reason = parser
        .finish(&mut |event| {
            events.push(event);
            Ok(())
        })
        .expect("finish");
    assert_eq!(reason, "tool_calls");
    assert_eq!(
        events,
        [
            StreamEvent::Text("hel".to_string()),
            StreamEvent::ToolCall {
                id: "call_1".to_string(),
                name: "read".to_string(),
                arguments: r#"{"path":"a.txt"}"#.to_string(),
            }
        ]
    );
}

#[test]
fn openai_responses_stream_parser_does_not_duplicate_done_text_after_delta() {
    let mut parser = OpenAiResponsesSseParser::default();
    let mut events = Vec::new();
    parser
        .parse_data(
            r#"{"type":"response.output_text.delta","delta":"hello"}"#,
            &mut |event| {
                events.push(event);
                Ok(())
            },
        )
        .expect("parse delta");
    parser
        .parse_data(
            r#"{"type":"response.output_item.done","item":{"type":"message","content":[{"type":"output_text","text":"hello"}]}}"#,
            &mut |event| {
                events.push(event);
                Ok(())
            },
        )
        .expect("parse done");

    assert_eq!(events, [StreamEvent::Text("hello".to_string())]);
}

/// #160: whether an item's text streamed is per output item: a later item
/// that streamed no deltas still gives its text when it is done.
#[test]
fn openai_responses_done_text_of_an_item_without_deltas_is_kept() {
    let mut parser = OpenAiResponsesSseParser::default();
    let mut events = Vec::new();
    for data in [
        r#"{"type":"response.output_text.delta","output_index":0,"delta":"first"}"#,
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"message","content":[{"type":"output_text","text":"first"}]}}"#,
        r#"{"type":"response.output_item.done","output_index":1,"item":{"type":"message","content":[{"type":"output_text","text":" second"}]}}"#,
    ] {
        parser
            .parse_data(data, &mut |event| {
                events.push(event);
                Ok(())
            })
            .expect("parse");
    }
    assert_eq!(
        events,
        [
            StreamEvent::Text("first".to_string()),
            StreamEvent::Text(" second".to_string())
        ]
    );
}

#[test]
fn parses_google_sse_text_deltas() {
    let body = concat!(
        "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hel\"}]}}]}\n\n",
        "data: {\"candidates\":[{\"content\":{\"parts\":[{\"thought\":true,\"text\":\"skip\"},{\"text\":\"lo\"}]},\"finishReason\":\"STOP\"}]}\n\n",
    );

    assert_eq!(parse_google_sse_text(body), Some("hello".to_string()));
}

#[test]
fn provider_message_converters_include_image_parts() {
    let request = ProviderRequest {
        max_output_tokens: None,
        system_prompt: None,
        messages: vec![ChatMessage {
            role: ChatRole::User,
            content: "describe".to_string(),
            media: vec![MediaInput {
                mime_type: "image/png".to_string(),
                data_base64: "aW1hZ2U=".to_string(),
                path: Some("fixture.png".to_string()),
                width: Some(1),
                height: Some(1),
            }],
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
        }],
        tools: Vec::new(),
    };

    let openai = openai_messages(&request);
    assert_eq!(
        openai[0]["content"][1]["image_url"]["url"],
        "data:image/png;base64,aW1hZ2U="
    );
    let anthropic = anthropic_messages(&request.messages);
    assert_eq!(
        anthropic[0]["content"][1]["source"]["media_type"],
        "image/png"
    );
    let google_config = ProviderConfig {
        model: ModelRef {
            provider: "google".to_string(),
            id: "gemini-2.5-pro".to_string(),
        },
        api: ProviderApi::Google,
        base_url: None,
        auth: ProviderAuth::ApiKey("key".to_string()),
        thinking_level: None,
        thinking_budget_tokens: None,
        session_id: None,
        traits: ModelTraits::default(),
    };
    let google = google_messages(&google_config, &request.messages);
    assert_eq!(google[0]["parts"][1]["inlineData"]["mimeType"], "image/png");
}

#[test]
fn openai_responses_input_preserves_tool_calls_and_results() {
    let request = ProviderRequest {
        max_output_tokens: None,
        system_prompt: None,
        messages: vec![
            ChatMessage {
                role: ChatRole::User,
                content: "use a tool".to_string(),
                media: Vec::new(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: Vec::new(),
            },
            ChatMessage {
                role: ChatRole::Assistant,
                content: String::new(),
                media: Vec::new(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: vec![ChatToolCall {
                    id: "call_1".to_string(),
                    name: "read".to_string(),
                    arguments: r#"{"path":"a.txt"}"#.to_string(),
                }],
            },
            ChatMessage {
                role: ChatRole::Tool,
                content: "file contents".to_string(),
                media: Vec::new(),
                tool_call_id: Some("call_1".to_string()),
                tool_name: Some("read".to_string()),
                tool_calls: Vec::new(),
            },
        ],
        tools: Vec::new(),
    };

    let input = openai_responses_input(&request);
    assert_eq!(input[1]["type"], "function_call");
    assert_eq!(input[1]["call_id"], "call_1");
    assert_eq!(input[1]["name"], "read");
    assert_eq!(input[2]["type"], "function_call_output");
    assert_eq!(input[2]["call_id"], "call_1");
    assert_eq!(input[2]["output"], "file contents");
}

#[test]
fn openai_responses_input_normalizes_pipe_separated_tool_call_ids() {
    let fixture = serde_json::from_str::<serde_json::Value>(include_str!(
        "../../../tests/fixtures/ts-parity/openai-responses-tool-id.json"
    ))
    .expect("parse TS parity fixture");
    let raw_id = fixture["conversion"]["rawToolCallId"]
        .as_str()
        .expect("raw tool call id");
    let request = ProviderRequest {
        max_output_tokens: None,
        system_prompt: None,
        messages: vec![
            ChatMessage {
                role: ChatRole::Assistant,
                content: String::new(),
                media: Vec::new(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: vec![ChatToolCall {
                    id: raw_id.to_string(),
                    name: "edit".to_string(),
                    arguments: r#"{"path":"src/styles/app.css"}"#.to_string(),
                }],
            },
            ChatMessage {
                role: ChatRole::Tool,
                content: "ok".to_string(),
                media: Vec::new(),
                tool_call_id: Some(raw_id.to_string()),
                tool_name: Some("edit".to_string()),
                tool_calls: Vec::new(),
            },
        ],
        tools: Vec::new(),
    };

    let input = openai_responses_input(&request);

    assert_eq!(input[0], fixture["conversion"]["functionCall"]);
    assert!(input[0]["id"].as_str().expect("id").len() <= 64);
    assert_eq!(input[1], fixture["conversion"]["toolOutput"]);
}

#[test]
fn openai_responses_tool_result_keeps_images_inside_function_output() {
    let request = ProviderRequest {
        max_output_tokens: None,
        system_prompt: None,
        messages: vec![ChatMessage {
            role: ChatRole::Tool,
            content: "A red circle".to_string(),
            media: vec![MediaInput {
                mime_type: "image/png".to_string(),
                data_base64: "cmVkLWNpcmNsZQ==".to_string(),
                path: Some("circle.png".to_string()),
                width: Some(100),
                height: Some(100),
            }],
            tool_call_id: Some("call_1".to_string()),
            tool_name: Some("get_image".to_string()),
            tool_calls: Vec::new(),
        }],
        tools: Vec::new(),
    };

    let input = openai_responses_input(&request);

    assert_eq!(input[0]["type"], "function_call_output");
    assert_eq!(input[0]["output"][0]["type"], "input_text");
    assert_eq!(input[0]["output"][0]["text"], "A red circle");
    assert_eq!(input[0]["output"][1]["type"], "input_image");
    assert_eq!(input[0]["output"][1]["detail"], "auto");
    assert_eq!(
        input[0]["output"][1]["image_url"],
        "data:image/png;base64,cmVkLWNpcmNsZQ=="
    );
}

#[test]
fn openai_chat_body_preserves_tools_calls_and_results() {
    let request = ProviderRequest {
        max_output_tokens: None,
        system_prompt: None,
        messages: vec![
            ChatMessage {
                role: ChatRole::Assistant,
                content: String::new(),
                media: Vec::new(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: vec![ChatToolCall {
                    id: "call_1".to_string(),
                    name: "read".to_string(),
                    arguments: r#"{"path":"a.txt"}"#.to_string(),
                }],
            },
            ChatMessage {
                role: ChatRole::Tool,
                content: "file contents".to_string(),
                media: Vec::new(),
                tool_call_id: Some("call_1".to_string()),
                tool_name: Some("read".to_string()),
                tool_calls: Vec::new(),
            },
        ],
        tools: vec![ToolDefinition {
            name: "read".to_string(),
            description: "Read a file".to_string(),
            parameters: json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"],
            }),
        }],
    };
    let config = ProviderConfig {
        model: ModelRef {
            provider: "openrouter".to_string(),
            id: "model".to_string(),
        },
        api: ProviderApi::OpenAi,
        base_url: None,
        auth: ProviderAuth::ApiKey("token".to_string()),
        thinking_level: None,
        thinking_budget_tokens: None,
        session_id: None,
        traits: ModelTraits::default(),
    };

    let mut openai = config.clone();
    openai.model.provider = "openai".into();
    assert_eq!(
        openai_chat_completions_body(&openai, &request)["stream_options"]["include_usage"],
        true
    );
    let body = openai_chat_completions_body(&config, &request);
    assert_eq!(body["tools"][0]["function"]["name"], "read");
    assert_eq!(body["messages"][0]["tool_calls"][0]["id"], "call_1");
    assert_eq!(body["messages"][1]["role"], "tool");
    assert_eq!(body["messages"][1]["tool_call_id"], "call_1");
}

#[test]
fn openai_chat_body_normalizes_responses_tool_call_ids() {
    let raw_id = "call_1234567890abcdefghijklmnopqrstuvwxyzEXTRA|fc_foreign_item";
    let request = ProviderRequest {
        max_output_tokens: None,
        system_prompt: None,
        messages: vec![
            ChatMessage {
                role: ChatRole::Assistant,
                content: String::new(),
                media: Vec::new(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: vec![ChatToolCall {
                    id: raw_id.to_string(),
                    name: "read".to_string(),
                    arguments: r#"{"path":"a.txt"}"#.to_string(),
                }],
            },
            ChatMessage {
                role: ChatRole::Tool,
                content: "file contents".to_string(),
                media: Vec::new(),
                tool_call_id: Some(raw_id.to_string()),
                tool_name: Some("read".to_string()),
                tool_calls: Vec::new(),
            },
        ],
        tools: Vec::new(),
    };
    let config = ProviderConfig {
        model: ModelRef {
            provider: "openai".to_string(),
            id: "gpt-4o-mini".to_string(),
        },
        api: ProviderApi::OpenAi,
        base_url: None,
        auth: ProviderAuth::ApiKey("sk".to_string()),
        thinking_level: None,
        thinking_budget_tokens: None,
        session_id: None,
        traits: ModelTraits::default(),
    };

    let body = openai_chat_completions_body(&config, &request);

    assert_eq!(
        body["messages"][0]["tool_calls"][0]["id"],
        "call_1234567890abcdefghijklmnopqrstuvwxy"
    );
    assert_eq!(
        body["messages"][1]["tool_call_id"],
        "call_1234567890abcdefghijklmnopqrstuvwxy"
    );
}

#[test]
fn anthropic_body_preserves_tools_calls_and_results() {
    let request = ProviderRequest {
        max_output_tokens: None,
        system_prompt: None,
        messages: vec![
            ChatMessage {
                role: ChatRole::Assistant,
                content: String::new(),
                media: Vec::new(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: vec![ChatToolCall {
                    id: "toolu_1".to_string(),
                    name: "read".to_string(),
                    arguments: r#"{"path":"a.txt"}"#.to_string(),
                }],
            },
            ChatMessage {
                role: ChatRole::Tool,
                content: "file contents".to_string(),
                media: Vec::new(),
                tool_call_id: Some("toolu_1".to_string()),
                tool_name: Some("read".to_string()),
                tool_calls: Vec::new(),
            },
        ],
        tools: vec![ToolDefinition {
            name: "read".to_string(),
            description: "Read a file".to_string(),
            parameters: json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"],
            }),
        }],
    };
    let config = ProviderConfig {
        model: ModelRef {
            provider: "anthropic".to_string(),
            id: "claude-sonnet-4-6".to_string(),
        },
        api: ProviderApi::Anthropic,
        base_url: None,
        auth: ProviderAuth::ApiKey("token".to_string()),
        thinking_level: None,
        thinking_budget_tokens: None,
        session_id: None,
        traits: ModelTraits::default(),
    };

    let body = anthropic_body(&config, &request);
    assert_eq!(body["tools"][0]["name"], "read");
    assert_eq!(
        body["tools"][0]["input_schema"],
        request.tools[0].parameters
    );
    assert_eq!(body["messages"][0]["content"][0]["type"], "tool_use");
    assert_eq!(body["messages"][0]["content"][0]["id"], "toolu_1");
    assert_eq!(body["messages"][1]["content"][0]["type"], "tool_result");
    assert_eq!(body["messages"][1]["content"][0]["tool_use_id"], "toolu_1");
}

#[test]
fn parses_anthropic_tool_use_events() {
    let response = json!({
        "content": [{
            "type": "tool_use",
            "id": "toolu_1",
            "name": "read",
            "input": { "path": "a.txt" }
        }],
        "stop_reason": "tool_use"
    });

    assert_eq!(
        parse_anthropic_response_events(&response),
        Some(vec![StreamEvent::ToolCall {
            id: "toolu_1".to_string(),
            name: "read".to_string(),
            arguments: r#"{"path":"a.txt"}"#.to_string(),
        }])
    );
}

#[test]
fn parses_anthropic_sse_tool_use_events() {
    let body = concat!(
        r#"event: message_start"#,
        "\n",
        r#"data: {"type":"message_start","message":{"id":"msg_1"}}"#,
        "\n\n",
        r#"event: content_block_start"#,
        "\n",
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        "\n\n",
        r#"event: content_block_delta"#,
        "\n",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"reading "}}"#,
        "\n\n",
        r#"event: content_block_start"#,
        "\n",
        r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"read","input":{}}}"#,
        "\n\n",
        r#"event: content_block_delta"#,
        "\n",
        r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\""}}"#,
        "\n\n",
        r#"event: content_block_delta"#,
        "\n",
        r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":":\"a.txt\"}"}}"#,
        "\n\n",
        r#"event: content_block_stop"#,
        "\n",
        r#"data: {"type":"content_block_stop","index":1}"#,
        "\n\n",
        r#"data: {"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#,
        "\n\n",
        r#"data: {"type":"message_stop"}"#,
        "\n\n",
    );

    assert_eq!(
        parse_anthropic_sse_events(body),
        Some(vec![
            StreamEvent::Text("reading ".to_string()),
            StreamEvent::ToolCall {
                id: "toolu_1".to_string(),
                name: "read".to_string(),
                arguments: r#"{"path":"a.txt"}"#.to_string(),
            }
        ])
    );
}

#[test]
fn google_body_preserves_tools_calls_and_results() {
    let request = ProviderRequest {
        max_output_tokens: None,
        system_prompt: None,
        messages: vec![
            ChatMessage {
                role: ChatRole::Assistant,
                content: String::new(),
                media: Vec::new(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: vec![ChatToolCall {
                    id: "read_0".to_string(),
                    name: "read".to_string(),
                    arguments: r#"{"path":"a.txt"}"#.to_string(),
                }],
            },
            ChatMessage {
                role: ChatRole::Tool,
                content: "file contents".to_string(),
                media: Vec::new(),
                tool_call_id: Some("read_0".to_string()),
                tool_name: Some("read".to_string()),
                tool_calls: Vec::new(),
            },
        ],
        tools: vec![ToolDefinition {
            name: "read".to_string(),
            description: "Read a file".to_string(),
            parameters: json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"],
            }),
        }],
    };
    let config = ProviderConfig {
        model: ModelRef {
            provider: "google".to_string(),
            id: "gemini-2.5-pro".to_string(),
        },
        api: ProviderApi::Google,
        base_url: None,
        auth: ProviderAuth::ApiKey("token".to_string()),
        thinking_level: None,
        thinking_budget_tokens: None,
        session_id: None,
        traits: ModelTraits::default(),
    };

    let body = google_body(&config, &request);
    assert_eq!(body["tools"][0]["functionDeclarations"][0]["name"], "read");
    assert_eq!(
        body["contents"][0]["parts"][0]["functionCall"]["name"],
        "read"
    );
    assert_eq!(
        body["contents"][1]["parts"][0]["functionResponse"]["name"],
        "read"
    );
    assert_eq!(
        body["contents"][1]["parts"][0]["functionResponse"]["response"]["output"],
        "file contents"
    );
}

#[test]
fn google_tool_result_images_follow_gemini_version_routing() {
    let fixture = serde_json::from_str::<Value>(include_str!(
        "../../../tests/fixtures/ts-parity/google-tool-image-routing.json"
    ))
    .expect("parse TS parity fixture");
    let messages = vec![ChatMessage {
        role: ChatRole::Tool,
        content: "alpha text".to_string(),
        media: vec![MediaInput {
            mime_type: "image/png".to_string(),
            data_base64: "YWJj".to_string(),
            path: Some("image.png".to_string()),
            width: Some(1),
            height: Some(1),
        }],
        tool_call_id: Some("call_img".to_string()),
        tool_name: Some("read".to_string()),
        tool_calls: Vec::new(),
    }];
    let mut config = ProviderConfig {
        model: ModelRef {
            provider: "google".to_string(),
            id: "gemini-2.5-flash".to_string(),
        },
        api: ProviderApi::Google,
        base_url: None,
        auth: ProviderAuth::ApiKey("token".to_string()),
        thinking_level: None,
        thinking_budget_tokens: None,
        session_id: None,
        traits: ModelTraits::default(),
    };

    let gemini_2 = google_messages(&config, &messages);
    assert_eq!(json!(gemini_2), fixture["routing"]["gemini2"]);

    config.model.id = "gemini-3-pro-preview".to_string();
    let gemini_3 = google_messages(&config, &messages);
    assert_eq!(json!(gemini_3), fixture["routing"]["gemini3"]);
}

#[test]
fn parses_google_function_call_events() {
    let body = concat!(
        "data: {\"candidates\":[{\"content\":{\"parts\":[",
        "{\"functionCall\":{\"name\":\"read\",\"args\":{\"path\":\"a.txt\"}}}",
        "]},\"finishReason\":\"STOP\"}]}\n\n"
    );

    assert_eq!(
        parse_google_sse_events(body),
        Some(vec![StreamEvent::ToolCall {
            id: "read_0".to_string(),
            name: "read".to_string(),
            arguments: r#"{"path":"a.txt"}"#.to_string(),
        }])
    );
}

#[test]
fn mistral_body_preserves_tools_calls_and_results() {
    let request = ProviderRequest {
        max_output_tokens: None,
        system_prompt: None,
        messages: vec![
            ChatMessage {
                role: ChatRole::Assistant,
                content: String::new(),
                media: Vec::new(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: vec![ChatToolCall {
                    id: "call_1".to_string(),
                    name: "read".to_string(),
                    arguments: r#"{"path":"a.txt"}"#.to_string(),
                }],
            },
            ChatMessage {
                role: ChatRole::Tool,
                content: "file contents".to_string(),
                media: Vec::new(),
                tool_call_id: Some("call_1".to_string()),
                tool_name: Some("read".to_string()),
                tool_calls: Vec::new(),
            },
        ],
        tools: vec![ToolDefinition {
            name: "read".to_string(),
            description: "Read a file".to_string(),
            parameters: json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"],
            }),
        }],
    };
    let config = ProviderConfig {
        model: ModelRef {
            provider: "mistral".to_string(),
            id: "devstral-medium-latest".to_string(),
        },
        api: ProviderApi::Mistral,
        base_url: None,
        auth: ProviderAuth::ApiKey("token".to_string()),
        thinking_level: None,
        thinking_budget_tokens: None,
        session_id: None,
        traits: ModelTraits::default(),
    };

    let body = mistral_chat_body(&config, &request);
    assert_eq!(body["tools"][0]["function"]["name"], "read");
    let fixture = serde_json::from_str::<serde_json::Value>(include_str!(
        "../../../tests/fixtures/ts-parity/mistral-tool-id.json"
    ))
    .expect("parse TS parity fixture");
    assert_eq!(body["messages"], fixture["request"]["body"]["messages"]);
}

#[test]
fn bedrock_body_preserves_tools_calls_and_results() {
    let request = ProviderRequest {
        max_output_tokens: None,
        system_prompt: None,
        messages: vec![
            ChatMessage {
                role: ChatRole::Assistant,
                content: String::new(),
                media: Vec::new(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: vec![ChatToolCall {
                    id: "tooluse_1".to_string(),
                    name: "read".to_string(),
                    arguments: r#"{"path":"a.txt"}"#.to_string(),
                }],
            },
            ChatMessage {
                role: ChatRole::Tool,
                content: "file contents".to_string(),
                media: Vec::new(),
                tool_call_id: Some("tooluse_1".to_string()),
                tool_name: Some("read".to_string()),
                tool_calls: Vec::new(),
            },
        ],
        tools: vec![ToolDefinition {
            name: "read".to_string(),
            description: "Read a file".to_string(),
            parameters: json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"],
            }),
        }],
    };
    let config = ProviderConfig {
        model: ModelRef {
            provider: "amazon-bedrock".to_string(),
            id: "us.anthropic.claude-opus-4-6-20260112-v1:0".to_string(),
        },
        api: ProviderApi::Bedrock,
        base_url: None,
        auth: ProviderAuth::ApiKey("token".to_string()),
        thinking_level: None,
        thinking_budget_tokens: None,
        session_id: None,
        traits: ModelTraits::default(),
    };

    let body = bedrock_body(&config, &request);
    assert_eq!(body["toolConfig"]["tools"][0]["toolSpec"]["name"], "read");
    assert_eq!(
        body["messages"][0]["content"][0]["toolUse"]["toolUseId"],
        "tooluse_1"
    );
    assert_eq!(
        body["messages"][1]["content"][0]["toolResult"]["toolUseId"],
        "tooluse_1"
    );
}

#[test]
fn parses_bedrock_tool_use_events() {
    let response = json!({
        "output": {
            "message": {
                "content": [{
                    "toolUse": {
                        "toolUseId": "tooluse_1",
                        "name": "read",
                        "input": { "path": "a.txt" }
                    }
                }]
            }
        },
        "stopReason": "tool_use"
    });

    assert_eq!(
        parse_bedrock_response_events(&response),
        Some(vec![StreamEvent::ToolCall {
            id: "tooluse_1".to_string(),
            name: "read".to_string(),
            arguments: r#"{"path":"a.txt"}"#.to_string(),
        }])
    );
}

#[test]
fn openrouter_images_body_matches_ts_shape() {
    let media = MediaInput {
        mime_type: "image/png".to_string(),
        data_base64: "aW1hZ2U=".to_string(),
        path: Some("fixture.png".to_string()),
        width: Some(1),
        height: Some(1),
    };
    let config = ImageProviderConfig {
        model: ModelRef {
            provider: "openrouter".to_string(),
            id: "google/gemini-3.1-flash-image-preview".to_string(),
        },
        api: ImageProviderApi::OpenRouterImages,
        base_url: Some("https://openrouter.ai/api/v1".to_string()),
        auth: ProviderAuth::ApiKey("token".to_string()),
        output_modalities: vec!["image".to_string(), "text".to_string()],
    };
    let body = openrouter_images_body(
        &config,
        &[
            ImageGenerationInput::Text {
                text: "Generate a dog".to_string(),
            },
            ImageGenerationInput::Image { media },
        ],
    );

    assert_eq!(body["model"], "google/gemini-3.1-flash-image-preview");
    assert_eq!(body["stream"], false);
    assert_eq!(body["modalities"], json!(["image", "text"]));
    assert_eq!(body["messages"][0]["role"], "user");
    assert_eq!(body["messages"][0]["content"][0]["type"], "text");
    assert_eq!(body["messages"][0]["content"][0]["text"], "Generate a dog");
    assert_eq!(body["messages"][0]["content"][1]["type"], "image_url");
    assert_eq!(
        body["messages"][0]["content"][1]["image_url"]["url"],
        "data:image/png;base64,aW1hZ2U="
    );
}

#[test]
fn parses_openrouter_images_response() {
    let response = json!({
        "id": "img-1",
        "usage": {
            "prompt_tokens": 12,
            "completion_tokens": 34
        },
        "choices": [{
            "message": {
                "content": "Here is your image.",
                "images": [
                    { "image_url": "data:image/png;base64,ZmFrZS1wbmc=" },
                    { "image_url": { "url": "data:image/jpeg;base64,ZmFrZS1qcGc=" } }
                ]
            }
        }]
    });

    let parsed = parse_openrouter_images_response(response).expect("parse image response");
    assert_eq!(parsed.response_id.as_deref(), Some("img-1"));
    assert_eq!(parsed.input_tokens, 12);
    assert_eq!(parsed.output_tokens, 34);
    assert_eq!(
        parsed.output[0],
        ImageGenerationOutput::Text("Here is your image.".to_string())
    );
    assert_eq!(
        parsed.output[1],
        ImageGenerationOutput::Image {
            mime_type: "image/png".to_string(),
            data_base64: "ZmFrZS1wbmc=".to_string(),
        }
    );
    assert_eq!(
        parsed.output[2],
        ImageGenerationOutput::Image {
            mime_type: "image/jpeg".to_string(),
            data_base64: "ZmFrZS1qcGc=".to_string(),
        }
    );
}

#[test]
fn provider_parity_request_helpers_match_expected_shapes() {
    assert_eq!(
        codex_responses_url("https://chatgpt.com/backend-api").expect("codex url"),
        "https://chatgpt.com/backend-api/codex/responses"
    );
    assert_eq!(
        anthropic_messages_url(&ProviderConfig {
            model: ModelRef {
                provider: "fireworks".to_string(),
                id: "accounts/fireworks/models/deepseek-v4-pro".to_string(),
            },
            api: ProviderApi::Anthropic,
            base_url: Some("https://api.fireworks.ai/inference".to_string()),
            auth: ProviderAuth::ApiKey("token".to_string()),
            thinking_level: None,
            thinking_budget_tokens: None,
            session_id: None,
            traits: ModelTraits::default(),
        })
        .expect("anthropic compatible url"),
        "https://api.fireworks.ai/inference/v1/messages"
    );
    assert_eq!(
        azure_openai_responses_url("https://example.openai.azure.com/openai/v1"),
        "https://example.openai.azure.com/openai/v1/responses?api-version=v1"
    );
    assert_eq!(
        encode_path_segment("us.anthropic.claude-opus-4-6-v1"),
        "us.anthropic.claude-opus-4-6-v1"
    );
    assert_eq!(
        encode_path_segment("workers-ai/@cf/model"),
        "workers-ai%2F%40cf%2Fmodel"
    );
}

/// A request config for a built-in model as noel builds one.
fn builtin_config(model: &catalog::CatalogModel) -> ProviderConfig {
    ProviderConfig {
        model: ModelRef {
            provider: model.provider.clone(),
            id: model.id.clone(),
        },
        api: model.api.clone(),
        base_url: model.base_url.clone(),
        auth: ProviderAuth::ApiKey("token".to_string()),
        thinking_level: None,
        thinking_budget_tokens: None,
        session_id: None,
        traits: ModelTraits::from(model),
    }
}

/// A request config for the model `provider` starts on.
fn provider_config(provider: &str) -> ProviderConfig {
    builtin_config(
        catalog::provider_base_model(provider)
            .unwrap_or_else(|| panic!("{provider} has no built-in model")),
    )
}

/// Upstream 4c992154: a request that never reached the provider says why.
#[tokio::test]
async fn a_connection_error_keeps_its_underlying_cause() {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port();
    let error = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}/"))
        .send()
        .await
        .expect_err("closed port");
    let message = ProviderError::Request(error).to_string();
    assert!(message.to_lowercase().contains("refused"), "{message}");
}

/// The catalog's base URLs are API roots, as upstream's SDK clients take
/// them. Every built-in model's request goes to its own base URL plus
/// the path its API defines; no provider is special.
#[test]
fn every_catalog_base_url_gets_its_apis_path() {
    let mut checked = BTreeMap::new();
    for model in catalog::builtin_models() {
        // A URL from the environment (Azure, Cloudflare) is covered by
        // its own test.
        let Some(base) = model.base_url.as_deref().filter(|base| !base.contains('{')) else {
            continue;
        };
        let config = builtin_config(model);
        let (url, path) = match model.api {
            ProviderApi::Faux => continue,
            ProviderApi::OpenAi => {
                let provider = OpenAiProvider {
                    config,
                    client: reqwest::Client::new(),
                };
                (
                    format!("{}/chat/completions", provider.chat_base_url().unwrap()),
                    "/chat/completions".to_string(),
                )
            }
            ProviderApi::OpenAiResponses => {
                let provider = OpenAiResponsesProvider {
                    config,
                    endpoint: OpenAiResponsesEndpoint::OpenAi,
                    client: reqwest::Client::new(),
                };
                (
                    provider.request_parts(false).unwrap().0,
                    "/responses".to_string(),
                )
            }
            ProviderApi::OpenAiCodexResponses => (
                codex_responses_url(base).unwrap(),
                "/codex/responses".to_string(),
            ),
            ProviderApi::Anthropic => (
                anthropic_messages_url(&config).unwrap(),
                "/v1/messages".to_string(),
            ),
            ProviderApi::Google => (
                google_url(&config).unwrap(),
                format!(
                    "/models/{}:streamGenerateContent?alt=sse",
                    encode_path_segment(&model.id)
                ),
            ),
            ProviderApi::Mistral => (
                mistral_chat_url(&config).unwrap(),
                "/v1/chat/completions".to_string(),
            ),
            // Bedrock and Vertex build their URLs from the region and
            // project; Azure has no catalog URL.
            ProviderApi::Bedrock
            | ProviderApi::GoogleVertex
            | ProviderApi::AzureOpenAiResponses => continue,
        };
        assert_eq!(
            url,
            format!("{base}{path}"),
            "{}/{}",
            model.provider,
            model.id
        );
        *checked.entry(model.api.upstream_name()).or_insert(0) += 1;
    }
    for api in [
        "openai-completions",
        "openai-responses",
        "openai-codex-responses",
        "anthropic-messages",
        "google-generative-ai",
        "mistral-conversations",
    ] {
        assert!(
            checked.get(api).is_some_and(|count| *count > 5),
            "{api}: {checked:?}"
        );
    }
}

#[test]
fn base_urls_that_already_name_the_api_version_are_kept() {
    let anthropic = |base: &str| {
        let mut config = provider_config("anthropic");
        config.base_url = Some(base.to_string());
        anthropic_messages_url(&config).unwrap()
    };
    assert_eq!(
        anthropic("https://api.anthropic.com/v1"),
        "https://api.anthropic.com/v1/messages"
    );
    assert_eq!(
        anthropic("https://proxy.example/anthropic/"),
        "https://proxy.example/anthropic/v1/messages"
    );
    assert_eq!(
        anthropic("https://proxy.example/v1/messages"),
        "https://proxy.example/v1/messages"
    );
    let mut mistral = provider_config("mistral");
    mistral.base_url = Some("https://api.mistral.ai/v1".to_string());
    assert_eq!(
        mistral_chat_url(&mistral).unwrap(),
        "https://api.mistral.ai/v1/chat/completions"
    );
    mistral.base_url = None;
    assert_eq!(
        mistral_chat_url(&mistral).unwrap(),
        "https://api.mistral.ai/v1/chat/completions"
    );
}

/// Azure's models carry no URL. A chat-completions model of Azure must
/// go to the Azure resource or fail, never to OpenAI's endpoint with an
/// Azure key.
#[test]
fn an_azure_chat_model_never_falls_back_to_openais_endpoint() {
    let azure_chat = catalog::builtin_models()
        .iter()
        .find(|model| model.provider == AZURE_PROVIDER && model.api == ProviderApi::OpenAi)
        .expect("an Azure chat-completions model");
    assert_eq!(azure_chat.base_url, None);
    for provider in [AZURE_PROVIDER, LEGACY_AZURE_PROVIDER] {
        let mut config = builtin_config(azure_chat);
        config.model.provider = provider.to_string();
        let resolved = OpenAiProvider {
            config: config.clone(),
            client: reqwest::Client::new(),
        }
        .chat_base_url();
        // Without AZURE_OPENAI_BASE_URL or AZURE_OPENAI_RESOURCE_NAME
        // this is an error; with one, that resource.
        assert!(
            !resolved.is_ok_and(|url| url.contains("api.openai.com")),
            "{provider}"
        );
        config.base_url = Some("https://example.openai.azure.com".to_string());
        assert_eq!(
            OpenAiProvider {
                config,
                client: reqwest::Client::new(),
            }
            .chat_base_url()
            .unwrap(),
            "https://example.openai.azure.com/openai/v1"
        );
    }
    // Any other provider without a URL keeps OpenAI's default.
    let mut openai = provider_config("openai");
    openai.api = ProviderApi::OpenAi;
    openai.base_url = None;
    assert_eq!(
        OpenAiProvider {
            config: openai,
            client: reqwest::Client::new(),
        }
        .chat_base_url()
        .unwrap(),
        "https://api.openai.com/v1"
    );
}

fn effort_request() -> ProviderRequest {
    ProviderRequest {
        max_output_tokens: None,
        system_prompt: None,
        messages: Vec::new(),
        tools: Vec::new(),
    }
}

/// A request config for a reasoning model with this thinking level map.
fn thinker(provider: &str, id: &str, api: ProviderApi, map: Value, level: &str) -> ProviderConfig {
    ProviderConfig {
        model: ModelRef {
            provider: provider.to_string(),
            id: id.to_string(),
        },
        api,
        base_url: None,
        auth: ProviderAuth::ApiKey("token".to_string()),
        thinking_level: Some(level.to_string()),
        thinking_budget_tokens: None,
        session_id: None,
        traits: ModelTraits {
            reasoning: Some(true),
            thinking_level_map: Some(serde_json::from_value(map).expect("level map")),
            ..ModelTraits::default()
        },
    }
}

/// The effort a request names is the level clamped to the model and
/// spelled as the model's catalog entry spells it.
#[test]
fn reasoning_effort_follows_the_models_thinking_level_map() {
    let map = json!({ "off": "none", "minimal": null, "xhigh": "xhigh" });
    let responses = |level: &str| {
        let config = thinker(
            "openai",
            "gpt-x",
            ProviderApi::OpenAiResponses,
            map.clone(),
            level,
        );
        openai_responses_body(&config, &effort_request())["reasoning"]["effort"].clone()
    };
    // `off` as the model spells it; a level the model lacks becomes the
    // nearest one it has, above first.
    assert_eq!(responses("off"), "none");
    assert_eq!(responses("minimal"), "low");
    assert_eq!(responses("high"), "high");
    assert_eq!(responses("xhigh"), "xhigh");
    assert_eq!(responses("max"), "xhigh");
    // A model may spell a level differently.
    let codex = thinker(
        "openai-codex",
        "gpt-x",
        ProviderApi::OpenAiCodexResponses,
        json!({ "minimal": "low" }),
        "minimal",
    );
    assert_eq!(
        openai_codex_responses_body(&codex, &effort_request())["reasoning"]["effort"],
        "low"
    );
    // OpenRouter is sent the model's own names too, not a collapsed set.
    let openrouter = |level: &str| {
        let config = thinker(
            "openrouter",
            "lab/model",
            ProviderApi::OpenAi,
            json!({ "off": null, "medium": null, "xhigh": "xhigh", "max": "ultra" }),
            level,
        );
        openai_chat_completions_body(&config, &effort_request())["reasoning"]["effort"].clone()
    };
    assert_eq!(openrouter("off"), "minimal");
    assert_eq!(openrouter("medium"), "high");
    assert_eq!(openrouter("xhigh"), "xhigh");
    assert_eq!(openrouter("max"), "ultra");
    // A model known to have no reasoning is sent nothing.
    let mut plain = thinker(
        "openai",
        "gpt-x",
        ProviderApi::OpenAiResponses,
        json!({}),
        "high",
    );
    plain.traits.reasoning = Some(false);
    assert_eq!(
        openai_responses_body(&plain, &effort_request()).get("reasoning"),
        None
    );
    // Copilot is sent nothing for `off`.
    let copilot = |level: &str| {
        let config = thinker(
            "github-copilot",
            "gpt-x",
            ProviderApi::OpenAiResponses,
            json!({ "off": "none" }),
            level,
        );
        github_copilot_responses_body(&config, &effort_request())
    };
    assert_eq!(copilot("off").get("reasoning"), None);
    assert_eq!(copilot("high")["reasoning"]["effort"], "high");
    // Nothing known of the model: the level as configured.
    let mut unknown = thinker(
        "openai",
        "gpt-x",
        ProviderApi::OpenAiResponses,
        json!({}),
        "minimal",
    );
    unknown.traits = ModelTraits::default();
    assert_eq!(
        openai_responses_body(&unknown, &effort_request())["reasoning"]["effort"],
        "minimal"
    );
    unknown.thinking_level = Some("max".to_string());
    assert_eq!(
        openai_responses_body(&unknown, &effort_request())["reasoning"]["effort"],
        "xhigh"
    );
}

#[test]
fn anthropic_and_bedrock_thinking_follow_the_models_thinking_level_map() {
    let anthropic = |id: &str, map: Value, level: &str| {
        let config = thinker("anthropic", id, ProviderApi::Anthropic, map, level);
        anthropic_body(&config, &effort_request())
    };
    // The model's own names for the top levels.
    assert_eq!(
        anthropic("claude-sonnet-4-6", json!({ "max": "max" }), "max")["output_config"]["effort"],
        "max"
    );
    assert_eq!(
        anthropic("claude-opus-4-8", json!({ "xhigh": "xhigh" }), "xhigh")["output_config"]
            ["effort"],
        "xhigh"
    );
    // Off is said where the model can turn thinking off, and left
    // unsaid where it cannot.
    assert_eq!(
        anthropic("claude-opus-4-8", json!({}), "off")["thinking"]["type"],
        "disabled"
    );
    assert_eq!(
        anthropic("claude-opus-5", json!({ "off": null }), "off").get("thinking"),
        None
    );
    // A model known to have no reasoning is sent no thinking at all.
    let mut plain = thinker(
        "anthropic",
        "claude-opus-4-8",
        ProviderApi::Anthropic,
        json!({}),
        "high",
    );
    plain.traits.reasoning = Some(false);
    assert_eq!(
        anthropic_body(&plain, &effort_request()).get("thinking"),
        None
    );

    let bedrock = thinker(
        "amazon-bedrock",
        "us.anthropic.claude-sonnet-4-6",
        ProviderApi::Bedrock,
        json!({ "max": "max" }),
        "max",
    );
    assert_eq!(
        bedrock_body(&bedrock, &effort_request())["additionalModelRequestFields"]["output_config"]
            ["effort"],
        "max"
    );
}

#[test]
fn anthropic_compatible_headers_cover_kimi_static_headers() {
    let headers = anthropic_headers(&ProviderConfig {
        model: ModelRef {
            provider: "kimi-coding".to_string(),
            id: "kimi-for-coding".to_string(),
        },
        api: ProviderApi::Anthropic,
        base_url: Some("https://api.kimi.com/coding".to_string()),
        auth: ProviderAuth::ApiKey("token".to_string()),
        thinking_level: None,
        thinking_budget_tokens: None,
        session_id: None,
        traits: ModelTraits::default(),
    })
    .expect("headers");

    assert_eq!(
        headers
            .get(USER_AGENT)
            .and_then(|value| value.to_str().ok()),
        Some("KimiCLI/1.5")
    );
}

#[test]
fn thinking_levels_map_to_provider_payloads() {
    let request = ProviderRequest {
        max_output_tokens: None,
        system_prompt: Some("system".to_string()),
        messages: vec![ChatMessage {
            role: ChatRole::User,
            content: "hello".to_string(),
            media: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
        }],
        tools: Vec::new(),
    };
    let opus = ProviderConfig {
        model: ModelRef {
            provider: "anthropic".to_string(),
            id: "claude-opus-4-7".to_string(),
        },
        api: ProviderApi::Anthropic,
        base_url: None,
        auth: ProviderAuth::ApiKey("token".to_string()),
        thinking_level: Some("max".to_string()),
        thinking_budget_tokens: None,
        session_id: None,
        traits: ModelTraits::default(),
    };
    let opus_body = anthropic_body(&opus, &request);
    assert_eq!(opus_body["thinking"]["type"], "adaptive");
    assert_eq!(opus_body["output_config"]["effort"], "max");

    let codex = ProviderConfig {
        model: ModelRef {
            provider: "openai-codex".to_string(),
            id: "gpt-5.5".to_string(),
        },
        api: ProviderApi::OpenAiCodexResponses,
        base_url: None,
        auth: ProviderAuth::ApiKey("token".to_string()),
        thinking_level: Some("xhigh".to_string()),
        thinking_budget_tokens: None,
        session_id: None,
        traits: ModelTraits::default(),
    };
    let codex_body = openai_responses_body(&codex, &request);
    assert_eq!(codex_body["reasoning"]["effort"], "xhigh");
}

#[test]
fn adaptive_thinking_support_is_version_parsed() {
    assert_eq!(
        claude_model_version("claude-opus-5-5", "opus"),
        Some((5, 5))
    );
    assert_eq!(
        claude_model_version("us.anthropic.claude-opus-4-6-v1", "opus"),
        Some((4, 6))
    );
    assert_eq!(
        claude_model_version("claude-opus-4.7", "opus"),
        Some((4, 7))
    );
    assert_eq!(claude_model_version("claude-3-opus-20240229", "opus"), None);
    assert_eq!(claude_model_version("gpt-5.5", "opus"), None);

    for id in [
        "claude-opus-4-6",
        "claude-opus-4-7",
        "claude-opus-5-5",
        "claude-sonnet-4-6",
        "us.anthropic.claude-opus-5-5-v1",
    ] {
        assert!(anthropic_supports_adaptive_thinking(id), "{id}");
    }
    for id in ["claude-sonnet-4-5", "claude-3-5-haiku", "claude-opus"] {
        assert!(!anthropic_supports_adaptive_thinking(id), "{id}");
    }
}

/// An Anthropic request config for a model nothing is known about
/// beyond its id.
fn anthropic_test_config(id: &str, thinking: Option<&str>, budget: Option<u64>) -> ProviderConfig {
    ProviderConfig {
        model: ModelRef {
            provider: "anthropic".to_string(),
            id: id.to_string(),
        },
        api: ProviderApi::Anthropic,
        base_url: None,
        auth: ProviderAuth::ApiKey("token".to_string()),
        thinking_level: thinking.map(str::to_string),
        thinking_budget_tokens: budget,
        session_id: None,
        traits: ModelTraits::default(),
    }
}

/// The config with the catalog's traits for its model, as noel builds
/// one: the output cap, the context window, and what the model supports
/// of thinking. The thinking level stays as the fixture asked for it;
/// request builders clamp it, as upstream's do.
fn with_catalog(mut config: ProviderConfig) -> ProviderConfig {
    let model = catalog::builtin_model(&config.model.provider, &config.model.id)
        .unwrap_or_else(|| panic!("{:?} is not a built-in model", config.model));
    config.traits = ModelTraits::from(model);
    config
}

fn uncapped_request(system_prompt: &str) -> ProviderRequest {
    ProviderRequest {
        max_output_tokens: None,
        system_prompt: Some(system_prompt.to_string()),
        messages: vec![ChatMessage {
            role: ChatRole::User,
            content: "write the file".to_string(),
            media: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
        }],
        tools: Vec::new(),
    }
}

#[test]
fn anthropic_requests_without_a_cap_use_the_model_output_limit() {
    let request = uncapped_request("system");
    // Every Anthropic model of the catalog is capped at its `maxTokens`.
    let mut caps = BTreeSet::new();
    for model in catalog::builtin_models() {
        if model.provider != "anthropic" {
            continue;
        }
        let config = with_catalog(anthropic_test_config(&model.id, None, None));
        let body = anthropic_body(&config, &request);
        assert_eq!(body["max_tokens"], model.max_tokens, "{}", model.id);
        caps.insert(model.max_tokens);
    }
    // The catalog tells models apart; this is not one cap for all.
    assert!(caps.len() > 1, "{caps:?}");
    // A model nothing is known about gets upstream's default for a
    // model without a cap.
    let body = anthropic_body(
        &anthropic_test_config("claude-unlisted", None, None),
        &request,
    );
    assert_eq!(body["max_tokens"], 16_384);
}

#[test]
fn anthropic_default_cap_leaves_room_in_the_context_window() {
    // The input estimates as 160_004 tokens (560k + 14 characters at 3.5
    // per token); 200k minus that and the 4096 safety margin remain.
    let request = uncapped_request(&"x".repeat(560_000));
    let with_limits = |mut config: ProviderConfig| {
        config.traits = ModelTraits {
            context_window: Some(200_000),
            max_tokens: Some(64_000),
            ..ModelTraits::default()
        };
        config
    };
    let config = with_limits(anthropic_test_config("claude-sonnet-4-5", None, None));
    assert_eq!(anthropic_body(&config, &request)["max_tokens"], 35_900);

    // A thinking budget stays below the cap and leaves 1024 for the answer.
    let thinking = with_limits(anthropic_test_config(
        "claude-sonnet-4-5",
        Some("high"),
        Some(50_000),
    ));
    let body = anthropic_body(&thinking, &request);
    assert_eq!(body["thinking"]["budget_tokens"], 34_876);
    let body = anthropic_body(&thinking, &uncapped_request("system"));
    assert_eq!(body["max_tokens"], 64_000);
    assert_eq!(body["thinking"]["budget_tokens"], 50_000);

    // An explicit cap is still sent as given.
    let capped = ProviderRequest {
        max_output_tokens: Some(2048),
        ..request
    };
    let body = anthropic_body(&thinking, &capped);
    assert_eq!(body["max_tokens"], 2048);
    assert_eq!(body["thinking"]["budget_tokens"], 1024);
}

#[test]
fn opus_5_5_uses_adaptive_thinking_with_xhigh() {
    let request = ProviderRequest {
        max_output_tokens: None,
        system_prompt: None,
        messages: vec![ChatMessage {
            role: ChatRole::User,
            content: "hello".to_string(),
            media: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
        }],
        tools: Vec::new(),
    };
    let opus = ProviderConfig {
        model: ModelRef {
            provider: "anthropic".to_string(),
            id: "claude-opus-5-5".to_string(),
        },
        api: ProviderApi::Anthropic,
        base_url: None,
        auth: ProviderAuth::ApiKey("token".to_string()),
        thinking_level: Some("xhigh".to_string()),
        thinking_budget_tokens: None,
        session_id: None,
        traits: ModelTraits::default(),
    };
    let body = anthropic_body(&opus, &request);
    assert_eq!(body["thinking"]["type"], "adaptive");
    assert_eq!(body["output_config"]["effort"], "xhigh");
    assert_eq!(
        bedrock_adaptive_effort("us.anthropic.claude-opus-5-5-v1", "xhigh"),
        "xhigh"
    );
    assert_eq!(
        bedrock_adaptive_effort("us.anthropic.claude-opus-4-6-v1", "xhigh"),
        "high"
    );
}

#[test]
fn claude_code_version_resolution_prefers_env_then_override() {
    assert_eq!(
        resolve_claude_code_version(None, None),
        DEFAULT_CLAUDE_CODE_VERSION
    );
    assert_eq!(
        resolve_claude_code_version(None, Some("2.1.300")),
        "2.1.300"
    );
    assert_eq!(
        resolve_claude_code_version(Some("9.9.9"), Some("2.1.300")),
        "9.9.9"
    );
    assert_eq!(
        resolve_claude_code_version(Some("   "), Some("")),
        DEFAULT_CLAUDE_CODE_VERSION
    );
}

#[test]
fn anthropic_claude_code_oauth_matches_ts_identity_shape() {
    // The runtime version tracks the latest published Claude Code release;
    // pin it to the compiled fallback, which must equal upstream's pinned
    // version, to compare the identity shape.
    set_claude_code_version(DEFAULT_CLAUDE_CODE_VERSION);
    let fixture = serde_json::from_str::<Value>(include_str!(
        "../../../tests/fixtures/ts-parity/anthropic-claude-code-oauth.json"
    ))
    .expect("parse TS parity fixture");
    let request = ProviderRequest {
        // The TS fixture script passes an explicit `maxTokens: 4096`.
        max_output_tokens: Some(4096),
        system_prompt: Some("pi rust cli".to_string()),
        messages: vec![ChatMessage {
            role: ChatRole::User,
            content: "hello".to_string(),
            media: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
        }],
        tools: Vec::new(),
    };
    let config = with_catalog(ProviderConfig {
        model: ModelRef {
            provider: "anthropic".to_string(),
            id: "claude-sonnet-4-6".to_string(),
        },
        api: ProviderApi::Anthropic,
        base_url: None,
        auth: ProviderAuth::ClaudeCodeOAuth {
            access_token: "access-token".to_string(),
        },
        thinking_level: Some("off".to_string()),
        thinking_budget_tokens: None,
        session_id: None,
        traits: ModelTraits::default(),
    });
    let fixture_request = &fixture["request"];

    let headers = anthropic_headers(&config).expect("anthropic headers");
    for name in [
        "accept",
        "anthropic-beta",
        "anthropic-dangerous-direct-browser-access",
        "anthropic-version",
        "content-type",
        "user-agent",
        "x-app",
    ] {
        assert_eq!(
            headers.get(name).and_then(|value| value.to_str().ok()),
            fixture_request["headers"][name].as_str(),
            "header {name}"
        );
    }

    let body = anthropic_body(&config, &request);
    assert_eq!(body, fixture_request["body"]);
}

#[test]
fn anthropic_api_key_matches_ts_body_shape() {
    let fixture = serde_json::from_str::<Value>(include_str!(
        "../../../tests/fixtures/ts-parity/anthropic-api-key.json"
    ))
    .expect("parse TS parity fixture");
    let request = ProviderRequest {
        // The TS fixture script passes an explicit `maxTokens: 4096`.
        max_output_tokens: Some(4096),
        system_prompt: Some("pi rust cli".to_string()),
        messages: vec![ChatMessage {
            role: ChatRole::User,
            content: "hello".to_string(),
            media: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
        }],
        tools: Vec::new(),
    };
    let config = with_catalog(ProviderConfig {
        model: ModelRef {
            provider: "anthropic".to_string(),
            id: "claude-sonnet-4-6".to_string(),
        },
        api: ProviderApi::Anthropic,
        base_url: None,
        auth: ProviderAuth::ApiKey("api-key".to_string()),
        thinking_level: Some("off".to_string()),
        thinking_budget_tokens: None,
        session_id: None,
        traits: ModelTraits::default(),
    });
    let fixture_request = &fixture["request"];

    let headers = anthropic_headers(&config).expect("anthropic headers");
    for name in ["accept", "anthropic-version", "content-type"] {
        assert_eq!(
            headers.get(name).and_then(|value| value.to_str().ok()),
            fixture_request["headers"][name].as_str(),
            "header {name}"
        );
    }
    assert!(headers.get("x-api-key").is_some());
    assert!(headers.get(AUTHORIZATION).is_none());
    assert!(headers.get("anthropic-beta").is_none());
    assert!(headers.get("x-app").is_none());

    let body = anthropic_body(&config, &request);
    assert_eq!(body, fixture_request["body"]);
}

#[test]
fn openai_compatible_headers_cover_copilot_and_cloudflare() {
    let copilot = ProviderConfig {
        model: ModelRef {
            provider: "github-copilot".to_string(),
            id: "gpt-5.4".to_string(),
        },
        api: ProviderApi::OpenAi,
        base_url: Some("https://api.individual.githubcopilot.com".to_string()),
        auth: ProviderAuth::ApiKey("token".to_string()),
        thinking_level: None,
        thinking_budget_tokens: None,
        session_id: None,
        traits: ModelTraits::default(),
    };
    let headers = openai_compatible_headers(&copilot, "token", true).expect("copilot headers");
    assert_eq!(
        headers
            .get("Copilot-Integration-Id")
            .and_then(|value| value.to_str().ok()),
        Some("vscode-chat")
    );
    assert_eq!(
        headers
            .get("Copilot-Vision-Request")
            .and_then(|value| value.to_str().ok()),
        Some("true")
    );

    let cloudflare = ProviderConfig {
        model: ModelRef {
            provider: "cloudflare-ai-gateway".to_string(),
            id: "workers-ai/@cf/moonshotai/kimi-k2.6".to_string(),
        },
        api: ProviderApi::OpenAi,
        base_url: None,
        auth: ProviderAuth::ApiKey("token".to_string()),
        thinking_level: None,
        thinking_budget_tokens: None,
        session_id: Some("session-test".to_string()),
        traits: ModelTraits::default(),
    };
    let headers =
        openai_compatible_headers(&cloudflare, "token", false).expect("cloudflare headers");
    assert_eq!(
        headers
            .get("cf-aig-authorization")
            .and_then(|value| value.to_str().ok()),
        Some("Bearer token")
    );
    assert_eq!(
        headers
            .get("x-session-affinity")
            .and_then(|value| value.to_str().ok()),
        Some("session-test")
    );
    assert!(headers.get(AUTHORIZATION).is_none());
}

#[test]
fn openai_codex_oauth_matches_ts_request_shape() {
    let fixture = serde_json::from_str::<Value>(include_str!(
        "../../../tests/fixtures/ts-parity/openai-codex-chatgpt-oauth.json"
    ))
    .expect("parse TS parity fixture");
    let request = ProviderRequest {
        max_output_tokens: None,
        system_prompt: Some("pi rust cli".to_string()),
        messages: vec![ChatMessage {
            role: ChatRole::User,
            content: "hello".to_string(),
            media: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
        }],
        tools: Vec::new(),
    };
    let config = with_catalog(ProviderConfig {
        model: ModelRef {
            provider: "openai-codex".to_string(),
            id: "gpt-6.1-sol".to_string(),
        },
        api: ProviderApi::OpenAiCodexResponses,
        base_url: None,
        auth: ProviderAuth::ChatGptOAuth {
            access_token: "access-token".to_string(),
            account_id: Some("acct_ts_parity".to_string()),
        },
        thinking_level: Some("xhigh".to_string()),
        thinking_budget_tokens: None,
        session_id: None,
        traits: ModelTraits::default(),
    });
    let fixture_request = &fixture["request"];

    let headers = codex_headers(&config.auth).expect("codex headers");
    for name in [
        "accept",
        "chatgpt-account-id",
        "content-type",
        "openai-beta",
        "originator",
    ] {
        assert_eq!(
            headers.get(name).and_then(|value| value.to_str().ok()),
            fixture_request["headers"][name].as_str(),
            "header {name}"
        );
    }
    assert!(headers.get(AUTHORIZATION).is_some());

    let body = openai_responses_body(&config, &request);
    assert_eq!(body, fixture_request["body"]);

    // Without a system prompt, upstream's placeholder stands in.
    for system_prompt in [None, Some(String::new())] {
        let request = ProviderRequest {
            system_prompt,
            ..request.clone()
        };
        assert_eq!(
            openai_responses_body(&config, &request)["instructions"],
            "You are a helpful assistant."
        );
    }
}

#[test]
fn openai_responses_tools_matches_ts_request_shape() {
    let fixture = serde_json::from_str::<Value>(include_str!(
        "../../../tests/fixtures/ts-parity/openai-responses-tools.json"
    ))
    .expect("parse TS parity fixture");
    let request = ProviderRequest {
        max_output_tokens: None,
        system_prompt: Some("pi rust cli".to_string()),
        messages: vec![ChatMessage {
            role: ChatRole::User,
            content: "hello".to_string(),
            media: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
        }],
        tools: vec![ToolDefinition {
            name: "fixture_echo".to_string(),
            description: "Echo text for the parity fixture.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "text": {
                        "type": "string",
                    },
                },
                "required": ["text"],
            }),
        }],
    };
    let config = with_catalog(ProviderConfig {
        model: ModelRef {
            provider: "openai".to_string(),
            id: "gpt-5.4".to_string(),
        },
        api: ProviderApi::OpenAiResponses,
        base_url: None,
        auth: ProviderAuth::ApiKey("sk-ts-parity-token".to_string()),
        thinking_level: Some("xhigh".to_string()),
        thinking_budget_tokens: None,
        session_id: None,
        traits: ModelTraits::default(),
    });
    let body = openai_responses_body(&config, &request);
    assert_eq!(body, fixture["request"]["body"]);
}

#[test]
fn anthropic_tools_matches_ts_request_shape() {
    let fixture = serde_json::from_str::<Value>(include_str!(
        "../../../tests/fixtures/ts-parity/anthropic-tools.json"
    ))
    .expect("parse TS parity fixture");
    let request = ProviderRequest {
        // The TS fixture script passes an explicit `maxTokens: 4096`.
        max_output_tokens: Some(4096),
        system_prompt: Some("pi rust cli".to_string()),
        messages: vec![ChatMessage {
            role: ChatRole::User,
            content: "hello".to_string(),
            media: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
        }],
        tools: vec![fixture_echo_tool()],
    };
    let config = with_catalog(ProviderConfig {
        model: ModelRef {
            provider: "anthropic".to_string(),
            id: "claude-sonnet-4-6".to_string(),
        },
        api: ProviderApi::Anthropic,
        base_url: None,
        auth: ProviderAuth::ApiKey("api-key".to_string()),
        thinking_level: Some("off".to_string()),
        thinking_budget_tokens: None,
        session_id: None,
        traits: ModelTraits::default(),
    });
    let body = anthropic_body(&config, &request);

    assert_eq!(body, fixture["request"]["body"]);
}

#[test]
fn anthropic_body_stays_within_cache_control_budget() {
    fn count_cache_control(value: &Value) -> usize {
        match value {
            Value::Object(map) => {
                let here = usize::from(map.contains_key("cache_control"));
                here + map.values().map(count_cache_control).sum::<usize>()
            }
            Value::Array(items) => items.iter().map(count_cache_control).sum(),
            _ => 0,
        }
    }

    let request = ProviderRequest {
        max_output_tokens: None,
        system_prompt: Some("pi rust cli".to_string()),
        messages: vec![ChatMessage {
            role: ChatRole::User,
            content: "hello".to_string(),
            media: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
        }],
        // More tools than the 4-breakpoint cap so a per-tool breakpoint would overflow.
        tools: vec![
            fixture_echo_tool(),
            fixture_echo_tool(),
            fixture_echo_tool(),
            fixture_echo_tool(),
            fixture_echo_tool(),
            fixture_echo_tool(),
            fixture_echo_tool(),
        ],
    };
    // OAuth path emits two system breakpoints, the most a request can carry.
    let config = ProviderConfig {
        model: ModelRef {
            provider: "anthropic".to_string(),
            id: "claude-opus-4-8".to_string(),
        },
        api: ProviderApi::Anthropic,
        base_url: None,
        auth: ProviderAuth::ClaudeCodeOAuth {
            access_token: "token".to_string(),
        },
        thinking_level: Some("xhigh".to_string()),
        thinking_budget_tokens: None,
        session_id: None,
        traits: ModelTraits::default(),
    };
    let body = anthropic_body(&config, &request);

    assert!(
        count_cache_control(&body) <= 4,
        "expected <= 4 cache_control breakpoints, found {} in {body}",
        count_cache_control(&body)
    );

    let tools = body["tools"].as_array().expect("tools array");
    for tool in &tools[..tools.len() - 1] {
        assert!(
            tool.get("cache_control").is_none(),
            "only the last tool should carry cache_control"
        );
    }
    assert!(
        tools.last().unwrap().get("cache_control").is_some(),
        "last tool should carry cache_control"
    );
}

#[test]
fn google_tools_matches_ts_request_shape() {
    let fixture = serde_json::from_str::<Value>(include_str!(
        "../../../tests/fixtures/ts-parity/google-tools.json"
    ))
    .expect("parse TS parity fixture");
    let request = ProviderRequest {
        max_output_tokens: None,
        system_prompt: Some("pi rust cli".to_string()),
        messages: vec![ChatMessage {
            role: ChatRole::User,
            content: "hello".to_string(),
            media: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
        }],
        tools: vec![fixture_echo_tool()],
    };
    let config = with_catalog(ProviderConfig {
        model: ModelRef {
            provider: "google".to_string(),
            id: "gemini-2.5-pro".to_string(),
        },
        api: ProviderApi::Google,
        base_url: Some("https://generativelanguage.googleapis.com/v1beta".to_string()),
        auth: ProviderAuth::ApiKey("google-key".to_string()),
        thinking_level: Some("high".to_string()),
        thinking_budget_tokens: None,
        session_id: None,
        traits: ModelTraits::default(),
    });

    assert_eq!(google_body(&config, &request), fixture["request"]["body"]);
}

#[test]
fn mistral_tools_matches_ts_request_shape() {
    let fixture = serde_json::from_str::<Value>(include_str!(
        "../../../tests/fixtures/ts-parity/mistral-tools.json"
    ))
    .expect("parse TS parity fixture");
    let request = ProviderRequest {
        max_output_tokens: None,
        system_prompt: Some("pi rust cli".to_string()),
        messages: vec![ChatMessage {
            role: ChatRole::User,
            content: "hello".to_string(),
            media: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
        }],
        tools: vec![fixture_echo_tool()],
    };
    let config = with_catalog(ProviderConfig {
        model: ModelRef {
            provider: "mistral".to_string(),
            id: "devstral-medium-latest".to_string(),
        },
        api: ProviderApi::Mistral,
        base_url: Some("https://api.mistral.ai/v1".to_string()),
        auth: ProviderAuth::ApiKey("mistral-key".to_string()),
        thinking_level: Some("high".to_string()),
        thinking_budget_tokens: None,
        session_id: Some("session_ts_parity".to_string()),
        traits: ModelTraits::default(),
    });

    assert_eq!(
        mistral_chat_body(&config, &request),
        fixture["request"]["body"]
    );
}

#[test]
fn bedrock_tools_matches_ts_payload_shape() {
    let fixture = serde_json::from_str::<Value>(include_str!(
        "../../../tests/fixtures/ts-parity/bedrock-tools.json"
    ))
    .expect("parse TS parity fixture");
    let request = ProviderRequest {
        max_output_tokens: None,
        system_prompt: Some("pi rust cli".to_string()),
        messages: vec![ChatMessage {
            role: ChatRole::User,
            content: "hello".to_string(),
            media: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
        }],
        tools: vec![fixture_echo_tool()],
    };
    let config = with_catalog(ProviderConfig {
        model: ModelRef {
            provider: "amazon-bedrock".to_string(),
            id: "us.anthropic.claude-opus-4-6-v1".to_string(),
        },
        api: ProviderApi::Bedrock,
        base_url: Some("https://bedrock-runtime.us-east-1.amazonaws.com".to_string()),
        auth: ProviderAuth::ApiKey("bedrock-key".to_string()),
        thinking_level: Some("xhigh".to_string()),
        thinking_budget_tokens: None,
        session_id: None,
        traits: ModelTraits::default(),
    });

    assert_eq!(bedrock_body(&config, &request), fixture["payload"]);
}

fn fixture_echo_tool() -> ToolDefinition {
    ToolDefinition {
        name: "fixture_echo".to_string(),
        description: "Echo text for the parity fixture.".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "text": {
                    "type": "string",
                },
            },
            "required": ["text"],
        }),
    }
}

#[test]
fn openrouter_kimi_matches_ts_request_shape() {
    let fixture = serde_json::from_str::<Value>(include_str!(
        "../../../tests/fixtures/ts-parity/openrouter-kimi.json"
    ))
    .expect("parse TS parity fixture");
    let request = ProviderRequest {
        max_output_tokens: None,
        system_prompt: Some("pi rust cli".to_string()),
        messages: vec![ChatMessage {
            role: ChatRole::User,
            content: "hello".to_string(),
            media: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
        }],
        tools: Vec::new(),
    };
    let config = with_catalog(ProviderConfig {
        model: ModelRef {
            provider: "openrouter".to_string(),
            id: "moonshotai/kimi-k2.6".to_string(),
        },
        api: ProviderApi::OpenAi,
        base_url: Some("https://openrouter.ai/api/v1".to_string()),
        auth: ProviderAuth::ApiKey("openrouter-key".to_string()),
        thinking_level: Some("xhigh".to_string()),
        thinking_budget_tokens: None,
        session_id: None,
        traits: ModelTraits::default(),
    });
    let fixture_request = &fixture["request"];

    let headers = openai_compatible_headers(&config, "openrouter-key", false).expect("headers");
    for name in ["accept", "content-type"] {
        assert_eq!(
            headers.get(name).and_then(|value| value.to_str().ok()),
            fixture_request["headers"][name].as_str(),
            "header {name}"
        );
    }
    assert_eq!(
        headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .map(|value| value.starts_with("Bearer ")),
        Some(true)
    );

    let body = openai_chat_completions_body(&config, &request);
    assert_eq!(body, fixture_request["body"]);
}

#[test]
fn github_copilot_matches_ts_responses_request_shape() {
    let fixture = serde_json::from_str::<Value>(include_str!(
        "../../../tests/fixtures/ts-parity/github-copilot-gpt-6.1-sol.json"
    ))
    .expect("parse TS parity fixture");
    let request = ProviderRequest {
        max_output_tokens: None,
        system_prompt: Some("pi rust cli".to_string()),
        messages: vec![ChatMessage {
            role: ChatRole::User,
            content: "hello".to_string(),
            media: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
        }],
        tools: Vec::new(),
    };
    let config = with_catalog(ProviderConfig {
        model: ModelRef {
            provider: "github-copilot".to_string(),
            id: "gpt-6.1-sol".to_string(),
        },
        api: ProviderApi::OpenAiResponses,
        base_url: Some("https://api.individual.githubcopilot.com".to_string()),
        auth: ProviderAuth::ApiKey("copilot-key".to_string()),
        thinking_level: Some("xhigh".to_string()),
        thinking_budget_tokens: None,
        session_id: None,
        traits: ModelTraits::default(),
    });
    let fixture_request = &fixture["request"];
    let provider = OpenAiResponsesProvider {
        config: config.clone(),
        endpoint: OpenAiResponsesEndpoint::OpenAi,
        client: reqwest::Client::new(),
    };
    let (url, headers, _) = provider.request_parts(false).expect("request parts");

    assert_eq!(url, fixture_request["url"].as_str().expect("fixture url"));
    for name in [
        "accept",
        "content-type",
        "copilot-integration-id",
        "editor-plugin-version",
        "editor-version",
        "openai-intent",
        "user-agent",
        "x-initiator",
    ] {
        assert_eq!(
            headers.get(name).and_then(|value| value.to_str().ok()),
            fixture_request["headers"][name].as_str(),
            "header {name}"
        );
    }
    assert!(headers.get(AUTHORIZATION).is_some());

    let body = openai_responses_body(&config, &request);
    assert_eq!(body, fixture_request["body"]);
}

#[test]
fn google_gemini_matches_ts_request_shape() {
    let fixture = serde_json::from_str::<Value>(include_str!(
        "../../../tests/fixtures/ts-parity/google-gemini-2.5-pro.json"
    ))
    .expect("parse TS parity fixture");
    let request = ProviderRequest {
        max_output_tokens: None,
        system_prompt: Some("pi rust cli".to_string()),
        messages: vec![ChatMessage {
            role: ChatRole::User,
            content: "hello".to_string(),
            media: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
        }],
        tools: Vec::new(),
    };
    let config = with_catalog(ProviderConfig {
        model: ModelRef {
            provider: "google".to_string(),
            id: "gemini-2.5-pro".to_string(),
        },
        api: ProviderApi::Google,
        base_url: Some("https://generativelanguage.googleapis.com/v1beta".to_string()),
        auth: ProviderAuth::ApiKey("google-key".to_string()),
        thinking_level: Some("high".to_string()),
        thinking_budget_tokens: None,
        session_id: None,
        traits: ModelTraits::default(),
    });
    let fixture_request = &fixture["request"];
    let headers = google_headers("google-key").expect("headers");

    assert_eq!(
        google_url(&config).expect("url"),
        fixture_request["url"].as_str().expect("fixture url")
    );
    assert_eq!(
        headers
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        fixture_request["headers"]["content-type"].as_str()
    );
    assert!(headers.get("x-goog-api-key").is_some());

    assert_eq!(google_body(&config, &request), fixture_request["body"]);
}

#[test]
fn bedrock_claude_opus_matches_ts_payload_shape() {
    let fixture = serde_json::from_str::<Value>(include_str!(
        "../../../tests/fixtures/ts-parity/bedrock-claude-opus-4.6.json"
    ))
    .expect("parse TS parity fixture");
    let request = ProviderRequest {
        max_output_tokens: None,
        system_prompt: Some("pi rust cli".to_string()),
        messages: vec![ChatMessage {
            role: ChatRole::User,
            content: "hello".to_string(),
            media: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
        }],
        tools: Vec::new(),
    };
    let config = with_catalog(ProviderConfig {
        model: ModelRef {
            provider: "amazon-bedrock".to_string(),
            id: "us.anthropic.claude-opus-4-6-v1".to_string(),
        },
        api: ProviderApi::Bedrock,
        base_url: Some("https://bedrock-runtime.us-east-1.amazonaws.com".to_string()),
        auth: ProviderAuth::ApiKey("bedrock-key".to_string()),
        thinking_level: Some("xhigh".to_string()),
        thinking_budget_tokens: None,
        session_id: None,
        traits: ModelTraits::default(),
    });

    assert_eq!(bedrock_body(&config, &request), fixture["payload"]);
}

#[test]
fn cloudflare_ai_gateway_matches_ts_request_shape() {
    // Test setup: the environment is process-wide, and writing it is unsafe since Rust 2024 while another thread may read it.
    unsafe { std::env::set_var("CLOUDFLARE_ACCOUNT_ID", "acct_ts_parity") };
    // Test setup: the environment is process-wide, and writing it is unsafe since Rust 2024 while another thread may read it.
    unsafe { std::env::set_var("CLOUDFLARE_GATEWAY_ID", "gateway_ts_parity") };
    let fixture = serde_json::from_str::<Value>(include_str!(
        "../../../tests/fixtures/ts-parity/cloudflare-ai-gateway-kimi.json"
    ))
    .expect("parse TS parity fixture");
    let request = ProviderRequest {
        max_output_tokens: None,
        system_prompt: Some("pi rust cli".to_string()),
        messages: vec![ChatMessage {
            role: ChatRole::User,
            content: "hello".to_string(),
            media: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
        }],
        tools: Vec::new(),
    };
    let config = with_catalog(ProviderConfig {
        model: ModelRef {
            provider: "cloudflare-ai-gateway".to_string(),
            id: "workers-ai/@cf/moonshotai/kimi-k2.6".to_string(),
        },
        api: ProviderApi::OpenAi,
        base_url: Some(
            "https://gateway.ai.cloudflare.com/v1/{CLOUDFLARE_ACCOUNT_ID}/{CLOUDFLARE_GATEWAY_ID}/compat"
                .to_string(),
        ),
        auth: ProviderAuth::ApiKey("cloudflare-key".to_string()),
        thinking_level: Some("xhigh".to_string()),
        thinking_budget_tokens: None,
        session_id: Some("session_ts_parity".to_string()),
        traits: ModelTraits::default(),
    });
    let fixture_request = &fixture["request"];
    let provider = OpenAiProvider {
        config: config.clone(),
        client: reqwest::Client::new(),
    };

    assert_eq!(
        format!(
            "{}/chat/completions",
            provider.chat_base_url().expect("base url")
        ),
        fixture_request["url"].as_str().expect("fixture url")
    );
    let headers = openai_compatible_headers(&config, "cloudflare-key", false).expect("headers");
    for name in [
        "accept",
        "cf-aig-authorization",
        "content-type",
        "session_id",
        "x-client-request-id",
        "x-session-affinity",
    ] {
        if name == "cf-aig-authorization" {
            assert_eq!(
                headers
                    .get(name)
                    .and_then(|value| value.to_str().ok())
                    .map(|value| value.starts_with("Bearer ")),
                Some(true)
            );
        } else {
            assert_eq!(
                headers.get(name).and_then(|value| value.to_str().ok()),
                fixture_request["headers"][name].as_str(),
                "header {name}"
            );
        }
    }
    assert!(headers.get(AUTHORIZATION).is_none());

    let body = openai_chat_completions_body(&config, &request);
    assert_eq!(body, fixture_request["body"]);
}

#[test]
fn mistral_devstral_matches_ts_request_shape() {
    let fixture = serde_json::from_str::<Value>(include_str!(
        "../../../tests/fixtures/ts-parity/mistral-devstral.json"
    ))
    .expect("parse TS parity fixture");
    let request = ProviderRequest {
        max_output_tokens: None,
        system_prompt: Some("pi rust cli".to_string()),
        messages: vec![ChatMessage {
            role: ChatRole::User,
            content: "hello".to_string(),
            media: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
        }],
        tools: Vec::new(),
    };
    let config = with_catalog(ProviderConfig {
        model: ModelRef {
            provider: "mistral".to_string(),
            id: "devstral-medium-latest".to_string(),
        },
        api: ProviderApi::Mistral,
        base_url: Some("https://api.mistral.ai/v1".to_string()),
        auth: ProviderAuth::ApiKey("mistral-key".to_string()),
        thinking_level: Some("high".to_string()),
        thinking_budget_tokens: None,
        session_id: Some("session_ts_parity".to_string()),
        traits: ModelTraits::default(),
    });
    let fixture_request = &fixture["request"];
    let headers = mistral_headers(&config, "mistral-key").expect("headers");

    for name in ["accept", "content-type", "x-affinity"] {
        assert_eq!(
            headers.get(name).and_then(|value| value.to_str().ok()),
            fixture_request["headers"][name].as_str(),
            "header {name}"
        );
    }
    // The fixture was captured on Linux x86_64 with the kernel release
    // pinned to `<kernel>`; Rust sends the same identity for its own host.
    assert_eq!(
        headers
            .get(USER_AGENT)
            .and_then(|value| value.to_str().ok()),
        Some(pi_user_agent())
    );
    assert_eq!(
        Some(format_pi_user_agent(node_platform("linux"), node_arch("x86_64")).as_str()),
        fixture_request["headers"]["user-agent"].as_str()
    );
    assert!(headers.get(AUTHORIZATION).is_some());

    let body = mistral_chat_body(&config, &request);
    assert_eq!(body, fixture_request["body"]);
}

#[test]
fn codex_headers_accept_login_tokens_and_api_keys() {
    let login_headers = codex_headers(&ProviderAuth::ChatGptOAuth {
        access_token: "access-token".to_string(),
        account_id: Some("account-id".to_string()),
    })
    .expect("login headers");
    assert_eq!(
        login_headers
            .get("ChatGPT-Account-ID")
            .and_then(|value| value.to_str().ok()),
        Some("account-id")
    );

    let api_key_headers =
        codex_headers(&ProviderAuth::ApiKey("api-key".to_string())).expect("api key headers");
    assert_eq!(
        api_key_headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok()),
        Some("Bearer api-key")
    );
    assert!(api_key_headers.get("ChatGPT-Account-ID").is_none());
}

#[test]
fn codex_headers_extract_account_id_from_chatgpt_jwt() {
    let token = concat!(
        "eyJhbGciOiJub25lIn0.",
        "eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjdF9mcm9tX2p3dCJ9fQ.",
        "signature"
    );
    let headers = codex_headers(&ProviderAuth::ChatGptOAuth {
        access_token: token.to_string(),
        account_id: None,
    })
    .expect("codex headers");

    assert_eq!(
        headers
            .get("ChatGPT-Account-ID")
            .and_then(|value| value.to_str().ok()),
        Some("acct_from_jwt")
    );
}

fn image_tool_result(content: &str) -> ChatMessage {
    ChatMessage {
        role: ChatRole::Tool,
        content: content.to_string(),
        media: vec![MediaInput {
            mime_type: "image/png".to_string(),
            data_base64: "aW1hZ2U=".to_string(),
            path: None,
            width: None,
            height: None,
        }],
        tool_call_id: Some("call_1".to_string()),
        tool_name: Some("read".to_string()),
        tool_calls: Vec::new(),
    }
}

#[test]
fn anthropic_tool_results_carry_images_as_content_blocks() {
    let messages = anthropic_messages(&[
        image_tool_result("Read image file [image/png]"),
        image_tool_result(""),
    ]);
    assert_eq!(
        messages[0]["content"][0],
        json!({
            "type": "tool_result",
            "tool_use_id": "call_1",
            "content": [
                { "type": "text", "text": "Read image file [image/png]" },
                {
                    "type": "image",
                    "source": { "type": "base64", "media_type": "image/png", "data": "aW1hZ2U=" },
                },
            ],
        })
    );
    // An image alone still needs a text block.
    assert_eq!(
        messages[1]["content"][0]["content"][0],
        json!({ "type": "text", "text": "(see attached image)" })
    );
}

#[test]
fn bedrock_tool_results_carry_images_as_content_blocks() {
    let messages = bedrock_messages(&[image_tool_result("Read image file [image/png]")]);
    assert_eq!(
        messages[0]["content"][0]["toolResult"]["content"],
        json!([
            { "text": "Read image file [image/png]" },
            { "image": { "format": "png", "source": { "bytes": "aW1hZ2U=" } } },
        ])
    );
}

fn chat_completions_config(api: ProviderApi, accepts_images: Option<bool>) -> ProviderConfig {
    ProviderConfig {
        model: ModelRef {
            provider: "local".to_string(),
            id: "test".to_string(),
        },
        api,
        base_url: None,
        auth: ProviderAuth::None,
        thinking_level: None,
        thinking_budget_tokens: None,
        session_id: None,
        traits: ModelTraits {
            input_modalities: accepts_images.map(|accepts| {
                if accepts {
                    vec!["text".to_string(), "image".to_string()]
                } else {
                    vec!["text".to_string()]
                }
            }),
            ..ModelTraits::default()
        },
    }
}

#[test]
fn chat_completions_tool_results_say_their_images_were_left_out() {
    let request = ProviderRequest {
        max_output_tokens: None,
        system_prompt: None,
        messages: vec![image_tool_result("Read image file [image/png]")],
        tools: Vec::new(),
    };
    for api in [ProviderApi::OpenAi, ProviderApi::Mistral] {
        // Unknown modalities: the API decides, and a tool message is text only.
        let config = chat_completions_config(api.clone(), None);
        let messages = openai_chat_messages(&config, &request);
        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0]["content"],
            "Read image file [image/png]\n(tool image omitted: this provider API does not accept images in tool results)"
        );

        // The catalog says the model takes no images.
        let config = chat_completions_config(api, Some(false));
        let messages = openai_chat_messages(&config, &request);
        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0]["content"],
            "Read image file [image/png]\n(tool image omitted: the current model does not support image input)"
        );
    }
}

#[test]
fn chat_completions_send_tool_result_images_in_a_following_user_message() {
    let request = ProviderRequest {
        max_output_tokens: None,
        system_prompt: None,
        messages: vec![
            ChatMessage {
                role: ChatRole::User,
                content: "read the screenshot".to_string(),
                media: Vec::new(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: Vec::new(),
            },
            ChatMessage {
                role: ChatRole::Assistant,
                content: String::new(),
                media: Vec::new(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: vec![ChatToolCall {
                    id: "call_1".to_string(),
                    name: "read".to_string(),
                    arguments: "{}".to_string(),
                }],
            },
            image_tool_result("Read image file [image/png]"),
            image_tool_result("Read image file [image/jpeg]"),
        ],
        tools: Vec::new(),
    };
    let config = chat_completions_config(ProviderApi::OpenAi, Some(true));
    let messages = openai_chat_messages(&config, &request);
    // The two tool results keep their text, and one user message with
    // both results' images follows the tool round.
    assert_eq!(messages.len(), 5);
    assert_eq!(messages[3]["role"], "tool");
    assert_eq!(messages[4]["role"], "user");
    let parts = messages[4]["content"].as_array().unwrap();
    assert_eq!(parts.len(), 4, "two labels and two images");
    assert!(parts[1]["image_url"]["url"]
        .as_str()
        .unwrap()
        .starts_with("data:image/png;base64,"));
    assert!(parts[3]["image_url"]["url"]
        .as_str()
        .unwrap()
        .starts_with("data:image/png;base64,"));
    // The tool messages themselves say where the images went.
    assert_eq!(
        messages[3]["content"],
        "Read image file [image/jpeg]\n(tool image omitted: this provider API does not accept images in tool results)"
    );
}

#[test]
fn chat_completions_replace_pasted_images_with_a_note_for_text_only_models() {
    let request = ProviderRequest {
        max_output_tokens: None,
        system_prompt: None,
        messages: vec![ChatMessage {
            role: ChatRole::User,
            content: "what is this?".to_string(),
            media: vec![MediaInput {
                mime_type: "image/png".to_string(),
                data_base64: "aW1hZ2U=".to_string(),
                path: None,
                width: None,
                height: None,
            }],
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
        }],
        tools: Vec::new(),
    };
    for (modalities, sends_image) in [(Some(true), true), (Some(false), false), (None, true)] {
        let config = chat_completions_config(ProviderApi::OpenAi, modalities);
        let messages = openai_chat_messages(&config, &request);
        let parts = messages[0]["content"].as_array().unwrap();
        let has_image = parts
            .iter()
            .any(|part| part.get("type") == Some(&json!("image_url")));
        assert_eq!(has_image, sends_image, "modalities {modalities:?}");
        if !sends_image {
            assert!(parts.iter().any(|part| part["text"]
                .as_str()
                .unwrap_or_default()
                .contains("does not support image input")));
        }
    }
}

#[test]
fn bedrock_messages_include_image_payloads() {
    let messages = bedrock_messages(&[ChatMessage {
        role: ChatRole::User,
        content: "describe".to_string(),
        media: vec![MediaInput {
            mime_type: "image/png".to_string(),
            data_base64: "aW1hZ2U=".to_string(),
            path: None,
            width: None,
            height: None,
        }],
        tool_call_id: None,
        tool_name: None,
        tool_calls: Vec::new(),
    }]);

    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[0]["content"][1]["image"]["format"], "png");
    assert_eq!(
        messages[0]["content"][1]["image"]["source"]["bytes"],
        "aW1hZ2U="
    );
}

/// Serves every request with `body` as an event stream, then closes the
/// connection: a stream that ends when the body ends.
fn spawn_sse_server(body: &'static str) -> String {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind stub");
    let url = format!("http://{}", listener.local_addr().expect("stub addr"));
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut request = Vec::new();
            let mut buffer = [0u8; 4096];
            while let Ok(read) = stream.read(&mut buffer) {
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                let text = String::from_utf8_lossy(&request);
                if let Some((headers, rest)) = text.split_once("\r\n\r\n") {
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|value| value.trim().parse::<usize>().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    if rest.len() >= length {
                        break;
                    }
                }
            }
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{body}"
                )
                .as_bytes(),
            );
        }
    });
    url
}

/// #135: every wire format ends a reply the way its provider ends it. A
/// stream cut short or failed mid-way is an error (retryable when it was
/// cut short), never a clean stop, and the streaming and batch paths agree
/// on events and on the one `Stop`, which comes last.
#[tokio::test]
async fn streams_end_with_their_provider_terminal_or_an_error() {
    const ANTHROPIC_START: &str = "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m\"}}\n\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"partial\"}}\n\n";
    let cases: Vec<(ProviderApi, &'static str, Result<&str, &str>)> = vec![
        (
            ProviderApi::Anthropic,
            // The verified case: an error event mid-stream.
            concat!(
                "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m\"}}\n\n",
                "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"partial\"}}\n\n",
                "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n",
            ),
            Err("overloaded_error"),
        ),
        (ProviderApi::Anthropic, ANTHROPIC_START, Err("ended before message_stop")),
        (
            ProviderApi::Anthropic,
            concat!(
                "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m\"}}\n\n",
                "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"partial\"}}\n\n",
                "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"max_tokens\"}}\n\n",
                "data: {\"type\":\"message_stop\"}\n\n",
            ),
            Ok("max_tokens"),
        ),
        (
            ProviderApi::OpenAi,
            "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\ndata: [DONE]\n\n",
            Err("without finish_reason"),
        ),
        (
            ProviderApi::OpenAi,
            "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"},\"finish_reason\":\"length\"}]}\n\ndata: [DONE]\n\n",
            Ok("length"),
        ),
        (
            ProviderApi::OpenAi,
            "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"},\"finish_reason\":\"content_filter\"}]}\n\n",
            Err("content_filter"),
        ),
        (
            ProviderApi::OpenAiResponses,
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n",
            Err("ended before a terminal response event"),
        ),
        (
            ProviderApi::OpenAiResponses,
            // Text arrives as deltas and again in the finished item: once.
            concat!(
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n",
                "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"partial\"}]}}\n\n",
                "data: {\"type\":\"response.incomplete\",\"response\":{\"status\":\"incomplete\",\"incomplete_details\":{\"reason\":\"max_output_tokens\"}}}\n\n",
            ),
            Ok("incomplete.max_output_tokens"),
        ),
        (
            ProviderApi::Google,
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"partial\"}]},\"finishReason\":\"MAX_TOKENS\"}]}\n\n",
            Ok("MAX_TOKENS"),
        ),
        (
            ProviderApi::Google,
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"partial\"}]}}]}\n\n",
            Err("without a finish reason"),
        ),
        (
            ProviderApi::Google,
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"partial\"}]},\"finishReason\":\"SAFETY\"}]}\n\n",
            Err("Provider stopped with: SAFETY"),
        ),
    ];
    for (api, body, expected) in cases {
        let provider = create_provider_with_client(
            ProviderConfig {
                model: ModelRef {
                    provider: "local".to_string(),
                    id: "test".to_string(),
                },
                api: api.clone(),
                base_url: Some(spawn_sse_server(body)),
                auth: ProviderAuth::ApiKey("test-key".to_string()),
                thinking_level: None,
                thinking_budget_tokens: None,
                session_id: None,
                traits: ModelTraits::default(),
            },
            provider_http_client(),
        );
        let mut streamed = Vec::new();
        let streaming = provider
            .complete_streaming(uncapped_request("system"), &mut |event| {
                streamed.push(event);
                Ok(())
            })
            .await;
        let batch = provider.complete(uncapped_request("system")).await;
        let case = format!("{api:?} {body}");
        match expected {
            Ok(reason) => {
                let batch = batch.unwrap_or_else(|error| panic!("{case}: {error}"));
                streaming.unwrap_or_else(|error| panic!("{case}: {error}"));
                assert_eq!(streamed, batch, "{case}");
                let stops: Vec<_> = batch
                    .iter()
                    .filter(|event| matches!(event, StreamEvent::Stop { .. }))
                    .collect();
                assert_eq!(
                    stops,
                    [&StreamEvent::Stop {
                        reason: reason.into()
                    }],
                    "{case}"
                );
                assert!(
                    matches!(batch.last(), Some(StreamEvent::Stop { .. })),
                    "{case}"
                );
                let texts = batch
                    .iter()
                    .filter(|event| matches!(event, StreamEvent::Text(_)))
                    .count();
                assert_eq!(texts, 1, "{case}");
                assert!(is_output_limit_stop(reason) || reason == "stop", "{case}");
            }
            Err(message) => {
                for error in [streaming.unwrap_err(), batch.unwrap_err()] {
                    assert!(error.to_string().contains(message), "{case}: {error}");
                    if message.contains("ended") || message.contains("overloaded") {
                        assert!(error.is_retryable(), "{case}: {error}");
                    }
                }
                assert!(
                    !streamed
                        .iter()
                        .any(|event| matches!(event, StreamEvent::Stop { .. })),
                    "{case}"
                );
            }
        }
    }
}
