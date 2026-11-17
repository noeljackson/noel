//! Integration tests for the streamable-HTTP transport, against a
//! hand-rolled HTTP/1.1 server on a loopback socket (the workspace's
//! pattern for local test servers): initialize with a session id,
//! tools/list as JSON, tools/call as an SSE stream, the GET stream with
//! 405, and a 401 surfacing `AuthRequired`.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use noel_mcp::http::{HttpOptions, HttpTransport};

/// What the test server answers each request with.
enum Answer {
    Json(String),
    Sse(Vec<String>),
    #[allow(dead_code)]
    Status(u16),
}

/// One served request, for assertions.
struct Served {
    method: String,
    path: String,
    headers: String,
    body: String,
}

fn read_request(stream: &mut TcpStream) -> Served {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
        // The peer closed; an empty served request ends the keep-alive
        // loop, whose break then returns to accept.
        return Served {
            method: String::new(),
            path: String::new(),
            headers: String::new(),
            body: String::new(),
        };
    }
    let mut headers = String::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let ended = line.trim().is_empty();
        headers.push_str(&line);
        if ended {
            break;
        }
    }
    let length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            (name.trim().eq_ignore_ascii_case("content-length"))
                .then(|| value.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    if length > 0 {
        reader.read_exact(&mut body).unwrap();
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();
    Served {
        method,
        path,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    }
}

fn write_response(stream: &mut TcpStream, status: u16, headers: &str, body: &str) {
    let reason = match status {
        200 => "OK",
        202 => "Accepted",
        204 => "No Content",
        401 => "Unauthorized",
        405 => "Method Not Allowed",
        _ => "Internal Server Error",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-length: {}\r\n{headers}\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).unwrap();
    stream.flush().unwrap();
}

fn sse_response(events: &[String]) -> (String, String) {
    let body = events
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect::<String>();
    ("content-type: text/event-stream\r\n".to_string(), body)
}

#[test]
fn http_transport_initializes_lists_and_calls() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let served = Arc::new(std::sync::Mutex::new(Vec::<Served>::new()));

    let served_thread = Arc::clone(&served);
    std::thread::spawn(move || {
        let session = Arc::new(AtomicUsize::new(0));
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            // HTTP/1.1 keep-alive: serve requests until the peer closes.
            loop {
                let request = read_request(&mut stream);
                if request.method.is_empty() {
                    break;
                }
                served_thread.lock().unwrap().push(Served {
                    method: request.method.clone(),
                    path: request.path.clone(),
                    headers: request.headers.clone(),
                    body: request.body.clone(),
                });
                let body = request.body.clone();
                let method = request.method.clone();
                let jsonrpc = serde_json::from_str::<serde_json::Value>(&body)
                    .ok()
                    .and_then(|value| {
                        value
                            .get("method")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_string)
                    })
                    .unwrap_or_default();
                let id = serde_json::from_str::<serde_json::Value>(&body)
                    .ok()
                    .and_then(|value| value.get("id").cloned())
                    .unwrap_or(serde_json::json!(0));
                if method == "GET" {
                    // The GET stream: this server offers none.
                    write_response(&mut stream, 405, "", "");
                    continue;
                }
                if method == "DELETE" {
                    write_response(&mut stream, 204, "", "");
                    continue;
                }
                session.fetch_add(1, Ordering::SeqCst);
                let answer = match jsonrpc.as_str() {
                "initialize" => Answer::Json(format!(
                    "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{{\"protocolVersion\":\"2025-06-18\",\"capabilities\":{{\"tools\":{{}}}},\"serverInfo\":{{\"name\":\"http-test\",\"version\":\"1.0\"}}}}}}"
                )),
                "tools/list" => Answer::Json(format!(
                    "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{{\"tools\":[{{\"name\":\"echo\",\"inputSchema\":{{\"type\":\"object\"}}}}]}}}}"
                )),
                "tools/call" => {
                    let events = vec![format!(
                        "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{{\"content\":[{{\"type\":\"text\",\"text\":\"sse-pong\"}}]}}}}"
                    )];
                    Answer::Sse(events)
                }
                _ => Answer::Json(format!(
                    "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{{}}}}"
                )),
            };
                let session_header = "mcp-session-id: test-session\r\n";
                match answer {
                    Answer::Json(body) => write_response(
                        &mut stream,
                        200,
                        &format!("content-type: application/json\r\n{session_header}"),
                        &body,
                    ),
                    Answer::Sse(events) => {
                        let (headers, body) = sse_response(&events);
                        write_response(
                            &mut stream,
                            200,
                            &format!("{headers}{session_header}"),
                            &body,
                        )
                    }
                    Answer::Status(status) => write_response(&mut stream, status, "", ""),
                }
            }
        }
    });

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async move {
        let mut transport = HttpTransport::new(HttpOptions {
            url: format!("http://127.0.0.1:{port}/mcp"),
            ..Default::default()
        });
        // initialize
        let initialize = noel_mcp::protocol::jsonrpc::JsonRpcMessage::Request(
            noel_mcp::protocol::jsonrpc::JsonRpcRequest {
                jsonrpc: "2.0".to_string(),
                id: noel_mcp::protocol::jsonrpc::JsonRpcId::Number(1),
                method: "initialize".to_string(),
                params: Some(serde_json::json!({
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": {"name": "noel", "version": "test"}
                })),
            },
        );
        let answer = transport
            .send(&initialize)
            .await
            .unwrap()
            .expect("answered");
        let version = match &answer {
            noel_mcp::protocol::jsonrpc::JsonRpcMessage::Request(_) => panic!("answer shape"),
            _ => "2025-06-18",
        };
        transport.set_protocol_version(version);
        // tools/list over JSON
        let list = noel_mcp::protocol::jsonrpc::JsonRpcMessage::Request(
            noel_mcp::protocol::jsonrpc::JsonRpcRequest {
                jsonrpc: "2.0".to_string(),
                id: noel_mcp::protocol::jsonrpc::JsonRpcId::Number(2),
                method: "tools/list".to_string(),
                params: None,
            },
        );
        let answer = transport.send(&list).await.unwrap().expect("answered");
        let text = serde_json::to_string(&answer).unwrap();
        assert!(text.contains("echo"), "{text}");
        // tools/call answered over SSE
        let call = noel_mcp::protocol::jsonrpc::JsonRpcMessage::Request(
            noel_mcp::protocol::jsonrpc::JsonRpcRequest {
                jsonrpc: "2.0".to_string(),
                id: noel_mcp::protocol::jsonrpc::JsonRpcId::Number(3),
                method: "tools/call".to_string(),
                params: Some(serde_json::json!({"name": "echo"})),
            },
        );
        let answer = match transport.send(&call).await {
            Ok(Some(answer)) => answer,
            other => panic!("call answer: {other:?}"),
        };
        let text = serde_json::to_string(&answer).unwrap();
        assert!(text.contains("sse-pong"), "{text}");
        transport.close().await;

        // The server saw the session header after the first answer.
        let served = served.lock().unwrap();
        assert!(served
            .iter()
            .any(|request| request.body.contains("tools/list")
                && request
                    .headers
                    .to_lowercase()
                    .contains("mcp-session-id: test-session")));
        // And the DELETE on close.
        assert!(served.iter().any(|request| request.method == "DELETE"));
    });
}

#[test]
fn a_401_surfaces_auth_required() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let _ = read_request(&mut stream);
            write_response(&mut stream, 401, "", "{\"error\":\"no token\"}");
        }
    });
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async move {
        let mut transport = HttpTransport::new(HttpOptions {
            url: format!("http://127.0.0.1:{port}/mcp"),
            ..Default::default()
        });
        let request = noel_mcp::protocol::jsonrpc::JsonRpcMessage::Request(
            noel_mcp::protocol::jsonrpc::JsonRpcRequest {
                jsonrpc: "2.0".to_string(),
                id: noel_mcp::protocol::jsonrpc::JsonRpcId::Number(1),
                method: "initialize".to_string(),
                params: None,
            },
        );
        match transport.send(&request).await {
            Err(noel_mcp::client::McpError::AuthRequired { body }) => {
                assert!(body.contains("no token"), "{body}");
            }
            other => panic!("expected AuthRequired, got {other:?}"),
        }
        transport.close().await;
    });
}

#[test]
fn a_token_in_options_is_sent_on_the_handshake() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let saw_token = Arc::new(std::sync::Mutex::new(false));
    let saw = Arc::clone(&saw_token);
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let request = read_request(&mut stream);
            if request
                .headers
                .to_lowercase()
                .contains("authorization: bearer tok")
            {
                *saw.lock().unwrap() = true;
            }
            write_response(
                &mut stream,
                200,
                "content-type: application/json\r\n",
                "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}",
            );
        }
    });
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async move {
        let mut transport = HttpTransport::new(HttpOptions {
            url: format!("http://127.0.0.1:{port}/mcp"),
            token: Some("tok".to_string()),
            ..Default::default()
        });
        let request = noel_mcp::protocol::jsonrpc::JsonRpcMessage::Request(
            noel_mcp::protocol::jsonrpc::JsonRpcRequest {
                jsonrpc: "2.0".to_string(),
                id: noel_mcp::protocol::jsonrpc::JsonRpcId::Number(1),
                method: "initialize".to_string(),
                params: None,
            },
        );
        let answer = transport.send(&request).await.unwrap().expect("answered");
        let _ = serde_json::to_string(&answer).unwrap();
        transport.close().await;
    });
    assert!(
        *saw_token.lock().unwrap(),
        "the Authorization header reached the server"
    );
}
