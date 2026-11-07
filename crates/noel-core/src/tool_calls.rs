//! Running a model's tool call: built-in tools, extension tools (v1 one-shot, v2 hosts, plugins), and the extension and plugin tools offered to the model.

use super::*;

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExtensionManifest {
    protocol: Option<String>,
    #[serde(default)]
    tools: Vec<ExtensionToolManifest>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExtensionToolManifest {
    name: String,
    description: Option<String>,
    parameters: Option<Value>,
    #[serde(default)]
    replay_safe: bool,
    prompt_snippet: Option<String>,
    #[serde(default)]
    prompt_guidelines: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExtensionProtocolRequest<'a> {
    protocol_version: u8,
    kind: &'static str,
    command: &'a str,
    input: &'a str,
    cwd: &'a str,
    tool_call_id: Option<&'a str>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExtensionCommandResponse {
    output: Option<String>,
    error: Option<String>,
}

/// Overlay the live v2 registrations on the manifest tools the run
/// presents: a live registration replaces the manifest entry of its name
/// (the handshake's word wins over the manifest's), and a v2 extension's
/// manifest tools that the handshake did not keep are withdrawn — the
/// handshake response is the extension's whole registration set, like
/// upstream's factory: what it does not register does not run.
pub(crate) fn overlay_live_registrations(
    systems: &mut ReloadableSystems,
    hosts: &extensions::ExtensionHosts,
) {
    // Only the extensions loaded now: a host a reload removed stops at the
    // next run, and its tools are not offered meanwhile.
    let loaded: BTreeSet<&str> = systems
        .extension_files
        .iter()
        .map(|file| file.name.as_str())
        .collect();
    let sets = hosts
        .registration_sets()
        .into_iter()
        .filter(|set| loaded.contains(set.extension_name.as_str()))
        .collect::<Vec<_>>();
    if sets.is_empty() {
        return;
    }
    let v2_extensions: BTreeSet<String> =
        sets.iter().map(|set| set.extension_name.clone()).collect();
    // Withdraw the manifest tools of v2 extensions that answered.
    systems
        .extension_tools
        .retain(|_, tool| !v2_extensions.contains(&tool.extension_name));
    for set in sets {
        for registration in set.tools {
            systems.extension_tools.insert(
                registration.name.clone(),
                ExtensionTool {
                    name: registration.name.clone(),
                    description: if registration.description.is_empty() {
                        format!("Run extension tool {}.", registration.name)
                    } else {
                        registration.description.clone()
                    },
                    parameters: registration.parameters.clone(),
                    extension_name: set.extension_name.clone(),
                    extension_path: PathBuf::new(),
                    protocol: "2".to_string(),
                    replay_safe: registration.replay_safe,
                    prompt_snippet: system_prompt::normalize_snippet(
                        registration.prompt_snippet.as_deref(),
                    ),
                    prompt_guidelines: system_prompt::normalize_guidelines(
                        &registration.prompt_guidelines,
                    ),
                },
            );
        }
    }
}

/// Offers the plugin tools as extension tools of the protocol `plugin`,
/// replacing the plugin tools offered before. A tool named like a built-in
/// or an extension's is left out. They are available as extension tools
/// are: when `enabledTools` names them (or is unset), and `excludedTools`
/// does not.
pub(crate) fn overlay_plugin_tools(systems: &mut ReloadableSystems, tools: &[plugins::PluginTool]) {
    let previous: Vec<String> = systems
        .extension_tools
        .iter()
        .filter(|(_, tool)| tool.protocol == "plugin")
        .map(|(name, _)| name.clone())
        .collect();
    for name in previous {
        systems.extension_tools.remove(&name);
        systems.available_tool_names.remove(&name);
    }
    let builtin: BTreeSet<String> = builtin_tool_definitions()
        .into_iter()
        .map(|definition| definition.name)
        .collect();
    for tool in tools {
        if builtin.contains(&tool.name) || systems.extension_tools.contains_key(&tool.name) {
            continue;
        }
        systems.extension_tools.insert(
            tool.name.clone(),
            ExtensionTool {
                name: tool.name.clone(),
                description: tool.description.clone(),
                parameters: tool.parameters.clone(),
                extension_name: tool.plugin.clone(),
                extension_path: PathBuf::new(),
                protocol: "plugin".to_string(),
                replay_safe: tool.replay_safe,
                prompt_snippet: None,
                prompt_guidelines: Vec::new(),
            },
        );
        let (enabled, excluded) = &systems.tool_selection;
        let selected = enabled
            .as_ref()
            .is_none_or(|enabled| noel_config::tool_name_matches(enabled, &tool.name));
        if selected && !noel_config::tool_name_matches(excluded, &tool.name) {
            systems.available_tool_names.insert(tool.name.clone());
        }
    }
}

pub(crate) fn extension_tools_from_resources(
    extensions: &[ResourceFile],
) -> BTreeMap<String, ExtensionTool> {
    let mut tools = BTreeMap::new();
    for extension in extensions {
        let Some(manifest) = read_extension_manifest(&extension.path) else {
            continue;
        };
        let protocol = manifest.protocol.unwrap_or_else(|| "json".to_string());
        if protocol != "json" {
            continue;
        }
        for tool in manifest.tools {
            let name = tool.name.trim();
            if name.is_empty() {
                continue;
            }
            tools.insert(
                name.to_string(),
                ExtensionTool {
                    name: name.to_string(),
                    description: tool
                        .description
                        .unwrap_or_else(|| format!("Run extension tool {name}.")),
                    parameters: tool.parameters.unwrap_or_else(|| {
                        serde_json::json!({
                            "type": "object",
                            "properties": {},
                            "required": []
                        })
                    }),
                    extension_name: extension.name.clone(),
                    extension_path: extension.path.clone(),
                    protocol: protocol.clone(),
                    replay_safe: tool.replay_safe,
                    prompt_snippet: system_prompt::normalize_snippet(
                        tool.prompt_snippet.as_deref(),
                    ),
                    prompt_guidelines: system_prompt::normalize_guidelines(&tool.prompt_guidelines),
                },
            );
        }
    }
    tools
}

pub(crate) fn read_extension_manifest(path: &Path) -> Option<ExtensionManifest> {
    extension_manifest_paths(path).into_iter().find_map(|path| {
        let content = fs::read_to_string(path).ok()?;
        serde_json::from_str::<ExtensionManifest>(&content).ok()
    })
}

pub(crate) fn extension_manifest_paths(path: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(file_name) = path.file_name() {
        paths.push(path.with_file_name(format!(
            "{}.noel-extension.json",
            file_name.to_string_lossy()
        )));
        // Extensions written for pi ship a .pi-extension.json manifest.
        paths.push(
            path.with_file_name(format!("{}.pi-extension.json", file_name.to_string_lossy())),
        );
        paths.push(path.with_file_name(format!("{}.json", file_name.to_string_lossy())));
    }
    if path.extension().is_some() {
        paths.push(path.with_extension("json"));
    }
    paths
}

/// What a bash command the model runs learns about its session (upstream's
/// `bash.ts`): the session, model, and thinking level as of that command,
/// each under noel's name and under pi's, which skills and scripts written
/// for pi read. A variable with nothing to report is removed under both
/// names, so a noel running inside another noel's command (or pi's) never
/// passes on its parent's. Commands the user types (`/bash`, `!!`) get none
/// of this.
pub(crate) fn bash_session_env(runtime: &Runtime) -> Vec<(String, Option<String>)> {
    let model = runtime.session.active_model.as_ref();
    let session_file = runtime.store.as_ref().map(|store| {
        let path = std::path::absolute(store.path()).unwrap_or_else(|_| store.path().into());
        path.display().to_string()
    });
    let thinking_level = runtime.thinking_level.as_deref().unwrap_or("off");
    [
        (
            ["NOEL_SESSION_ID", "PI_SESSION_ID"],
            Some(runtime.session.session_id.clone()),
        ),
        (["NOEL_SESSION_FILE", "PI_SESSION_FILE"], session_file),
        (
            ["NOEL_PROVIDER", "PI_PROVIDER"],
            model.map(|model| model.provider.clone()),
        ),
        (
            ["NOEL_MODEL", "PI_MODEL"],
            model.map(|model| model.id.clone()),
        ),
        (
            ["NOEL_REASONING_LEVEL", "PI_REASONING_LEVEL"],
            Some(thinking_level.to_string()),
        ),
    ]
    .into_iter()
    .flat_map(|(names, value)| names.map(|name| (name.to_string(), value.clone())))
    .collect()
}

#[derive(Debug, Deserialize)]
pub(crate) struct TodoToolArgs {
    todos: Vec<TodoItem>,
}

/// A reply cut off by the output limit can carry truncated arguments that
/// still parse, so its tool calls are recorded as errors instead of run.
pub(crate) fn fail_truncated_tool_call(
    runtime: &mut Runtime,
    tool_call: &ChatToolCall,
) -> Result<(), AgentError> {
    let output = noel_ai::truncated_tool_call_error(&tool_call.name);
    runtime.push_tool_result(tool_call, output, Vec::new(), true)?;
    Ok(())
}

/// Runs a tool call of the model and saves its result. `on_output` gets a
/// running `bash` command's output so far.
#[cfg(test)]
pub(crate) async fn execute_model_tool_call(
    runtime: &mut Runtime,
    tool_call: &ChatToolCall,
    on_output: &mut (dyn FnMut(OutputSoFar<'_>) + Send),
) -> Result<(), AgentError> {
    execute_model_tool_call_with(runtime, tool_call, on_output, &mut |_| {}, None).await
}

/// [`execute_model_tool_call`], with a `codemode` call's nested calls
/// reported to `on_nested` as they change.
pub(crate) async fn execute_model_tool_call_with(
    runtime: &mut Runtime,
    tool_call: &ChatToolCall,
    on_output: &mut (dyn FnMut(OutputSoFar<'_>) + Send),
    on_nested: &mut (dyn FnMut(Vec<NestedToolCall>) + Send),
    cancellation: Option<&CancellationToken>,
) -> Result<(), AgentError> {
    let extension_tool = runtime
        .systems
        .extension_tools
        .get(&tool_call.name)
        .cloned();
    let mut is_error = false;
    let mut images = Vec::new();
    let outputs = if let Some(extension_tool) =
        extension_tool.filter(|_| model_tool_enabled(runtime, &tool_call.name))
    {
        // A persistent v2 host serves its tools over one process; v1 runs
        // one process per call. Both are started lazily, on the call that
        // needs them. A plugin's tool runs through the plugin host.
        if extension_tool.protocol == "plugin" {
            match runtime.plugin_host.clone() {
                Some(host) => {
                    let outcome = host
                        .call(
                            &extension_tool.extension_name,
                            &extension_tool.name,
                            serde_json::from_str(&tool_call.arguments)
                                .unwrap_or(Value::Object(Default::default())),
                        )
                        .await;
                    is_error = outcome.is_error;
                    vec![outcome.output]
                }
                None => {
                    is_error = true;
                    vec![format!(
                        "plugin {} is not reachable",
                        extension_tool.extension_name
                    )]
                }
            }
        } else if extension_tool.protocol == "2" {
            let extension_name = extension_tool.extension_name.clone();
            let tool_name = extension_tool.name.clone();
            let call = tool_call.clone();
            let cwd = runtime.session.cwd.clone();
            let hosts = runtime.extension_hosts();
            let mut hosts = hosts.lock().await;
            let outcome = hosts
                .call(
                    &extension_name,
                    "tool",
                    &tool_name,
                    &call.arguments,
                    &cwd,
                    Some(&call.id),
                )
                .await;
            match outcome {
                Ok(outcome) => {
                    is_error = outcome.is_error;
                    if let Some(set) = outcome.registrations {
                        drop(hosts);
                        runtime.apply_extension_registrations(set).await;
                    }
                    vec![outcome.output]
                }
                Err(error) => {
                    is_error = true;
                    vec![error]
                }
            }
        } else {
            match execute_extension_tool(&runtime.session.cwd, &extension_tool, tool_call).await {
                Ok(result) => {
                    is_error = result.is_error;
                    vec![result.output]
                }
                Err(error) => {
                    is_error = true;
                    vec![error.to_string()]
                }
            }
        }
    } else if tool_call.name == codemode::CODEMODE {
        if model_tool_enabled(runtime, codemode::CODEMODE) {
            let callable = codemode::callable(&enabled_tool_definitions(runtime));
            let guidelines = tool_guidelines(runtime);
            let result = codemode::run(
                runtime,
                tool_call,
                &callable,
                &guidelines,
                on_nested,
                cancellation,
            )
            .await?;
            is_error = result.is_error;
            images = result.images;
            vec![result.output]
        } else {
            is_error = true;
            vec![format!("Tool {} not found", tool_call.name)]
        }
    } else if tool_call.name == "todo" {
        if model_tool_enabled(runtime, "todo") {
            match serde_json::from_str::<TodoToolArgs>(&tool_call.arguments) {
                Ok(args) => match runtime.set_todos(args.todos) {
                    Ok(()) => vec![format_todo_list(&runtime.session.todos)],
                    Err(error) => {
                        is_error = true;
                        vec![error.to_string()]
                    }
                },
                Err(error) => {
                    is_error = true;
                    vec![format!("invalid todo arguments: {error}")]
                }
            }
        } else {
            is_error = true;
            vec![format!("Tool {} not found", tool_call.name)]
        }
    } else {
        match model_tool_requests(tool_call) {
            Ok(requests) if model_tool_enabled(runtime, &tool_call.name) => {
                let mut outputs = Vec::new();
                let mut edited_paths = Vec::new();
                // One workspace for the call; failing to get one fails it.
                let env = runtime.tool_env();
                for request in requests {
                    let edited_path = match &request {
                        ToolRequest::Edit { path, .. } | ToolRequest::Write { path, .. } => {
                            Some(path.clone())
                        }
                        _ => None,
                    };
                    let env = match &env {
                        Ok(env) => env.clone(),
                        Err(message) => {
                            is_error = true;
                            outputs.push(message.clone());
                            continue;
                        }
                    };
                    let output = match execute_tool_streaming_in(
                        env.as_ref(),
                        request,
                        &tool_runtime_options(runtime, bash_session_env(runtime)),
                        on_output,
                    )
                    .await
                    {
                        Ok(result) => {
                            is_error |= result.is_error;
                            if let Some(path) = edited_path {
                                edited_paths.push(path);
                            }
                            images.extend(result.images);
                            result.output
                        }
                        Err(error) => {
                            is_error = true;
                            error.to_string()
                        }
                    };
                    outputs.push(output);
                }
                if !edited_paths.is_empty() {
                    runtime.note_edited_files(edited_paths)?;
                }
                outputs
            }
            Ok(_) => {
                is_error = true;
                vec![format!("Tool {} not found", tool_call.name)]
            }
            Err(error) => {
                is_error = true;
                vec![error.to_string()]
            }
        }
    };
    let output = outputs.join("\n");
    runtime.push_tool_result(tool_call, output, tool_result_media(images), is_error)?;
    Ok(())
}

/// Runs an extension's tool. Its output is capped like a built-in tool's,
/// and a non-zero exit is an error result carrying what the extension
/// printed.
pub(crate) async fn execute_extension_tool(
    cwd: &Path,
    extension: &ExtensionTool,
    tool_call: &ChatToolCall,
) -> Result<ToolResult, AgentError> {
    let cwd_string = cwd.display().to_string();
    let request = ExtensionProtocolRequest {
        protocol_version: 1,
        kind: "tool",
        command: &tool_call.name,
        input: &tool_call.arguments,
        cwd: &cwd_string,
        tool_call_id: Some(&tool_call.id),
    };
    let mut command = Command::new(&extension.extension_path);
    command
        .current_dir(cwd)
        .env("NOEL_EXTENSION_NAME", &extension.extension_name)
        .env("NOEL_EXTENSION_PATH", &extension.extension_path)
        .env("NOEL_EXTENSION_PROTOCOL", &extension.protocol)
        .env("NOEL_EXTENSION_TOOL", &extension.name)
        // Extensions written for pi read the same values under PI_ names.
        .env("PI_EXTENSION_NAME", &extension.extension_name)
        .env("PI_EXTENSION_PATH", &extension.extension_path)
        .env("PI_EXTENSION_PROTOCOL", &extension.protocol)
        .env("PI_EXTENSION_TOOL", &extension.name)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Like bash: no controlling terminal, and an interrupted call (this
    // future dropped) kills the extension's whole process group.
    let (mut child, group) = spawn_tool_process(&mut command).map_err(|source| {
        AgentError::Tool(ToolError::Io {
            path: extension.extension_path.clone(),
            source,
        })
    })?;
    if let Some(mut stdin) = child.stdin.take() {
        let input = format!(
            "{}\n",
            serde_json::to_string(&request).map_err(|source| AgentError::Session(
                SessionError::Parse {
                    path: extension.extension_path.clone(),
                    source,
                }
            ))?
        );
        stdin.write_all(input.as_bytes()).await.map_err(|source| {
            AgentError::Tool(ToolError::Io {
                path: extension.extension_path.clone(),
                source,
            })
        })?;
    }
    let output = child.wait_with_output().await.map_err(|source| {
        AgentError::Tool(ToolError::Io {
            path: extension.extension_path.clone(),
            source,
        })
    })?;
    group.finish();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        return Ok(ToolResult::error(cap_tool_output(format!(
            "extension {} failed:\n{}{}",
            extension.extension_name, stdout, stderr
        ))));
    }
    let response =
        serde_json::from_str::<ExtensionCommandResponse>(stdout.trim()).map_err(|source| {
            AgentError::Session(SessionError::Parse {
                path: extension.extension_path.clone(),
                source,
            })
        })?;
    if let Some(error) = response.error {
        return Err(AgentError::InvalidToolCall {
            tool: tool_call.name.clone(),
            message: format!("extension {} failed: {error}", extension.extension_name),
        });
    }
    Ok(ToolResult::text(cap_tool_output(
        response.output.unwrap_or_else(|| {
            format!(
                "extension {} tool {} completed",
                extension.extension_name, extension.name
            )
        }),
    )))
}
