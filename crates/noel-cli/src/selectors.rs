//! The TUI's selectors: sessions, models, providers, accounts and settings, as rows.

use super::*;

/// One-line session row for the resume picker, in the Kimi/Claude/Codex
/// style: topic preview first, then relative age, message count, model
/// badge, and an unfinished-turn marker so a crashed session stands out.
pub(crate) fn format_session_picker_label(session: &noel_core::SessionSummary) -> String {
    let topic = session
        .first_prompt
        .as_deref()
        .map(|prompt| {
            let line = prompt.lines().next().unwrap_or("").to_string();
            let truncated: String = line.chars().take(60).collect();
            if truncated.chars().count() < line.chars().count() {
                format!("{truncated}…")
            } else {
                truncated
            }
        })
        .filter(|topic| !topic.trim().is_empty())
        .unwrap_or_else(|| {
            session
                .name
                .clone()
                .unwrap_or_else(|| session.session_id.clone())
        });
    let age = session
        .modified
        .map(relative_time_label)
        .unwrap_or_else(|| "-".to_string());
    let model = session.active_model.clone().unwrap_or_else(|| "-".into());
    let unfinished = if session.has_unfinished_turn {
        " · unfinished"
    } else {
        ""
    };
    format!(
        "{topic} · {age} · {} msg · {model}{unfinished}",
        session.message_count
    )
}

/// Compact relative age like Claude's picker: "2m", "3h", "5d".
/// What `/import <words>` names.
pub(crate) enum ImportSource {
    /// A session export or a journal, as typed.
    File(PathBuf),
    /// `claude` alone: the Claude Code sessions of the directory, newest
    /// first, to choose from.
    ClaudeCode(Vec<noel_core::claude_code::ClaudeCodeSession>),
}

/// Reads `/import`'s words: a file, or `claude [session]`, a Claude Code
/// transcript of `cwd` named by its number in the list or by its id. A file
/// called `claude` is `./claude`.
pub(crate) fn import_source(cwd: &Path, words: &str) -> Result<ImportSource> {
    let (first, reference) = split_once_text(words.trim());
    if first != "claude" {
        return Ok(ImportSource::File(PathBuf::from(words.trim())));
    }
    let sessions = noel_core::claude_code::claude_code_projects_dir()
        .map(|projects| noel_core::claude_code::claude_code_sessions(&projects, cwd))
        .unwrap_or_default();
    if sessions.is_empty() {
        return Err(anyhow!("no Claude Code sessions for {}", cwd.display()));
    }
    if reference.is_empty() {
        return Ok(ImportSource::ClaudeCode(sessions));
    }
    noel_core::claude_code::find_claude_code_session(&sessions, reference)
        .map(|session| ImportSource::File(session.path.clone()))
        .map_err(|error| anyhow!(error))
}

/// A Claude Code session as a picker row: its title, its age, its id.
pub(crate) fn claude_code_session_label(
    session: &noel_core::claude_code::ClaudeCodeSession,
) -> String {
    let age = session
        .modified
        .map(relative_time_label)
        .unwrap_or_else(|| "-".to_string());
    let id = session.id.chars().take(8).collect::<String>();
    format!("{} · {age} · {id}", session.title)
}

/// The Claude Code sessions of `cwd` as `/import claude` lists them where
/// there is no picker.
pub(crate) fn format_claude_code_sessions(
    cwd: &Path,
    sessions: &[noel_core::claude_code::ClaudeCodeSession],
) -> String {
    let mut lines = vec![format!(
        "Claude Code sessions for {} (import one by its number or id: import claude <session>):",
        cwd.display()
    )];
    lines.extend(sessions.iter().enumerate().map(|(place, session)| {
        format!("{:>3}. {}", place + 1, claude_code_session_label(session))
    }));
    lines.join("\n")
}

pub(crate) fn relative_time_label(modified: std::time::SystemTime) -> String {
    let Ok(elapsed) = modified.elapsed() else {
        return "now".to_string();
    };
    let secs = elapsed.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

pub(crate) fn selector_for_kind(
    config: &LoadedConfig,
    runtime: &Runtime,
    kind: &str,
) -> Result<Selector> {
    match kind {
        "model" | "models" | "scoped-models" => Ok(Selector::new(
            "model",
            picker_models(config)
                .into_iter()
                .map(|model| {
                    let value = format!("{}/{}", model.provider, model.id);
                    SelectorItem {
                        label: value.clone(),
                        value,
                        active: runtime
                            .session()
                            .active_model
                            .as_ref()
                            .map(|active| {
                                active.provider == model.provider && active.id == model.id
                            })
                            .unwrap_or(false),
                    }
                })
                .collect(),
        )),
        // Codex-style first layer: pick the provider, then its models.
        "provider" => Ok(Selector::new(
            "provider",
            provider_selector_items(
                &picker_models(config),
                runtime
                    .session()
                    .active_model
                    .as_ref()
                    .map(|model| model.provider.as_str()),
            ),
        )),
        "theme" | "themes" => Ok(Selector::new(
            "theme",
            theme_names(config)
                .into_iter()
                .map(|name| SelectorItem {
                    active: config.settings.theme.as_deref().unwrap_or("system") == name
                        || (name == "system"
                            && config.settings.theme.as_deref() == Some("default")),
                    label: name.clone(),
                    value: name,
                })
                .collect(),
        )),
        // The branch navigator: /tree's rows as pickable items; Enter
        // switches to the picked branch (upstream's tree selector).
        "branches" => Ok(Selector::new(
            "branches",
            session_tree_items(config, Some(&runtime.session().session_id))?,
        )),
        "session" | "sessions" | "resume" | "tree" => Ok(Selector::new(
            "session",
            SessionStore::list(&config.paths.session_dir)?
                .into_iter()
                // Journal listing is oldest-first; the picker should open on
                // the most recent session like Kimi/Claude/Codex.
                .rev()
                .map(|session| SelectorItem {
                    label: format_session_picker_label(&session),
                    value: session.session_id.clone(),
                    active: runtime.session().session_id == session.session_id,
                })
                .collect(),
        )),
        "auth" | "login" | "logout" => Ok(Selector::new(
            if kind == "logout" { "logout" } else { "auth" },
            config
                .models
                .iter()
                .map(|model| model.provider.clone())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .map(|provider| SelectorItem {
                    label: match (
                        kind == "logout",
                        auth_for_provider(&config.auth, &provider, None).is_some(),
                    ) {
                        // /logout removes what is there; /login runs the
                        // provider's flow, so the row says which it has.
                        (false, true) => format!("{provider}: available (login replaces)"),
                        (false, false) => format!("{provider}: missing (login)"),
                        (true, true) => format!("{provider}: available"),
                        (true, false) => format!("{provider}: missing"),
                    },
                    value: provider.clone(),
                    active: auth_for_provider(&config.auth, &provider, None).is_some(),
                })
                .collect(),
        )),
        "account" => Ok(Selector::new(
            "account",
            account_selector_items(config, runtime),
        )),
        "settings" => Ok(Selector::new("settings", settings_selector_items(config))),
        "rewind" => Ok(crate::rewind_commands::rewind_selector(runtime)),
        _ => Err(anyhow!("unknown selector: {kind}")),
    }
}

pub(crate) fn sorted_models(models: &[ModelDefinition]) -> Vec<&ModelDefinition> {
    let mut models = models.iter().collect::<Vec<_>>();
    models.sort_by(|left, right| {
        left.provider
            .cmp(&right.provider)
            .then_with(|| left.id.cmp(&right.id))
    });
    models
}

/// The models a model picker lists, by provider and id: the scope's when
/// `--models` or `enabledModels` sets one, else every known model.
pub(crate) fn picker_models(config: &LoadedConfig) -> Vec<&ModelDefinition> {
    if config.model_scope.is_empty() {
        return sorted_models(&config.models);
    }
    let mut models = config
        .model_scope
        .iter()
        .filter_map(|scoped| configured_model(config, &scoped.model))
        .collect::<Vec<_>>();
    models.sort_by(|left, right| {
        left.provider
            .cmp(&right.provider)
            .then_with(|| left.id.cmp(&right.id))
    });
    models
}

/// Provider rows for the first layer of the model picker: provider name,
/// model count, and whether the active model belongs to it.
pub(crate) fn provider_selector_items(
    models: &[&ModelDefinition],
    active_provider: Option<&str>,
) -> Vec<SelectorItem> {
    let mut providers: BTreeMap<String, usize> = BTreeMap::new();
    for model in models {
        *providers.entry(model.provider.clone()).or_default() += 1;
    }
    providers
        .into_iter()
        .map(|(provider, count)| {
            let active_provider = active_provider == Some(provider.as_str());
            SelectorItem {
                label: format!(
                    "{provider} · {count} model{}{}",
                    if count == 1 { "" } else { "s" },
                    if active_provider { " · active" } else { "" }
                ),
                value: provider,
                active: active_provider,
            }
        })
        .collect()
}

pub(crate) fn account_selector_items(
    config: &LoadedConfig,
    runtime: &Runtime,
) -> Vec<SelectorItem> {
    let Some(model) = runtime.session().active_model.clone() else {
        return Vec::new();
    };
    account_selector_items_for(
        config,
        &model.provider,
        runtime.session().active_account.as_deref(),
    )
}

/// [`account_selector_items`] without the runtime, for the picker opened
/// mid-turn while a streaming turn borrows it: the provider of the active
/// model and the session's pinned account come from the app's chrome.
pub(crate) fn account_selector_items_for(
    config: &LoadedConfig,
    provider: &str,
    active_account: Option<&str>,
) -> Vec<SelectorItem> {
    let failover = failover_enabled_for(config, provider);
    let pinned_note = if failover {
        " (pins account; failover disabled while pinned)"
    } else {
        ""
    };
    let active = active_account;
    // Auth is read from disk, not the startup snapshot: an account logged
    // in or refreshed by another process (the CLI, the TUI's own /login)
    // appears and reads current without a /reload. A missing file keeps
    // the snapshot (nothing was ever stored; the snapshot may hold
    // process-only state a unit harness built).
    let auth = if config.paths.auth_path.exists() {
        noel_config::read_auth_file(&config.paths.auth_path).unwrap_or_else(|_| config.auth.clone())
    } else {
        config.auth.clone()
    };
    let mut items = vec![SelectorItem {
        label: if failover {
            "auto (failover: sticky account, switches on limits)".to_string()
        } else {
            "auto (default resolution)".to_string()
        },
        value: String::new(),
        active: active.is_none(),
    }];
    items.extend(
        listed_accounts_for_provider(&auth, provider)
            .into_iter()
            .map(|(account, source)| {
                // The token's own expiry state, so a stale snapshot cannot
                // call a refreshed account expired (the request path
                // refreshes within its window before use anyway).
                let state = match &source {
                    AccountSource::Stored => {
                        let expiring = auth
                            .credential(provider, &account)
                            .and_then(|credential| match credential {
                                AuthCredential::OAuth { expires, .. } => Some(*expires),
                                _ => None,
                            })
                            .map(|expires| {
                                expires
                                    <= unix_seconds().unwrap_or_default()
                                        + crate::oauth_refresh::REFRESH_WINDOW_SECONDS
                            })
                            .unwrap_or(false);
                        if expiring {
                            format!("{account} (expires soon; refreshed before use){pinned_note}")
                        } else {
                            format!("{account}{pinned_note}")
                        }
                    }
                    AccountSource::Environment => {
                        format!("{account} (environment, read-only)")
                    }
                    AccountSource::ImportedFile => {
                        format!("{account} (imported login file, read-only)")
                    }
                };
                SelectorItem {
                    label: state,
                    active: active == Some(account.as_str()),
                    value: account,
                }
            }),
    );
    items
}

pub(crate) fn settings_selector_items(config: &LoadedConfig) -> Vec<SelectorItem> {
    vec![
        bool_setting_item(
            "auto compact",
            "compaction.enabled",
            auto_compaction_enabled(config),
        ),
        bool_setting_item(
            "show images",
            "terminal.showImages",
            config
                .settings
                .terminal
                .as_ref()
                .and_then(|terminal| terminal.show_images)
                .unwrap_or(false),
        ),
        bool_setting_item(
            "terminal progress",
            "terminal.showTerminalProgress",
            terminal_progress_enabled(config),
        ),
        bool_setting_item(
            "auto resize images",
            "images.autoResize",
            images_auto_resize(config),
        ),
        bool_setting_item("block images", "images.blockImages", images_blocked(config)),
        bool_setting_item(
            "provider retry",
            "retry.enabled",
            config
                .settings
                .retry
                .as_ref()
                .and_then(|retry| retry.enabled)
                .unwrap_or(true),
        ),
        bool_setting_item(
            "background model refresh",
            "modelRefresh.enabled",
            model_refresh_enabled(&config.settings),
        ),
        bool_setting_item(
            "hide thinking block",
            "hideThinkingBlock",
            config.settings.hide_thinking_block.unwrap_or(false),
        ),
        bool_setting_item(
            "Anthropic extra usage warning",
            "warnings.anthropicExtraUsage",
            config
                .settings
                .warnings
                .as_ref()
                .and_then(|warnings| warnings.anthropic_extra_usage)
                .unwrap_or(true),
        ),
        // follow-up mode selector removed: queued bursts always dispatch as
        // one combined turn; there is no mode to choose anymore.
    ]
}

pub(crate) fn bool_setting_item(label: &str, value: &str, active: bool) -> SelectorItem {
    SelectorItem {
        label: format!("{label}: {}", on_off(active)),
        value: value.to_string(),
        active,
    }
}

pub(crate) fn on_off(value: bool) -> &'static str {
    if value {
        "on"
    } else {
        "off"
    }
}
