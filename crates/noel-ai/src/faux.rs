//! The faux provider: scripted replies for tests and offline use.

#[allow(unused_imports)]
use super::*;

pub(crate) struct FauxProvider {
    pub(crate) config: ProviderConfig,
}

impl FauxProvider {
    pub(crate) fn is_script(&self) -> bool {
        self.config.model.id == "script"
    }
}

#[async_trait]
impl Provider for FauxProvider {
    async fn complete(&self, request: ProviderRequest) -> Result<Vec<StreamEvent>, ProviderError> {
        if self.is_script() {
            let mut events = Vec::new();
            FauxScript::from_env()?
                .respond(&request, &mut |event| {
                    events.push(event);
                    Ok(())
                })
                .await?;
            return Ok(events);
        }
        let last_user = request
            .messages
            .iter()
            .rev()
            .find(|message| message.role == ChatRole::User)
            .map(|message| {
                if message.media.is_empty() {
                    message.content.clone()
                } else {
                    format!("{} [media:{}]", message.content, message.media.len())
                }
            })
            .unwrap_or_default();
        Ok(vec![
            StreamEvent::Text(format!(
                "[{}/{}] ",
                self.config.model.provider, self.config.model.id
            )),
            StreamEvent::Text(last_user),
            StreamEvent::Stop {
                reason: "stop".to_string(),
            },
        ])
    }

    async fn complete_streaming(
        &self,
        request: ProviderRequest,
        on_event: &mut (dyn FnMut(StreamEvent) -> Result<(), ProviderError> + Send),
    ) -> Result<(), ProviderError> {
        if self.is_script() {
            return FauxScript::from_env()?.respond(&request, on_event).await;
        }
        let delay_ms = env::var("NOEL_FAUX_STREAM_DELAY_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or_default();
        let split_chars = env::var("NOEL_FAUX_STREAM_CHARS").ok().as_deref() == Some("1");
        for event in self.complete(request).await? {
            if split_chars {
                if let StreamEvent::Text(text) = event {
                    for ch in text.chars() {
                        if delay_ms > 0 {
                            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                        }
                        on_event(StreamEvent::Text(ch.to_string()))?;
                    }
                    continue;
                }
            }
            if delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            }
            on_event(event)?;
        }
        Ok(())
    }
}

/// `faux/script`: replays the responses in the JSON file named by
/// `NOEL_FAUX_SCRIPT`, an array of responses that are each an array of steps.
/// With `NOEL_FAUX_SCRIPT_LOG`, every request is appended to that file as one
/// JSON line and fsynced before the response starts. The number of requests
/// already logged selects the response, so a restarted process resumes the
/// script where the previous one stopped. Without a log, an in-process counter
/// per script file selects it.
#[derive(Debug, Clone)]
pub(crate) struct FauxScript {
    pub(crate) path: PathBuf,
    pub(crate) log: Option<PathBuf>,
}

/// One step of a scripted response, written as a single-key JSON object.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum FauxScriptStep {
    Text(String),
    Thinking(String),
    ToolCall(FauxScriptToolCall),
    Usage {
        input: u64,
        output: u64,
    },
    Stop(String),
    /// Fails the request here, before or after other steps' output.
    Error(FauxScriptError),
    DelayMs(u64),
    /// Holds the response until the file exists; relative to the script.
    WaitForFile(PathBuf),
}

/// How a scripted request fails: a message is the body of an HTTP 503,
/// which the agent retries; `status` and `body` are any other response,
/// with the wait a `retry-after-ms` header would ask for.
#[derive(Debug, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub(crate) enum FauxScriptError {
    Overloaded(String),
    Status {
        status: u16,
        body: String,
        #[serde(default)]
        retry_after_ms: Option<u64>,
    },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FauxScriptToolCall {
    #[serde(default)]
    pub(crate) id: Option<String>,
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) arguments: serde_json::Map<String, Value>,
}

/// Next response per script file, for scripts run without a request log.
pub(crate) static FAUX_SCRIPT_POSITIONS: Mutex<BTreeMap<PathBuf, usize>> =
    Mutex::new(BTreeMap::new());

impl FauxScript {
    pub(crate) fn from_env() -> Result<Self, ProviderError> {
        let path = env::var_os("NOEL_FAUX_SCRIPT")
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                ProviderError::Config("faux/script needs NOEL_FAUX_SCRIPT to name a script".into())
            })?;
        Ok(Self {
            path: PathBuf::from(path),
            log: env::var_os("NOEL_FAUX_SCRIPT_LOG")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from),
        })
    }

    pub(crate) async fn respond(
        &self,
        request: &ProviderRequest,
        on_event: &mut (dyn FnMut(StreamEvent) -> Result<(), ProviderError> + Send),
    ) -> Result<(), ProviderError> {
        let responses = self.load()?;
        let position = self.next_position(request)?;
        let steps = responses.get(position).ok_or_else(|| {
            ProviderError::Config(format!(
                "faux script {} is exhausted: request {} has no response (the script has {})",
                self.path.display(),
                position + 1,
                responses.len()
            ))
        })?;
        let mut tool_calls = 0;
        let mut stopped = false;
        for step in steps {
            match step {
                FauxScriptStep::Text(text) => on_event(StreamEvent::Text(text.clone()))?,
                FauxScriptStep::Thinking(text) => on_event(StreamEvent::Thinking(text.clone()))?,
                FauxScriptStep::ToolCall(call) => {
                    tool_calls += 1;
                    on_event(StreamEvent::ToolCall {
                        id: call
                            .id
                            .clone()
                            .unwrap_or_else(|| format!("call_{}_{tool_calls}", position + 1)),
                        name: call.name.clone(),
                        arguments: Value::Object(call.arguments.clone()).to_string(),
                    })?;
                }
                FauxScriptStep::Usage { input, output } => on_event(StreamEvent::Usage {
                    input_tokens: *input,
                    output_tokens: *output,
                })?,
                FauxScriptStep::Stop(reason) => {
                    stopped = true;
                    on_event(StreamEvent::Stop {
                        reason: reason.clone(),
                    })?;
                }
                FauxScriptStep::Error(error) => {
                    let (status, body, retry_after) = match error {
                        FauxScriptError::Overloaded(message) => {
                            (reqwest::StatusCode::SERVICE_UNAVAILABLE, message, None)
                        }
                        FauxScriptError::Status {
                            status,
                            body,
                            retry_after_ms,
                        } => {
                            let status = reqwest::StatusCode::from_u16(*status).map_err(|_| {
                                ProviderError::Config(format!(
                                    "faux script {}: {status} is not an HTTP status",
                                    self.path.display()
                                ))
                            })?;
                            (status, body, retry_after_ms.map(Duration::from_millis))
                        }
                    };
                    return Err(ProviderError::Status {
                        status,
                        body: body.clone(),
                        retry_after,
                    });
                }
                FauxScriptStep::DelayMs(delay) => {
                    tokio::time::sleep(Duration::from_millis(*delay)).await;
                }
                FauxScriptStep::WaitForFile(file) => {
                    let file = self.path.parent().unwrap_or(Path::new("")).join(file);
                    while !file.exists() {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                }
            }
        }
        if !stopped {
            let reason = if tool_calls > 0 { "tool_use" } else { "stop" };
            on_event(StreamEvent::Stop {
                reason: reason.to_string(),
            })?;
        }
        Ok(())
    }

    pub(crate) fn load(&self) -> Result<Vec<Vec<FauxScriptStep>>, ProviderError> {
        let text = fs::read_to_string(&self.path).map_err(|error| {
            ProviderError::Config(format!("faux script {}: {error}", self.path.display()))
        })?;
        serde_json::from_str(&text).map_err(|error| {
            ProviderError::Config(format!(
                "invalid faux script {}: {error}",
                self.path.display()
            ))
        })
    }

    /// Index of the response for this request: the number of requests logged
    /// before it (it is appended and fsynced first), or the in-process count.
    pub(crate) fn next_position(&self, request: &ProviderRequest) -> Result<usize, ProviderError> {
        let Some(log) = &self.log else {
            let mut positions = FAUX_SCRIPT_POSITIONS
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let next = positions.entry(self.path.clone()).or_default();
            *next += 1;
            return Ok(*next - 1);
        };
        let log_error = |error: std::io::Error| {
            ProviderError::Config(format!("faux script log {}: {error}", log.display()))
        };
        let position = match fs::read(log) {
            Ok(bytes) => bytes.iter().filter(|byte| **byte == b'\n').count(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
            Err(error) => return Err(log_error(error)),
        };
        let mut line = faux_script_log_entry(request).to_string();
        line.push('\n');
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
            .map_err(log_error)?;
        file.write_all(line.as_bytes()).map_err(log_error)?;
        file.sync_all().map_err(log_error)?;
        Ok(position)
    }
}

/// What one request sent to the model, as a `NOEL_FAUX_SCRIPT_LOG` line.
pub(crate) fn faux_script_log_entry(request: &ProviderRequest) -> Value {
    let messages = request
        .messages
        .iter()
        .map(|message| {
            let mut entry = json!({ "role": message.role, "content": message.content });
            if !message.tool_calls.is_empty() {
                entry["tool_calls"] = message
                    .tool_calls
                    .iter()
                    .map(|call| {
                        json!({
                            "id": call.id,
                            "name": call.name,
                            "arguments": serde_json::from_str::<Value>(&call.arguments)
                                .unwrap_or_else(|_| Value::String(call.arguments.clone())),
                        })
                    })
                    .collect();
            }
            if let Some(id) = &message.tool_call_id {
                entry["tool_call_id"] = json!(id);
            }
            if let Some(name) = &message.tool_name {
                entry["tool_name"] = json!(name);
            }
            if !message.media.is_empty() {
                entry["media"] = json!(message.media.len());
            }
            entry
        })
        .collect::<Vec<_>>();
    json!({
        "system_prompt": request.system_prompt,
        "messages": messages,
        "tools": request.tools.iter().map(|tool| &tool.name).collect::<Vec<_>>(),
    })
}
