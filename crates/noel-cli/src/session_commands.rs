//! The session side of the TUI's slash commands: everything a command does
//! to the session, its configuration and the machine it runs on, behind
//! [`CommandUi`], so the same code serves the in-process TUI and a worker
//! answering an attached client (docs/client-tui-plan.md). What only the
//! terminal can do (the editor, the clipboard, the screen, local files to
//! attach) stays in `handle_tui_submission`.

use super::*;

/// Where a command's output goes.
pub(crate) trait CommandUi {
    fn push(&mut self, kind: TuiEntryKind, text: impl Into<String>);
    /// A row a later [`CommandUi::replace`] fills in; its index.
    fn placeholder(&mut self, kind: TuiEntryKind, text: impl Into<String>) -> usize;
    fn replace(&mut self, index: usize, text: impl Into<String>);
    /// Shows what was pushed so far, before a slow step.
    fn redraw(&mut self, config: &LoadedConfig) -> Result<()>;
    /// The session changed: whether its last turn is unfinished, so the
    /// client resumes it.
    fn set_resume_pending(&mut self, pending: bool);
    /// Moves to another session: in place for the in-process TUI
    /// ([`switch_in_place`]), or by telling the client which one.
    fn switch_session(
        &mut self,
        runtime: &mut Runtime,
        config: &LoadedConfig,
        switch: SessionSwitch,
    ) -> Result<CommandOutcome>;
}

/// Another session a command moves to.
pub(crate) enum SessionSwitch {
    /// An existing session; the client opening it continues its
    /// unfinished turn.
    Open {
        path: PathBuf,
    },
    /// A fork (or, with `clone`, a full copy) of `source`.
    Fork {
        source: Box<SessionState>,
        clone: bool,
    },
    Import(PathBuf),
    /// A new session in the same cwd and workspace, with the same model.
    New,
}

/// Writes `switch`'s session without opening it here, for a client to
/// attach to: its id.
pub(crate) fn switch_elsewhere(
    runtime: &Runtime,
    config: &LoadedConfig,
    switch: SessionSwitch,
) -> Result<String> {
    Ok(match switch {
        SessionSwitch::Open { path } => SessionStore::read(&path)?.session_id,
        SessionSwitch::Fork { source, clone } => {
            SessionStore::fork(&config.paths.session_dir, &source, clone)?
                .1
                .session_id
        }
        SessionSwitch::Import(path) => {
            SessionStore::import_path(&config.paths.session_dir, &path)?
                .1
                .session_id
        }
        SessionSwitch::New => new_session_like(runtime, config)?.1.session_id,
    })
}

/// A new session in `runtime`'s cwd and workspace, with its model,
/// thinking level and account (the new session keeps the workspace, as it
/// keeps the model).
pub(crate) fn new_session_like(
    runtime: &Runtime,
    config: &LoadedConfig,
) -> Result<(SessionStore, SessionState)> {
    Ok(SessionStore::create_with_state(
        &config.paths.session_dir,
        session_like(runtime.session(), noel_core::new_session_id()),
    )?)
}

/// A new, empty session `id` with `current`'s cwd, workspace, model,
/// thinking level and account: what `/new` starts from, with a journal or
/// without (`--no-session`).
pub(crate) fn session_like(current: &SessionState, id: String) -> SessionState {
    let mut session = SessionState::new(id, current.cwd.clone());
    session.active_model = current.active_model.clone();
    session.active_thinking_level = current.active_thinking_level.clone();
    session.active_account = current.active_account.clone();
    session.workspace = current.workspace.clone();
    session
}

/// What a command leaves for the client to do.
pub(crate) enum CommandOutcome {
    Handled,
    /// Send this to the model.
    Submit {
        prompt: TypedPrompt,
        media: Vec<MediaInput>,
    },
    /// Compact now, with this focus.
    Compact {
        focus: String,
    },
    /// Run a provider login.
    Login {
        provider: String,
        account: String,
    },
    /// Open this selector.
    Selector(Box<TuiSelectorState>),
    /// Attach to this session instead (a worker's answer to a switch).
    Session(String),
}

/// Opens selector `kind`, filtered by `query`: an empty one is a note.
pub(crate) fn open_selector(
    ui: &mut impl CommandUi,
    config: &LoadedConfig,
    runtime: &Runtime,
    kind: &str,
    query: &str,
) -> Result<CommandOutcome> {
    let selector = selector_for_kind(config, runtime, kind)?;
    if selector.items.is_empty() {
        ui.push(TuiEntryKind::System, format!("no {kind}"));
        return Ok(CommandOutcome::Handled);
    }
    let mut state =
        TuiSelectorState::new(kind, selector, query, None).with_model_thinking(config, runtime);
    if kind == "branches" {
        state.set_tree(crate::session_tree_rows(config)?);
    }
    ui.push(TuiEntryKind::System, format!("{} selector", state.title));
    Ok(CommandOutcome::Selector(Box::new(state)))
}

/// Runs the session side of `line`, a slash command or a prompt.
pub(crate) async fn run(
    ui: &mut impl CommandUi,
    runtime: &mut Runtime,
    config: &mut LoadedConfig,
    offline: bool,
    line: String,
) -> Result<CommandOutcome> {
    match line.as_str() {
        "/image-models" => ui.push(TuiEntryKind::System, format_image_models(config, "")),
        "/thinking" => ui.push(
            TuiEntryKind::System,
            format!(
                "thinking: {}",
                thinking_label(runtime, config).unwrap_or_else(|| "-".to_string())
            ),
        ),
        "/model" | "/models" | "/scoped-models" => {
            // Codex-style two-layer picker: provider first, then its models.
            return open_selector(ui, config, runtime, "provider", "");
        }
        "/account" => return open_selector(ui, config, runtime, "account", ""),
        "/todos" => {
            ui.push(
                TuiEntryKind::System,
                format_todo_list(&runtime.session().todos),
            );
        }
        "/accounts" | "/usage" => {
            if line == "/usage" {
                ui.push(TuiEntryKind::System, format_session_usage(runtime));
            }
            if offline || runtime.systems().offline {
                ui.push(
                    TuiEntryKind::System,
                    "account quotas unavailable in offline mode",
                );
                return Ok(CommandOutcome::Handled);
            }
            let checking = ui.placeholder(
                TuiEntryKind::System,
                "checking account status...".to_string(),
            );
            ui.redraw(config)?;
            let now = unix_seconds().unwrap_or(0) as i64;
            let rows = usage::collect_account_status(config, false, None, None, now).await;
            ui.replace(checking, usage::format_account_status(&rows, now));
        }
        "/session" => ui.push(TuiEntryKind::System, format_session(runtime)),
        "/settings" => return open_selector(ui, config, runtime, "settings", ""),
        "/settings show" => ui.push(TuiEntryKind::System, format_settings(config, runtime)),
        "/diagnostics" => ui.push(TuiEntryKind::System, format_diagnostics(config)),
        "/skills" => ui.push(
            TuiEntryKind::System,
            resource_commands::format_skills(config),
        ),
        "/prompts" => ui.push(
            TuiEntryKind::System,
            resource_commands::format_prompts(config),
        ),
        "/extensions" => ui.push(
            TuiEntryKind::System,
            format_resources("extensions", &config.extensions),
        ),
        "/queue" => ui.push(TuiEntryKind::System, format_queue(runtime)),
        "/queue-clear" => {
            let cleared = runtime.clear_queued_messages()?;
            ui.push(
                TuiEntryKind::System,
                format!("cleared {cleared} queued message(s)"),
            );
        }
        "/interrupt" => {
            let cleared = runtime.clear_queued_messages()?;
            ui.push(
                TuiEntryKind::System,
                format!("interrupted; cleared {cleared} queued message(s)"),
            );
        }
        "/tree" | "/tree --print" => {
            // The navigator; --print keeps the printed form for terminals
            // and transcripts where a selector cannot run.
            if line == "/tree --print" {
                ui.push(
                    TuiEntryKind::System,
                    format_session_tree(
                        &config.paths.session_dir,
                        Some(&runtime.session().session_id),
                    )?,
                );
            } else {
                return open_selector(ui, config, runtime, "branches", "");
            }
        }
        "/summaries" => ui.push(TuiEntryKind::System, format_summaries(runtime)),
        "/mcp" => {
            // The configured MCP servers and their live state (§9.7).
            let servers = runtime.systems().mcp_servers.clone();
            let mcp_tools = runtime.systems().mcp_tools.clone();
            if servers.is_empty() {
                ui.push(
                    TuiEntryKind::System,
                    "no MCP servers configured; add one to mcp.json (see the README)".to_string(),
                );
            } else {
                let lines = servers
                    .iter()
                    .map(|server| {
                        let tools = mcp_tools
                            .iter()
                            .filter(|tool| {
                                tool.name.starts_with(&format!(
                                    "mcp__{}__",
                                    server.name.replace('-', "_")
                                ))
                            })
                            .count();
                        match &server.config {
                            noel_config::mcp::McpServerConfig::Stdio { command, .. } => format!(
                                "{}  {} ({} tool{})",
                                server.name,
                                command,
                                tools,
                                if tools == 1 { "" } else { "s" }
                            ),
                            noel_config::mcp::McpServerConfig::Http { url, .. } => {
                                format!("{}  {url} (http)", server.name)
                            }
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                ui.push(TuiEntryKind::System, lines);
            }
        }
        _ if line.split_whitespace().next() == Some("/compact") => {
            let focus = line.strip_prefix("/compact").unwrap_or_default().trim();
            return Ok(CommandOutcome::Compact {
                focus: focus.to_string(),
            });
        }
        "/reload" => {
            // v1 one-shot extensions get their reload notification; the v2
            // hosts get it too, then are replaced by the next generation's
            // (a removed extension's host is stopped, a new one starts with
            // its next run or command).
            let mut lifecycle_diagnostics =
                notify_extension_lifecycle(&config.extensions, "reload", &runtime.session().cwd);
            {
                let hosts = runtime.extension_hosts();
                let mut hosts = hosts.lock().await;
                lifecycle_diagnostics.extend(hosts.notify("reload", &runtime.session().cwd).await);
            }
            *config = reload_config(config)?;
            start_model_refresh(config, offline, false);
            let next_generation = runtime.systems().config_generation + 1;
            let report = runtime.reload(ReloadableSystems::from_config(config, next_generation))?;
            let mut output = format_diagnostics(config);
            for diagnostic in lifecycle_diagnostics {
                output.push_str(&format!("\n{diagnostic}"));
            }
            if !report.active_model_valid {
                output
                    .push_str("\nactive model is no longer available; use /model <provider/model>");
            }
            if !report.active_account_valid {
                output.push_str("\nactive account is no longer available; use /account");
            }
            if !report.removed_active_tools.is_empty() {
                output.push_str(&format!(
                    "\nremoved active tools: {}",
                    report.removed_active_tools.join(", ")
                ));
            }
            output.push_str("\nreloaded");
            ui.push(TuiEntryKind::System, output);
        }
        "/trust" => {
            let cwd = &runtime.session().cwd;
            let trust_path = save_project_trust(&config.paths.agent_dir, cwd, true)?;
            ui.push(
                TuiEntryKind::System,
                format!(
                    "saved trust decision: trusted {} ({})",
                    cwd.display(),
                    trust_path.display()
                ),
            );
        }
        _ if line.starts_with("/complete ") => {
            let prefix = line.trim_start_matches("/complete ").trim();
            let completions = command_completions(config, prefix);
            ui.push(
                TuiEntryKind::System,
                if completions.is_empty() {
                    "no completions".to_string()
                } else {
                    completions.join("\n")
                },
            );
        }
        _ if line.starts_with("/image-models ") => {
            let search = line.trim_start_matches("/image-models ").trim();
            ui.push(TuiEntryKind::System, format_image_models(config, search));
        }
        _ if line.starts_with("/models ") => {
            let arg = line.trim_start_matches("/models ").trim();
            match arg {
                "refresh" => {
                    if offline {
                        ui.push(
                            TuiEntryKind::Error,
                            "model refresh is disabled in offline mode",
                        );
                    } else if !model_refresh_enabled(&config.settings) {
                        ui.push(
                            TuiEntryKind::Error,
                            "model refresh is disabled by modelRefresh.enabled",
                        );
                    } else {
                        let report =
                            refresh_model_cache(config.paths.clone(), config.auth.clone()).await?;
                        *config = reload_config(config)?;
                        let next_generation = runtime.systems().config_generation + 1;
                        let reload_report = runtime
                            .reload(ReloadableSystems::from_config(config, next_generation))?;
                        ui.push(
                            TuiEntryKind::System,
                            format_model_refresh_after_reload(&report, config, &reload_report),
                        );
                    }
                }
                "reset" => {
                    let message = reset_model_cache(&config.paths.model_cache_path)?;
                    *config = reload_config(config)?;
                    let next_generation = runtime.systems().config_generation + 1;
                    let reload_report =
                        runtime.reload(ReloadableSystems::from_config(config, next_generation))?;
                    let mut output = message;
                    if !reload_report.active_model_valid {
                        output.push_str(
                            "\nactive model is no longer available; use /model <provider/model>",
                        );
                    }
                    output.push_str(&format!(
                        "\n{} models available; run /models refresh to repopulate",
                        config.models.len()
                    ));
                    ui.push(TuiEntryKind::System, output);
                }
                "help" | "usage" => ui.push(
                    TuiEntryKind::System,
                    "usage: /models [refresh|reset|<query>]",
                ),
                query => return open_selector(ui, config, runtime, "model", query),
            }
        }
        _ if line.starts_with("/scoped-models ") => {
            let query = line.trim_start_matches("/scoped-models ").trim();
            return open_selector(ui, config, runtime, "model", query);
        }
        _ if line.starts_with("/generate-image ") => {
            let rest = line.trim_start_matches("/generate-image ").trim();
            let (path, prompt) = split_once_text(rest);
            if path.is_empty() || prompt.is_empty() {
                ui.push(
                    TuiEntryKind::Error,
                    "usage: /generate-image <output> <prompt>",
                );
            } else {
                match generate_image_to_path(
                    config,
                    &runtime.session().cwd,
                    "openrouter/google/gemini-3.1-flash-image-preview",
                    Path::new(path),
                    prompt,
                    &[],
                )
                .await
                {
                    Ok(output) => ui.push(TuiEntryKind::System, output),
                    Err(error) => ui.push(TuiEntryKind::Error, format!("{error}")),
                }
            }
        }
        _ if line.starts_with("/queue ") => {
            let message = expand_input(config, line.trim_start_matches("/queue ").trim());
            runtime.queue_message(message)?;
            ui.push(
                TuiEntryKind::System,
                format!("queued: {}", runtime.session().queue.len()),
            );
        }
        _ if line.starts_with("/selector ") => {
            let kind = line.trim_start_matches("/selector ").trim();
            return open_selector(ui, config, runtime, kind, "");
        }
        // A settings pick applies it and lists the settings again.
        _ if line.starts_with("/select settings ") => {
            let value = line.trim_start_matches("/select settings ").trim();
            let message = apply_settings_item(config, runtime, value)?;
            ui.push(TuiEntryKind::System, message);
            let selector = selector_for_kind(config, runtime, "settings")?;
            let mut state = TuiSelectorState::new("settings", selector, "", None);
            if let Some(index) = state
                .filtered_indices
                .iter()
                .position(|&index| state.items[index].value == value)
            {
                state.selected = index;
            }
            return Ok(CommandOutcome::Selector(Box::new(state)));
        }
        _ if line.starts_with("/select ") => {
            let (kind, _) = split_once_text(line.trim_start_matches("/select ").trim());
            let resuming = matches!(kind, "session" | "sessions" | "resume" | "tree");
            ui.push(
                TuiEntryKind::System,
                select_from_selector_message(config, runtime, &line)?,
            );
            if resuming {
                ui.set_resume_pending(runtime.session().has_unfinished_turn());
                if let Some(hint) = resume_hint(
                    runtime.session().has_unfinished_turn(),
                    runtime.session().queue.len(),
                ) {
                    ui.push(TuiEntryKind::System, hint);
                }
            }
        }
        _ if line.starts_with("/model ") => {
            // As upstream: an exact reference switches, and anything else
            // opens the picker with the text as its search.
            let reference = line.trim_start_matches("/model ").trim();
            match resolve_model_reference(config, reference, offline) {
                Some(model) => {
                    switch_model(runtime, config, &model, None)?;
                    persist_default_model(config, &model)?;
                    ui.push(
                        TuiEntryKind::System,
                        format_model_selection(&model, thinking_label(runtime, config).as_deref()),
                    );
                }
                None => return open_selector(ui, config, runtime, "model", reference),
            }
        }
        _ if line.starts_with("/thinking ") => {
            // As upstream: one of the levels the model supports, by name.
            let requested = line.trim_start_matches("/thinking ").trim();
            let available = runtime
                .session()
                .active_model
                .as_ref()
                .and_then(|model| model_definition(config, model))
                .map(|model| model.supported_thinking_levels())
                .unwrap_or_else(|| noel_config::THINKING_LEVELS.to_vec());
            let level = normalized_thinking_level(requested)
                .filter(|level| available.contains(level))
                .ok_or_else(|| {
                    anyhow!(
                        "Unknown thinking level \"{requested}\". Available levels: {}.",
                        available.join(", ")
                    )
                })?;
            set_thinking_level(runtime, config, level)?;
            ui.push(TuiEntryKind::System, format!("thinking: {level}"));
        }
        _ if line.starts_with("/extension:") => {
            let (name, input) = split_resource_command(&line, "/extension:");
            let extension = find_resource(&config.extensions, name)
                .ok_or_else(|| anyhow!("extension not found: {name}"))?;
            if is_executable_extension(&extension.path) {
                // A v2 extension runs its `/extension:` command on the
                // persistent host, so its state carries between commands
                // and tools; v1 keeps its one-shot run.
                if noel_core::extensions::manifest_protocol_version(&extension.path) == Some(2) {
                    runtime.start_extension_hosts().await;
                    let hosts = runtime.extension_hosts();
                    let mut hosts = hosts.lock().await;
                    let outcome = hosts
                        .call(
                            extension.name.as_str(),
                            "command",
                            extension.name.as_str(),
                            input,
                            &runtime.session().cwd.clone(),
                            None,
                        )
                        .await;
                    match outcome {
                        Ok(outcome) => {
                            if let Some(set) = outcome.registrations {
                                drop(hosts);
                                runtime.apply_extension_registrations(set).await;
                            } else {
                                drop(hosts);
                            }
                            let kind = if outcome.is_error {
                                TuiEntryKind::Error
                            } else {
                                TuiEntryKind::System
                            };
                            ui.push(kind, outcome.output);
                        }
                        Err(error) => {
                            drop(hosts);
                            ui.push(TuiEntryKind::Error, error);
                        }
                    }
                    return Ok(CommandOutcome::Handled);
                }
                match run_executable_extension(extension, input, &runtime.session().cwd) {
                    Ok(output) => ui.push(TuiEntryKind::System, output),
                    Err(error) => ui.push(TuiEntryKind::Error, format!("{error}")),
                }
                return Ok(CommandOutcome::Handled);
            }
            let prompt = if input.is_empty() {
                extension.content.clone()
            } else {
                format!("{}\n\n{}", extension.content, input)
            };
            return Ok(CommandOutcome::Submit {
                prompt: TypedPrompt::from(prompt),
                media: Vec::new(),
            });
        }
        "/prompt" => ui.push(TuiEntryKind::System, "usage: /prompt <name> [input]"),
        _ if line.starts_with("/prompt ") => {
            let rest = line.trim_start_matches("/prompt ").trim();
            let (name, input) = split_once_text(rest);
            let template = resource_commands::find_template(config, name)
                .ok_or_else(|| anyhow!("prompt template not found: {name}"))?;
            let prompt = resource_commands::fill_prompt_template(&template.content, input);
            return Ok(CommandOutcome::Submit {
                prompt: TypedPrompt::from(prompt),
                media: Vec::new(),
            });
        }
        _ if line.starts_with("/new") => {
            return ui.switch_session(runtime, config, SessionSwitch::New);
        }
        _ if line.starts_with("/resume") => {
            let reference = line.trim_start_matches("/resume").trim();
            if reference.is_empty() {
                // Kimi/Claude-style picker: interactive selector with topic,
                // age, message count, model badge, and unfinished marker.
                return open_selector(ui, config, runtime, "session", "");
            } else {
                let path = resolve_session_reference(&config.paths.session_dir, reference)?;
                return ui.switch_session(runtime, config, SessionSwitch::Open { path });
            }
        }
        _ if line.starts_with("/fork") => {
            let source =
                resolve_source_session(&config.paths.session_dir, runtime, &line, "/fork")?;
            return ui.switch_session(
                runtime,
                config,
                SessionSwitch::Fork {
                    source: Box::new(source),
                    clone: false,
                },
            );
        }
        _ if line.starts_with("/clone") => {
            let source =
                resolve_source_session(&config.paths.session_dir, runtime, &line, "/clone")?;
            return ui.switch_session(
                runtime,
                config,
                SessionSwitch::Fork {
                    source: Box::new(source),
                    clone: true,
                },
            );
        }
        _ if line.starts_with("/delete") => {
            ui.push(
                TuiEntryKind::System,
                delete_session_message(config, runtime, &line)?,
            );
        }
        _ if line.starts_with("/name") => {
            let name = line.trim_start_matches("/name").trim();
            if name.is_empty() {
                // Upstream: a bare /name shows the current name; it never
                // clears it.
                ui.push(
                    TuiEntryKind::System,
                    match &runtime.session().name {
                        Some(name) => format!("session name: {name}"),
                        None => "usage: /name <name>".to_string(),
                    },
                );
            } else {
                runtime.rename_session(Some(name.to_string()))?;
                ui.push(TuiEntryKind::System, format_session(runtime));
            }
        }
        _ if line.starts_with("/labels") => {
            let labels = line
                .trim_start_matches("/labels")
                .split_whitespace()
                .map(ToString::to_string)
                .collect();
            runtime.set_labels(labels)?;
            ui.push(TuiEntryKind::System, format_session(runtime));
        }
        _ if line == "/export" || line.starts_with("/export ") => {
            let requested = line.trim_start_matches("/export").trim();
            let path = if requested.is_empty() {
                // Upstream's default: pi-session-<session>.html in the cwd.
                runtime.session().cwd.join(format!(
                    "noel-session-{}.html",
                    runtime.session().session_id
                ))
            } else {
                PathBuf::from(requested)
            };
            export_session(runtime, &path)?;
            ui.push(TuiEntryKind::System, format!("exported {}", path.display()));
        }
        "/import" => ui.push(TuiEntryKind::System, "usage: /import <path.jsonl>"),
        _ if line.starts_with("/import ") => {
            let path = PathBuf::from(line.trim_start_matches("/import ").trim());
            return ui.switch_session(runtime, config, SessionSwitch::Import(path));
        }
        _ if line.starts_with("/account ") => {
            let account = line.trim_start_matches("/account ").trim();
            runtime.set_active_account(if account.is_empty() {
                None
            } else {
                Some(account.to_string())
            })?;
            ui.push(
                TuiEntryKind::System,
                format!(
                    "account: {}",
                    if account.is_empty() { "auto" } else { account }
                ),
            );
        }
        _ if line.starts_with("/login") => {
            // "/login [provider] [account]": with no provider, the selector;
            // with one, the flow runs at once.
            let rest = line.trim_start_matches("/login").trim();
            let mut parts = rest.split_whitespace();
            let provider = parts.next().unwrap_or_default().to_string();
            let account = parts.next().unwrap_or(DEFAULT_ACCOUNT_NAME).to_string();
            if provider.is_empty() {
                return open_selector(ui, config, runtime, "login", "");
            } else if config.models.iter().any(|model| model.provider == provider) {
                return Ok(CommandOutcome::Login { provider, account });
            } else {
                ui.push(
                    TuiEntryKind::Error,
                    format!("unknown provider: {provider} (see /models)"),
                );
            }
        }
        _ if line.starts_with("/logout") => {
            let args = line.trim_start_matches("/logout").trim();
            if args.is_empty() {
                return open_selector(ui, config, runtime, "logout", "");
            } else {
                let mut parts = args.split_whitespace();
                let provider = parts.next().unwrap_or_default();
                let account = parts.next().unwrap_or(DEFAULT_ACCOUNT_NAME);
                if remove_stored_auth(config, provider, account)? {
                    ui.push(
                        TuiEntryKind::System,
                        format!(
                            "removed stored auth for {provider}{}",
                            account_suffix(account)
                        ),
                    );
                } else {
                    ui.push(
                        TuiEntryKind::System,
                        format!("no stored auth for {provider}{}", account_suffix(account)),
                    );
                }
            }
        }
        _ if line.starts_with("/share") => {
            let requested = line.trim_start_matches("/share").trim();
            let path = if requested.is_empty() {
                config
                    .paths
                    .session_dir
                    .join(format!("{}.html", runtime.session().session_id))
            } else {
                PathBuf::from(requested)
            };
            export_session(runtime, &path)?;
            ui.push(
                TuiEntryKind::System,
                format!("share exported {}", path.display()),
            );
        }
        // An extension-registered command: `/<name> [args]` runs the v2
        // host's registered command (upstream `registerCommand`). Built-ins
        // and resources above win, as upstream's own commands do.
        _ if line.starts_with('/') && !line.starts_with("//") => {
            let (name, input) = split_resource_command(&line, "/");
            let (name, input) = (name.to_string(), input.to_string());
            // A v2 host's registered commands surface only once its
            // handshake ran; hosts start lazily, so an unknown /command
            // starts them once and looks again before falling through to
            // the model (upstream loads extensions at startup).
            let registered = {
                let hosts = runtime.extension_hosts();
                let hosts = hosts.lock().await;
                hosts
                    .registration_sets()
                    .iter()
                    .any(|set| set.commands.iter().any(|command| command.name == name))
            };
            let registered = registered || {
                runtime.start_extension_hosts().await;
                let hosts = runtime.extension_hosts();
                let hosts = hosts.lock().await;
                hosts
                    .registration_sets()
                    .iter()
                    .any(|set| set.commands.iter().any(|command| command.name == name))
            };
            if registered {
                let extension = {
                    let hosts = runtime.extension_hosts();
                    let hosts = hosts.lock().await;
                    hosts
                        .registration_sets()
                        .iter()
                        .find(|set| set.commands.iter().any(|command| command.name == name))
                        .map(|set| set.extension_name.clone())
                };
                if let Some(extension) = extension {
                    let cwd = runtime.session().cwd.clone();
                    let hosts = runtime.extension_hosts();
                    let mut hosts = hosts.lock().await;
                    let outcome = hosts
                        .call(&extension, "command", &name, &input, &cwd, None)
                        .await;
                    drop(hosts);
                    match outcome {
                        Ok(outcome) => {
                            if let Some(set) = outcome.registrations {
                                runtime.apply_extension_registrations(set).await;
                            }
                            let kind = if outcome.is_error {
                                TuiEntryKind::Error
                            } else {
                                TuiEntryKind::System
                            };
                            ui.push(kind, outcome.output);
                        }
                        Err(error) => ui.push(TuiEntryKind::Error, error),
                    }
                    return Ok(CommandOutcome::Handled);
                }
            }
            let prompt = TypedPrompt::expand(config, line);
            return Ok(CommandOutcome::Submit {
                prompt,
                media: Vec::new(),
            });
        }
        // Anything else is a prompt: a skill command or a prompt template
        // expands, and other text goes to the model as typed.
        _ => {
            let prompt = TypedPrompt::expand(config, line);
            return Ok(CommandOutcome::Submit {
                prompt,
                media: Vec::new(),
            });
        }
    }
    Ok(CommandOutcome::Handled)
}
