use super::*;
use std::sync::Mutex;

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn optional_u64_object(fields: &[(&str, Option<u64>)]) -> serde_json::Value {
    let mut object = serde_json::Map::new();
    for (key, value) in fields {
        if let Some(value) = value {
            object.insert((*key).to_string(), serde_json::json!(value));
        }
    }
    serde_json::Value::Object(object)
}

fn provider_retry_summary(settings: &Settings) -> serde_json::Value {
    let provider = settings
        .retry
        .as_ref()
        .and_then(|retry| retry.provider.as_ref());
    let mut object = serde_json::Map::new();
    if let Some(timeout_ms) = provider.and_then(|provider| provider.timeout_ms) {
        object.insert("timeoutMs".to_string(), serde_json::json!(timeout_ms));
    }
    if let Some(max_retries) = provider.and_then(|provider| provider.max_retries) {
        object.insert("maxRetries".to_string(), serde_json::json!(max_retries));
    }
    object.insert(
        "maxRetryDelayMs".to_string(),
        serde_json::json!(provider
            .and_then(|provider| provider.max_retry_delay_ms)
            .unwrap_or(60000)),
    );
    serde_json::Value::Object(object)
}

fn thinking_budgets_summary(settings: &Settings) -> serde_json::Value {
    let Some(thinking) = settings.thinking_budgets.as_ref() else {
        return serde_json::Value::Null;
    };
    optional_u64_object(&[
        ("minimal", thinking.minimal),
        ("low", thinking.low),
        ("medium", thinking.medium),
        ("high", thinking.high),
    ])
}

fn settings_summary(settings: &Settings) -> serde_json::Value {
    let compaction = settings.compaction.as_ref();
    let retry = settings.retry.as_ref();
    let terminal = settings.terminal.as_ref();
    let images = settings.images.as_ref();
    serde_json::json!({
        "defaultProvider": settings.default_provider.as_deref(),
        "defaultModel": settings.default_model.as_deref(),
        "defaultThinkingLevel": settings.default_thinking_level.as_deref(),
        "transport": settings.transport.as_deref().unwrap_or("auto"),
        "steeringMode": settings.steering_mode.as_deref().unwrap_or("one-at-a-time"),
        "followUpMode": settings.follow_up_mode.as_deref().unwrap_or("one-at-a-time"),
        "enabledModels": settings.enabled_models.as_ref(),
        // Resolved against upstream's default tools, which the fixture
        // records; noel's own default set is larger.
        "defaultTools": settings.default_tools.as_ref().map(|entries| {
            let upstream_defaults = ["read", "bash", "edit", "write"].map(String::from);
            resolve_default_tools(entries, &upstream_defaults)
        }),
        "compaction": {
            "enabled": compaction.and_then(|compaction| compaction.enabled).unwrap_or(true),
            "reserveTokens": compaction.and_then(|compaction| compaction.reserve_tokens).unwrap_or(16384),
            "keepRecentTokens": compaction.and_then(|compaction| compaction.keep_recent_tokens).unwrap_or(20000),
        },
        "retry": {
            "enabled": retry.and_then(|retry| retry.enabled).unwrap_or(true),
            "maxRetries": retry.and_then(|retry| retry.max_retries).unwrap_or(3),
            "baseDelayMs": retry.and_then(|retry| retry.base_delay_ms).unwrap_or(2000),
            "maxAgentDelayMs": retry.and_then(|retry| retry.max_agent_delay_ms).unwrap_or(60000),
        },
        "providerRetry": provider_retry_summary(settings),
        "terminal": {
            "showImages": terminal.and_then(|terminal| terminal.show_images).unwrap_or(true),
            "imageWidthCells": terminal.and_then(|terminal| terminal.image_width_cells).unwrap_or(60),
            "clearOnShrink": terminal.and_then(|terminal| terminal.clear_on_shrink).unwrap_or(false),
            "showTerminalProgress": terminal.and_then(|terminal| terminal.show_terminal_progress).unwrap_or(false),
        },
        "images": {
            "autoResize": images.and_then(|images| images.auto_resize).unwrap_or(true),
            "blockImages": images.and_then(|images| images.block_images).unwrap_or(false),
        },
        "thinkingBudgets": thinking_budgets_summary(settings),
        "doubleEscapeAction": settings.double_escape_action.as_deref().unwrap_or("tree"),
        "treeFilterMode": settings.tree_filter_mode.as_deref().unwrap_or("default"),
        "quietStartup": settings.quiet_startup.unwrap_or(false),
        "hideThinkingBlock": settings.hide_thinking_block.unwrap_or(false),
    })
}

#[test]
fn compaction_settings_merge_and_model_windows_accept_old_cache_and_custom_overrides() {
    let global: Settings = serde_json::from_value(serde_json::json!({
        "compaction":{"enabled":true,"reserveTokens":2048,"triggerPercent":75}
    }))
    .unwrap();
    let project: Settings = serde_json::from_value(serde_json::json!({
        "compaction":{"keepRecentTokens":6000,"triggerPercent":80}
    }))
    .unwrap();
    let settings = global.merge(project).compaction.unwrap();
    assert_eq!(settings.enabled, Some(true));
    assert_eq!(settings.reserve_tokens, Some(2048));
    assert_eq!(settings.keep_recent_tokens, Some(6000));
    assert_eq!(settings.trigger_percent, Some(80));
    // An entry without a window (an old cache) takes the catalog's.
    let builtin = noel_ai::catalog::provider_base_model("openai").expect("built-in model");
    let old = ModelDefinition {
        provider: builtin.provider.clone(),
        id: builtin.id.clone(),
        ..ModelDefinition::default()
    };
    assert_eq!(model_context_window(&old), builtin.context_window);
    let mut custom: ModelDefinition =
        serde_json::from_str(r#"{"provider":"local","id":"test","contextWindow":8192}"#).unwrap();
    assert_eq!(model_context_window(&custom), 8192);
    custom.context_window = None;
    assert_eq!(model_context_window(&custom), 128_000);
    custom.context_window = Some(0);
    assert_eq!(model_context_window(&custom), 128_000);
}

#[test]
fn load_config_accepts_keybinding_map_and_keeps_every_model_under_a_scope() {
    let root = test_dir("noel-config-load");
    let agent_dir = root.join("agent");
    let cwd = root.join("repo");
    fs::create_dir_all(&agent_dir).expect("create agent dir");
    fs::create_dir_all(&cwd).expect("create cwd");
    fs::write(
        agent_dir.join("settings.json"),
        r#"{"enabledModels":["faux/echo","named"],"systemPrompt":"prompt"}"#,
    )
    .expect("write settings");
    fs::write(
        agent_dir.join("models.json"),
        r#"[
                {"provider":"faux","id":"echo","api":"faux"},
                {"provider":"openai","id":"gpt-test","name":"named","api":"open-ai"},
                {"provider":"anthropic","id":"claude-test","api":"anthropic"}
            ]"#,
    )
    .expect("write models");
    fs::write(
        agent_dir.join("keybindings.json"),
        r#"{"submit":["enter"],"cancel":["escape"]}"#,
    )
    .expect("write keybindings");
    fs::create_dir_all(agent_dir.join("skills/review")).expect("create skills");
    fs::create_dir_all(agent_dir.join("prompts")).expect("create prompts");
    fs::create_dir_all(agent_dir.join("extensions")).expect("create extensions");
    fs::create_dir_all(cwd.join(".noel/themes")).expect("create project themes");
    fs::write(
        agent_dir.join("skills/review/SKILL.md"),
        "---\ndescription: Review code.\n---\nreview skill",
    )
    .expect("write skill");
    fs::write(agent_dir.join("prompts/fix.md"), "fix {{input}}").expect("write prompt");
    fs::write(agent_dir.join("extensions/plan.md"), "plan extension").expect("write extension");
    fs::write(cwd.join(".noel/themes/dark.json"), r#"{"name":"dark"}"#).expect("write theme");

    let config = load_config(ConfigPaths {
        cwd: cwd.clone(),
        agent_dir: agent_dir.clone(),
        session_dir: agent_dir.join("sessions"),
        settings_path: agent_dir.join("settings.json"),
        project_settings_path: root.join("missing-project-settings.json"),
        auth_path: root.join("missing-auth.json"),
        models_path: agent_dir.join("models.json"),
        model_cache_path: agent_dir.join("model-cache.json"),
        keybindings_path: agent_dir.join("keybindings.json"),
    })
    .expect("load config");

    assert_eq!(config.system_prompt, Some("prompt".to_string()));
    // `enabledModels` scopes selection; it removes no model.
    let has = |provider: &str, id: &str| {
        config
            .models
            .iter()
            .any(|model| model.provider == provider && model.id == id)
    };
    assert!(has("faux", "echo"));
    assert!(has("openai", "gpt-test") && has("anthropic", "claude-test"));
    let builtin = noel_ai::catalog::provider_base_model("anthropic").expect("built-in model");
    assert!(has("anthropic", &builtin.id));
    let scope = resolve_model_scope(
        config.settings.enabled_models.as_deref().unwrap(),
        &config.models.iter().collect::<Vec<_>>(),
    );
    assert_eq!(
        scope
            .models
            .iter()
            .map(|scoped| format!("{}/{}", scoped.model.provider, scoped.model.id))
            .collect::<Vec<_>>(),
        ["faux/echo", "openai/gpt-test"]
    );
    assert!(scope.diagnostics.is_empty(), "{:?}", scope.diagnostics);
    assert_eq!(config.keybindings.len(), 2);
    assert!(config
        .keybindings
        .iter()
        .any(|binding| binding.action == "submit" && binding.keys == ["enter"]));
    assert_eq!(config.extensions[0].name, "plan");
    assert_eq!(config.extensions[0].content, "plan extension");
    assert_eq!(config.skills[0].name, "review");
    assert_eq!(config.skills[0].description, "Review code.");
    assert_eq!(config.prompt_templates[0].name, "fix");
    assert_eq!(config.prompt_templates[0].content, "fix {{input}}");
    assert_eq!(config.themes[0].name, "dark");
    assert!(config.diagnostics.is_empty(), "{:?}", config.diagnostics);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn settings_and_local_packages_extend_resource_paths() {
    let root = test_dir("noel-config-package-resources");
    let agent_dir = root.join("agent");
    let cwd = root.join("repo");
    let package_dir = cwd.join("package");
    fs::create_dir_all(agent_dir.join("custom-extensions")).expect("create extensions");
    fs::create_dir_all(cwd.join(".noel/custom-prompts")).expect("create prompts");
    fs::create_dir_all(package_dir.join("extensions")).expect("create package extensions");
    fs::create_dir_all(package_dir.join("skills")).expect("create package skills");
    fs::write(
        agent_dir.join("settings.json"),
        r#"{"extensions":["custom-extensions"],"packages":["package"]}"#,
    )
    .expect("write user settings");
    fs::write(
        cwd.join(".noel/settings.json"),
        r#"{"prompts":["custom-prompts"]}"#,
    )
    .expect("write project settings");
    fs::write(
        agent_dir.join("custom-extensions/global.md"),
        "global extension",
    )
    .expect("write global extension");
    fs::write(cwd.join(".noel/custom-prompts/local.md"), "local prompt")
        .expect("write local prompt");
    fs::write(
        package_dir.join("package.json"),
        r#"{"noel":{"extensions":["extensions/pkg.md"],"skills":["skills/pkg.md"]}}"#,
    )
    .expect("write package manifest");
    fs::write(package_dir.join("extensions/pkg.md"), "package extension")
        .expect("write package extension");
    fs::write(
        package_dir.join("skills/pkg.md"),
        "---\nname: pkg\ndescription: A package skill.\n---\npackage skill",
    )
    .expect("write package skill");

    let config = load_config(ConfigPaths {
        cwd: cwd.clone(),
        agent_dir: agent_dir.clone(),
        session_dir: agent_dir.join("sessions"),
        settings_path: agent_dir.join("settings.json"),
        project_settings_path: cwd.join(".noel/settings.json"),
        auth_path: root.join("missing-auth.json"),
        models_path: root.join("missing-models.json"),
        model_cache_path: root.join("missing-model-cache.json"),
        keybindings_path: root.join("missing-keybindings.json"),
    })
    .expect("load config");

    assert!(config
        .extensions
        .iter()
        .any(|resource| resource.name == "global" && resource.content == "global extension"));
    assert!(config
        .extensions
        .iter()
        .any(|resource| resource.name == "pkg" && resource.content == "package extension"));
    assert!(config
        .skills
        .iter()
        .any(|skill| skill.name == "pkg" && skill.source_info.from_package));
    assert!(config
        .prompt_templates
        .iter()
        .any(|resource| resource.name == "local" && resource.content == "local prompt"));
    assert!(config.diagnostics.is_empty(), "{:?}", config.diagnostics);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn package_object_filters_resource_paths() {
    let root = test_dir("noel-config-package-filters");
    let agent_dir = root.join("agent");
    let cwd = root.join("repo");
    let package_dir = cwd.join("package");
    fs::create_dir_all(&agent_dir).expect("create agent dir");
    fs::create_dir_all(package_dir.join("extensions")).expect("create package extensions");
    fs::create_dir_all(package_dir.join("skills")).expect("create package skills");
    fs::create_dir_all(package_dir.join("prompts")).expect("create package prompts");
    fs::create_dir_all(package_dir.join("themes")).expect("create package themes");
    fs::write(
        agent_dir.join("settings.json"),
        r#"{
                "packages":[{
                    "source":"package",
                    "extensions":[
                        "extensions/*.md",
                        "!extensions/skip.md",
                        "+extensions/force.txt",
                        "-extensions/drop.md"
                    ],
                    "skills":[],
                    "prompts":["prompts/review.md"],
                    "themes":[]
                }]
            }"#,
    )
    .expect("write settings");
    fs::write(package_dir.join("extensions/keep.md"), "keep extension")
        .expect("write keep extension");
    fs::write(package_dir.join("extensions/skip.md"), "skip extension")
        .expect("write skip extension");
    fs::write(package_dir.join("extensions/drop.md"), "drop extension")
        .expect("write drop extension");
    fs::write(package_dir.join("extensions/force.txt"), "force extension")
        .expect("write force extension");
    fs::write(package_dir.join("extensions/other.txt"), "other extension")
        .expect("write other extension");
    fs::write(package_dir.join("skills/pkg.md"), "package skill").expect("write skill");
    fs::write(package_dir.join("prompts/review.md"), "review prompt").expect("write prompt");
    fs::write(package_dir.join("prompts/other.md"), "other prompt").expect("write prompt");
    fs::write(package_dir.join("themes/dark.json"), r#"{"name":"dark"}"#).expect("write theme");

    let config = load_config_hermetic(
        ConfigPaths {
            cwd: cwd.clone(),
            agent_dir: agent_dir.clone(),
            session_dir: agent_dir.join("sessions"),
            settings_path: agent_dir.join("settings.json"),
            project_settings_path: cwd.join(".noel/settings.json"),
            auth_path: root.join("missing-auth.json"),
            models_path: root.join("missing-models.json"),
            model_cache_path: root.join("missing-model-cache.json"),
            keybindings_path: root.join("missing-keybindings.json"),
        },
        true,
    )
    .expect("load config");

    assert!(config
        .extensions
        .iter()
        .any(|resource| resource.name == "keep" && resource.content == "keep extension"));
    assert!(config
        .extensions
        .iter()
        .any(|resource| resource.name == "force" && resource.content == "force extension"));
    assert!(!config
        .extensions
        .iter()
        .any(|resource| resource.name == "skip"));
    assert!(!config
        .extensions
        .iter()
        .any(|resource| resource.name == "drop"));
    assert!(!config
        .extensions
        .iter()
        .any(|resource| resource.name == "other"));
    assert!(config.skills.is_empty());
    assert_eq!(config.prompt_templates.len(), 1);
    assert_eq!(config.prompt_templates[0].name, "review");
    assert!(config.themes.is_empty());
    assert!(config.diagnostics.is_empty());

    let _ = fs::remove_dir_all(root);
}

#[test]
fn agents_skill_dirs_load_skill_md_by_directory_name() {
    let root = test_dir("noel-config-agents-skills");
    let agent_dir = root.join("agent");
    let repo = root.join("repo");
    let cwd = repo.join("nested");
    fs::create_dir_all(&agent_dir).expect("create agent dir");
    fs::create_dir_all(cwd.join(".agents/skills/local")).expect("create local skill");
    fs::create_dir_all(repo.join(".agents/skills/repo")).expect("create repo skill");
    fs::write(repo.join(".git"), "").expect("mark git root");
    fs::write(
        cwd.join(".agents/skills/local/SKILL.md"),
        "---\ndescription: The local skill.\n---\nlocal skill body",
    )
    .expect("write local skill");
    fs::write(
        repo.join(".agents/skills/repo/SKILL.md"),
        "---\ndescription: The repo skill.\n---\nrepo skill body",
    )
    .expect("write repo skill");
    fs::write(
        cwd.join(".agents/skills/ignored.md"),
        "---\nname: ignored\ndescription: Directly in .agents/skills.\n---\n",
    )
    .expect("write ignored root markdown");

    let config = load_config_hermetic(
        ConfigPaths {
            cwd: cwd.clone(),
            agent_dir: agent_dir.clone(),
            session_dir: agent_dir.join("sessions"),
            settings_path: agent_dir.join("settings.json"),
            project_settings_path: cwd.join(".noel/settings.json"),
            auth_path: root.join("missing-auth.json"),
            models_path: root.join("missing-models.json"),
            model_cache_path: root.join("missing-model-cache.json"),
            keybindings_path: root.join("missing-keybindings.json"),
        },
        true,
    )
    .expect("load config");

    // Nearest first; a Markdown file directly in .agents/skills is no skill.
    let names = config.skills.iter().map(|skill| skill.name.as_str());
    assert_eq!(names.collect::<Vec<_>>(), ["local", "repo"]);
    assert_eq!(config.skills[1].description, "The repo skill.");
    assert_eq!(config.skills[1].base_dir, repo.join(".agents/skills/repo"));
    assert!(config.diagnostics.is_empty());

    let _ = fs::remove_dir_all(root);
}

#[test]
fn untrusted_projects_load_context_files_but_not_project_resources() {
    let root = test_dir("noel-config-untrusted-context");
    let agent_dir = root.join("agent");
    let repo = root.join("repo");
    let cwd = repo.join("nested");
    fs::create_dir_all(&agent_dir).expect("create agent dir");
    fs::create_dir_all(cwd.join(".noel/extensions")).expect("create project extensions");
    fs::create_dir_all(repo.join(".agents/skills/repo")).expect("create repo skill");
    fs::write(repo.join(".git"), "").expect("mark git root");
    fs::write(agent_dir.join("AGENTS.md"), "global rules").expect("write global");
    fs::write(repo.join("AGENTS.md"), "repo rules").expect("write repo rules");
    fs::write(cwd.join("CLAUDE.md"), "nested rules").expect("write nested rules");
    fs::write(
        cwd.join(".noel/settings.json"),
        r#"{"shellCommandPrefix":"echo untrusted"}"#,
    )
    .expect("write project settings");
    fs::write(cwd.join(".noel/extensions/local.md"), "project extension")
        .expect("write project extension");
    fs::write(
        repo.join(".agents/skills/repo/SKILL.md"),
        "---\ndescription: The repo skill.\n---\nrepo skill",
    )
    .expect("write repo skill");

    let config = load_config_hermetic(
        ConfigPaths {
            cwd: cwd.clone(),
            agent_dir: agent_dir.clone(),
            session_dir: agent_dir.join("sessions"),
            settings_path: agent_dir.join("settings.json"),
            project_settings_path: cwd.join(".noel/settings.json"),
            auth_path: root.join("missing-auth.json"),
            models_path: root.join("missing-models.json"),
            model_cache_path: root.join("missing-model-cache.json"),
            keybindings_path: root.join("missing-keybindings.json"),
        },
        false,
    )
    .expect("load config");

    let contents = config
        .context_files
        .iter()
        .map(|file| file.content.as_str())
        .collect::<Vec<_>>();
    assert_eq!(contents, ["global rules", "repo rules", "nested rules"]);
    assert_eq!(config.settings.shell_command_prefix, None);
    assert!(config.extensions.is_empty());
    assert!(config.skills.is_empty());

    let _ = fs::remove_dir_all(root);
}

#[test]
fn prompt_settings_come_before_discovered_prompt_files() {
    let root = test_dir("noel-config-prompt-files");
    let agent_dir = root.join("agent");
    let cwd = root.join("repo");
    fs::create_dir_all(&agent_dir).expect("create agent dir");
    fs::create_dir_all(&cwd).expect("create cwd");
    fs::write(agent_dir.join("SYSTEM.md"), "file system").expect("write SYSTEM.md");
    fs::write(agent_dir.join("APPEND_SYSTEM.md"), "file append").expect("write append");
    let load = || {
        load_config(ConfigPaths {
            cwd: cwd.clone(),
            agent_dir: agent_dir.clone(),
            session_dir: agent_dir.join("sessions"),
            settings_path: agent_dir.join("settings.json"),
            project_settings_path: cwd.join(".noel/settings.json"),
            auth_path: root.join("missing-auth.json"),
            models_path: root.join("missing-models.json"),
            model_cache_path: root.join("missing-model-cache.json"),
            keybindings_path: root.join("missing-keybindings.json"),
        })
        .expect("load config")
    };

    let config = load();
    assert_eq!(config.system_prompt.as_deref(), Some("file system"));
    assert_eq!(config.append_system_prompt, ["file append"]);

    // The settings are explicit, as upstream's flags are: each replaces
    // its discovered file.
    fs::write(
        agent_dir.join("settings.json"),
        r#"{"systemPrompt":"setting system","appendSystemPrompt":["one","two"]}"#,
    )
    .expect("write settings");
    let config = load();
    assert_eq!(config.system_prompt.as_deref(), Some("setting system"));
    assert_eq!(config.append_system_prompt, ["one", "two"]);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn a_context_file_that_is_not_utf8_still_loads() {
    let root = test_dir("noel-config-context-bytes");
    let agent_dir = root.join("agent");
    fs::create_dir_all(&agent_dir).expect("create agent dir");
    fs::write(agent_dir.join("AGENTS.md"), b"caf\xe9 rules").expect("write context file");

    let mut diagnostics = Vec::new();
    let files = load_context_files(&root, &agent_dir, &mut diagnostics);
    let contents = files
        .iter()
        .filter(|file| file.path.starts_with(&root))
        .map(|file| file.content.as_str())
        .collect::<Vec<_>>();
    assert_eq!(contents, ["caf\u{fffd} rules"]);
    assert!(diagnostics.is_empty(), "{diagnostics:?}");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn resource_discovery_honors_ignore_files() {
    let root = test_dir("noel-config-resource-ignore");
    let agent_dir = root.join("agent");
    let cwd = root.join("repo");
    let package_dir = cwd.join("package");
    fs::create_dir_all(&agent_dir).expect("create agent dir");
    fs::create_dir_all(package_dir.join("extensions")).expect("create extensions");
    fs::create_dir_all(package_dir.join("skills")).expect("create skills");
    fs::create_dir_all(package_dir.join("prompts/ignored-dir")).expect("create prompts");
    fs::create_dir_all(package_dir.join("themes")).expect("create themes");
    fs::write(
        agent_dir.join("settings.json"),
        r#"{"packages":["package"]}"#,
    )
    .expect("write settings");
    fs::write(
        package_dir.join("prompts/.ignore"),
        "skip.md\nignored-dir/\n!keep.md\n",
    )
    .expect("write ignore");
    fs::write(package_dir.join("prompts/keep.md"), "keep prompt").expect("write keep prompt");
    fs::write(package_dir.join("prompts/skip.md"), "skip prompt").expect("write skip prompt");
    fs::write(
        package_dir.join("prompts/ignored-dir/nested.md"),
        "nested prompt",
    )
    .expect("write nested prompt");

    let config = load_config(ConfigPaths {
        cwd: cwd.clone(),
        agent_dir: agent_dir.clone(),
        session_dir: agent_dir.join("sessions"),
        settings_path: agent_dir.join("settings.json"),
        project_settings_path: cwd.join(".noel/settings.json"),
        auth_path: root.join("missing-auth.json"),
        models_path: root.join("missing-models.json"),
        model_cache_path: root.join("missing-model-cache.json"),
        keybindings_path: root.join("missing-keybindings.json"),
    })
    .expect("load config");

    assert!(config
        .prompt_templates
        .iter()
        .any(|resource| resource.name == "keep"));
    assert!(!config
        .prompt_templates
        .iter()
        .any(|resource| resource.name == "skip"));
    assert!(!config
        .prompt_templates
        .iter()
        .any(|resource| resource.name == "nested"));
    assert!(config.diagnostics.is_empty(), "{:?}", config.diagnostics);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn disabled_resources_are_filtered_from_loaded_config() {
    let root = test_dir("noel-config-disabled-resources");
    let agent_dir = root.join("agent");
    let cwd = root.join("repo");
    fs::create_dir_all(agent_dir.join("extensions")).expect("create extensions");
    fs::create_dir_all(agent_dir.join("prompts")).expect("create prompts");
    fs::write(
        agent_dir.join("settings.json"),
        r#"{"disabledResources":{"extensions":["assist"],"prompts":["prompt:fix"]}}"#,
    )
    .expect("write settings");
    fs::write(agent_dir.join("extensions/assist.md"), "assist extension").expect("write assist");
    fs::write(agent_dir.join("extensions/keep.md"), "keep extension").expect("write keep");
    fs::write(agent_dir.join("prompts/fix.md"), "fix prompt").expect("write prompt");

    let config = load_config(ConfigPaths {
        cwd: cwd.clone(),
        agent_dir: agent_dir.clone(),
        session_dir: agent_dir.join("sessions"),
        settings_path: agent_dir.join("settings.json"),
        project_settings_path: cwd.join(".noel/settings.json"),
        auth_path: root.join("missing-auth.json"),
        models_path: root.join("missing-models.json"),
        model_cache_path: root.join("missing-model-cache.json"),
        keybindings_path: root.join("missing-keybindings.json"),
    })
    .expect("load config");

    assert!(!config
        .extensions
        .iter()
        .any(|resource| resource.name == "assist"));
    assert!(config
        .extensions
        .iter()
        .any(|resource| resource.name == "keep"));
    assert!(config.prompt_templates.is_empty());
    assert!(config.diagnostics.is_empty(), "{:?}", config.diagnostics);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn resource_name_collisions_emit_diagnostics() {
    let root = test_dir("noel-config-resource-collisions");
    let agent_dir = root.join("agent");
    let cwd = root.join("repo");
    fs::create_dir_all(agent_dir.join("extensions")).expect("create user extensions");
    fs::create_dir_all(cwd.join(".noel/extensions")).expect("create project extensions");
    fs::write(agent_dir.join("extensions/dup.md"), "user extension").expect("write user extension");
    fs::write(cwd.join(".noel/extensions/dup.md"), "project extension")
        .expect("write project extension");

    let config = load_config(ConfigPaths {
        cwd,
        agent_dir: agent_dir.clone(),
        session_dir: agent_dir.join("sessions"),
        settings_path: agent_dir.join("settings.json"),
        project_settings_path: root.join("repo/.noel/settings.json"),
        auth_path: root.join("missing-auth.json"),
        models_path: root.join("missing-models.json"),
        model_cache_path: root.join("missing-model-cache.json"),
        keybindings_path: root.join("missing-keybindings.json"),
    })
    .expect("load config");

    assert!(config
        .extensions
        .iter()
        .any(|resource| { resource.name == "dup" && resource.content == "project extension" }));
    assert!(
        config
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.contains("resource name collision for dup")),
        "{:?}",
        config.diagnostics
    );

    let _ = fs::remove_dir_all(root);
}

/// Runs `test` with no provider base-URL variable ([`BASE_URL_ENV`]) set,
/// so models keep the addresses the test gives them wherever it runs.
fn without_base_url_env(test: impl FnOnce()) {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let saved = BASE_URL_ENV.map(|(_, name)| save_env(name));
    for (_, name) in BASE_URL_ENV {
        env::remove_var(name);
    }
    test();
    for (name, value) in saved {
        restore_env(name, value);
    }
}

#[test]
fn model_cache_extends_builtin_and_explicit_models() {
    without_base_url_env(model_cache_extends_builtin_and_explicit_models_here);
}

fn model_cache_extends_builtin_and_explicit_models_here() {
    let root = test_dir("noel-config-model-cache");
    let agent_dir = root.join("agent");
    let cwd = root.join("repo");
    fs::create_dir_all(&agent_dir).expect("create agent dir");
    fs::create_dir_all(&cwd).expect("create cwd");
    fs::write(
            agent_dir.join("models.json"),
            r#"[
                {"provider":"anthropic","id":"claude-custom","name":"Custom Claude","api":"anthropic"}
            ]"#,
        )
        .expect("write explicit models");
    let builtin = noel_ai::catalog::provider_base_model("anthropic").expect("built-in model");
    write_model_cache(
        &agent_dir.join("model-cache.json"),
        &ModelCache {
            refreshed_at: 123,
            version: MODEL_CACHE_VERSION,
            claude_code_version: None,
            models: vec![
                // A listing of a model the catalog lacks, as a refresh
                // writes it: no URL, the provider's name for it.
                ModelDefinition {
                    provider: "anthropic".to_string(),
                    id: "claude-listed-only".to_string(),
                    name: Some("Claude Listed Only".to_string()),
                    api: ProviderApi::Anthropic,
                    ..ModelDefinition::default()
                },
                // A listing of a model the catalog has.
                ModelDefinition {
                    provider: "anthropic".to_string(),
                    id: builtin.id.clone(),
                    name: Some("Listed Under Another Name".to_string()),
                    api: ProviderApi::Anthropic,
                    ..ModelDefinition::default()
                },
                // A provider the catalog lacks.
                ModelDefinition {
                    provider: "elsewhere".to_string(),
                    id: "model".to_string(),
                    api: ProviderApi::OpenAi,
                    base_url: Some("https://elsewhere.example/v1".to_string()),
                    ..ModelDefinition::default()
                },
            ],
            diagnostics: vec!["model refresh warning".to_string()],
        },
    )
    .expect("write model cache");

    let config = load_config(ConfigPaths {
        cwd,
        agent_dir: agent_dir.clone(),
        session_dir: agent_dir.join("sessions"),
        settings_path: root.join("missing-settings.json"),
        project_settings_path: root.join("missing-project-settings.json"),
        auth_path: root.join("missing-auth.json"),
        models_path: agent_dir.join("models.json"),
        model_cache_path: agent_dir.join("model-cache.json"),
        keybindings_path: root.join("missing-keybindings.json"),
    })
    .expect("load config");

    let model_ids = config
        .models
        .iter()
        .map(|model| format!("{}/{}", model.provider, model.id))
        .collect::<Vec<_>>();
    assert!(model_ids.contains(&"faux/echo".to_string()));
    assert!(model_ids.contains(&"faux/script".to_string()));
    assert!(model_ids.contains(&"anthropic/claude-listed-only".to_string()));
    assert!(model_ids.contains(&"anthropic/claude-custom".to_string()));
    assert_eq!(config.diagnostics, ["model refresh warning"]);

    let model = |provider: &str, id: &str| {
        config
            .models
            .iter()
            .find(|model| model.provider == provider && model.id == id)
            .unwrap_or_else(|| panic!("{provider}/{id}"))
    };
    let default = model("anthropic", &builtin.id);
    // A listed model the catalog has is the catalog's entry.
    assert_eq!(default, &ModelDefinition::from(builtin));
    // A listed id the catalog lacks is built onto the provider's
    // default model and keeps its listed name.
    let listed = model("anthropic", "claude-listed-only");
    assert_eq!(listed.name.as_deref(), Some("Claude Listed Only"));
    assert_eq!(
        ModelDefinition {
            id: default.id.clone(),
            name: default.name.clone(),
            ..listed.clone()
        },
        *default
    );
    // A `models.json` entry is used as written and takes the rest from
    // the provider's default model.
    let custom = model("anthropic", "claude-custom");
    assert_eq!(custom.name.as_deref(), Some("Custom Claude"));
    assert_eq!(
        (&custom.api, &custom.base_url),
        (&ProviderApi::Anthropic, &None)
    );
    assert_eq!(custom.max_tokens, default.max_tokens);
    assert_eq!(
        custom.supported_thinking_levels(),
        default.supported_thinking_levels()
    );
    // Nothing is known about a provider the catalog lacks.
    let elsewhere = model("elsewhere", "model");
    assert_eq!(elsewhere.max_tokens, None);
    assert_eq!(elsewhere.supported_thinking_levels(), ["off"]);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn with_session_dir_expands_relative_paths_from_cwd() {
    let cwd = test_dir("noel-config-session-dir");
    let paths = ConfigPaths {
        cwd: cwd.clone(),
        agent_dir: cwd.join("agent"),
        session_dir: cwd.join("agent/sessions"),
        settings_path: cwd.join("agent/settings.json"),
        project_settings_path: cwd.join(".noel/settings.json"),
        auth_path: cwd.join("agent/auth.json"),
        models_path: cwd.join("agent/models.json"),
        model_cache_path: cwd.join("agent/model-cache.json"),
        keybindings_path: cwd.join("agent/keybindings.json"),
    };

    let next = paths
        .with_session_dir("local-sessions")
        .expect("apply session dir");

    assert_eq!(next.session_dir, cwd.join("local-sessions"));
    let _ = fs::remove_dir_all(cwd);
}

#[test]
fn tool_lists_validate_and_match_like_upstream() {
    let list = |entries: &[&str]| entries.iter().map(ToString::to_string).collect::<Vec<_>>();
    assert_eq!(tool_list_error(&list(&["read", "mcp_*"])), None);
    assert_eq!(tool_list_error(&list(&["+grep", "-bash"])), None);
    assert_eq!(
        tool_list_error(&list(&["read", "+grep"])).as_deref(),
        Some("tool names cannot be mixed with +name or -name entries")
    );
    assert_eq!(
        tool_list_error(&list(&["+grep", "-web_*"])).as_deref(),
        Some("+name and -name entries take exact tool names, not patterns: -web_*")
    );

    let patterns = list(&["read", "web_*", "*_tool", "a*b*c"]);
    for name in [
        "read",
        "web_",
        "web_search",
        "my_tool",
        "abc",
        "a-b-c",
        "axxbyybc",
    ] {
        assert!(tool_name_matches(&patterns, name), "{name}");
    }
    // A pattern covers the whole name, and a plain entry is exact.
    for name in [
        "reader",
        "xread",
        "the_web_search",
        "tool_x",
        "ab",
        "acb",
        "abcd",
    ] {
        assert!(!tool_name_matches(&patterns, name), "{name}");
    }
    assert!(tool_name_matches(&list(&["*"]), "anything"));
    assert!(!tool_name_matches(&[], "read"));
}

#[test]
fn settings_behavior_matches_upstream_ts_fixture() {
    let fixture = serde_json::from_str::<serde_json::Value>(include_str!(
        "../../../tests/fixtures/ts-parity/settings.json"
    ))
    .expect("parse settings fixture");

    for case in fixture["cases"].as_array().expect("settings cases") {
        let name = case["name"].as_str().expect("case name");
        let root = test_dir(&format!("noel-config-settings-parity-{name}"));
        let agent_dir = root.join("agent");
        let cwd = root.join("repo");
        fs::create_dir_all(&agent_dir).expect("create agent dir");
        fs::create_dir_all(cwd.join(".noel")).expect("create project settings dir");

        let global = &case["globalSettings"];
        if !global.as_object().expect("global settings").is_empty() {
            fs::write(
                agent_dir.join("settings.json"),
                format!(
                    "{}\n",
                    serde_json::to_string_pretty(global).expect("serialize global")
                ),
            )
            .expect("write global settings");
        }
        let project = &case["projectSettings"];
        if !project.as_object().expect("project settings").is_empty() {
            fs::write(
                cwd.join(".noel/settings.json"),
                format!(
                    "{}\n",
                    serde_json::to_string_pretty(project).expect("serialize project")
                ),
            )
            .expect("write project settings");
        }

        let config = load_config(ConfigPaths {
            cwd: cwd.clone(),
            agent_dir: agent_dir.clone(),
            session_dir: agent_dir.join("sessions"),
            settings_path: agent_dir.join("settings.json"),
            project_settings_path: cwd.join(".noel/settings.json"),
            auth_path: root.join("missing-auth.json"),
            models_path: root.join("missing-models.json"),
            model_cache_path: root.join("missing-model-cache.json"),
            keybindings_path: root.join("missing-keybindings.json"),
        })
        .unwrap_or_else(|error| panic!("load config for {name}: {error}"));

        assert_eq!(
            settings_summary(&config.settings),
            case["expected"],
            "settings parity case {name}"
        );

        let _ = fs::remove_dir_all(root);
    }
}

#[test]
fn theme_and_accent_settings_merge_independently() {
    let global: Settings =
        serde_json::from_str(r#"{"theme":"dark","accentColor":"cyan"}"#).unwrap();
    let project: Settings = serde_json::from_str(r##"{"accentColor":"#123456"}"##).unwrap();
    let settings = global.merge(project);
    assert_eq!(settings.theme.as_deref(), Some("dark"));
    assert_eq!(settings.accent_color.as_deref(), Some("#123456"));
    let settings = settings.merge(serde_json::from_str(r#"{"theme":"kimi"}"#).unwrap());
    assert_eq!(settings.theme.as_deref(), Some("kimi"));
    assert_eq!(settings.accent_color.as_deref(), Some("#123456"));
}

/// `failover` is off unless a settings file turns it on, and merges as
/// the other nested settings do: a project keeps what it does not name.
/// Its per-provider map is replaced whole by a project that has one.
#[test]
fn failover_settings_are_off_by_default_and_merge_like_their_neighbours() {
    assert_eq!(Settings::default().failover, None);
    let parsed: Settings = serde_json::from_str("{}").unwrap();
    assert_eq!(parsed.failover, None);

    let global: Settings = serde_json::from_str(
        r#"{"failover":{"enabled":true,"providers":{"anthropic":{"minSwitchIntervalMs":5000}}}}"#,
    )
    .unwrap();
    let unrelated: Settings = serde_json::from_str(r#"{"theme":"dark"}"#).unwrap();
    let kept = global.clone().merge(unrelated).failover.expect("failover");
    assert_eq!(kept.enabled, Some(true));
    assert_eq!(
        kept.providers["anthropic"].min_switch_interval_ms,
        Some(5000)
    );

    let project: Settings = serde_json::from_str(
        r#"{"failover":{"providers":{"openai":{"exhaustionThresholdPct":80}}}}"#,
    )
    .unwrap();
    let merged = global.merge(project).failover.expect("failover");
    assert_eq!(merged.enabled, Some(true));
    assert_eq!(merged.providers.keys().collect::<Vec<_>>(), ["openai"]);
    assert_eq!(
        merged.providers["openai"].exhaustion_threshold_pct,
        Some(80)
    );
}

#[test]
fn nested_settings_deep_merge_like_ts_reference() {
    let root = test_dir("noel-config-nested-settings");
    let agent_dir = root.join("agent");
    let cwd = root.join("repo");
    fs::create_dir_all(&agent_dir).expect("create agent dir");
    fs::create_dir_all(cwd.join(".noel")).expect("create project settings dir");
    fs::write(
        agent_dir.join("settings.json"),
        r#"{
                "transport":"auto",
                "steeringMode":"all",
                "compaction":{"enabled":true,"reserveTokens":100},
                "retry":{"enabled":true,"provider":{"timeoutMs":1000}},
                "terminal":{"showImages":true},
                "images":{"autoResize":true},
                "thinkingBudgets":{"minimal":1000},
                "markdown":{"codeBlockIndent":"  "},
                "warnings":{"anthropicExtraUsage":true},
                "modelRefresh":{"enabled":true,"ttlHours":24}
            }"#,
    )
    .expect("write user settings");
    fs::write(
        cwd.join(".noel/settings.json"),
        r#"{
                "followUpMode":"one-at-a-time",
                "compaction":{"keepRecentTokens":200},
                "retry":{"provider":{"maxRetries":2}},
                "terminal":{"imageWidthCells":40},
                "images":{"blockImages":true},
                "thinkingBudgets":{"high":4000},
                "warnings":{"anthropicExtraUsage":false},
                "modelRefresh":{"ttlHours":12}
            }"#,
    )
    .expect("write project settings");

    let config = load_config(ConfigPaths {
        cwd: cwd.clone(),
        agent_dir: agent_dir.clone(),
        session_dir: agent_dir.join("sessions"),
        settings_path: agent_dir.join("settings.json"),
        project_settings_path: cwd.join(".noel/settings.json"),
        auth_path: root.join("missing-auth.json"),
        models_path: root.join("missing-models.json"),
        model_cache_path: root.join("missing-model-cache.json"),
        keybindings_path: root.join("missing-keybindings.json"),
    })
    .expect("load config");

    assert_eq!(config.settings.transport.as_deref(), Some("auto"));
    assert_eq!(config.settings.steering_mode.as_deref(), Some("all"));
    assert_eq!(
        config.settings.follow_up_mode.as_deref(),
        Some("one-at-a-time")
    );
    let compaction = config.settings.compaction.expect("compaction");
    assert_eq!(compaction.enabled, Some(true));
    assert_eq!(compaction.reserve_tokens, Some(100));
    assert_eq!(compaction.keep_recent_tokens, Some(200));
    let retry = config.settings.retry.expect("retry");
    let provider = retry.provider.expect("provider retry");
    assert_eq!(provider.timeout_ms, Some(1000));
    assert_eq!(provider.max_retries, Some(2));
    let terminal = config.settings.terminal.expect("terminal");
    assert_eq!(terminal.show_images, Some(true));
    assert_eq!(terminal.image_width_cells, Some(40));
    let images = config.settings.images.expect("images");
    assert_eq!(images.auto_resize, Some(true));
    assert_eq!(images.block_images, Some(true));
    let thinking = config.settings.thinking_budgets.expect("thinking budgets");
    assert_eq!(thinking.minimal, Some(1000));
    assert_eq!(thinking.high, Some(4000));
    assert_eq!(
        config
            .settings
            .markdown
            .expect("markdown")
            .code_block_indent
            .as_deref(),
        Some("  ")
    );
    assert_eq!(
        config
            .settings
            .warnings
            .expect("warnings")
            .anthropic_extra_usage,
        Some(false)
    );
    let model_refresh = config.settings.model_refresh.expect("model refresh");
    assert_eq!(model_refresh.enabled, Some(true));
    assert_eq!(model_refresh.ttl_hours, Some(12));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn explicit_oauth_maps_to_provider_specific_auth() {
    let auth = AuthData::from(BTreeMap::from([
        (
            "anthropic".to_string(),
            AuthCredential::OAuth {
                access_token: "claude-token".to_string(),
                refresh_token: None,
                expires: 0,
                account_id: None,
            },
        ),
        (
            "openai-codex".to_string(),
            AuthCredential::OAuth {
                access_token: "codex-token".to_string(),
                refresh_token: Some("codex-refresh".to_string()),
                expires: 0,
                account_id: Some("account-id".to_string()),
            },
        ),
    ]));

    assert_eq!(
        auth_for_provider(&auth, "anthropic", None),
        Some(ResolvedAuth::ClaudeCodeOAuth {
            access_token: "claude-token".to_string(),
            refresh_token: None,
            expires: 0,
        })
    );
    assert_eq!(
        auth_for_provider(&auth, "openai-codex", None),
        Some(ResolvedAuth::ChatGptOAuth {
            access_token: "codex-token".to_string(),
            refresh_token: Some("codex-refresh".to_string()),
            expires: 0,
            account_id: Some("account-id".to_string())
        })
    );
}

/// The variables that can give `anthropic`, `openai`, `openai-codex`, or
/// a Cloudflare provider a credential.
const LOGIN_ENV: [&str; 11] = [
    "CLOUDFLARE_API_KEY",
    "CLOUDFLARE_ACCOUNT_ID",
    "CLOUDFLARE_GATEWAY_ID",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_OAUTH_TOKEN",
    "ANTHROPIC_AUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "OPENAI_API_KEY",
    "CODEX_API_KEY",
    "CODEX_ACCESS_TOKEN",
    "CHATGPT_ACCOUNT_ID",
];

/// Runs `test` with none of [`LOGIN_ENV`] set and no login file read, so
/// those providers have only the credentials the test gives them.
fn without_ambient_logins(test: impl FnOnce()) {
    let _guard = ENV_LOCK.lock().expect("lock env");
    let saved = LOGIN_ENV.map(save_env);
    let isolation = save_env(ENV_TEST_ISOLATION);
    for name in LOGIN_ENV {
        env::remove_var(name);
    }
    env::set_var(ENV_TEST_ISOLATION, "1");
    test();
    for (name, value) in saved {
        restore_env(name, value);
    }
    restore_env(isolation.0, isolation.1);
}

/// Which credentials make a provider's models available, and the model
/// startup then takes, against upstream's real model runtime
/// (`credentials` in `model-resolution.json`).
#[test]
fn credentials_offer_the_same_models_as_upstream() {
    let fixture = serde_json::from_str::<serde_json::Value>(include_str!(
        "../../../tests/fixtures/ts-parity/model-resolution.json"
    ))
    .expect("parse model-resolution.json");
    let cases = fixture["credentials"].as_array().expect("credentials");
    assert!(cases.len() >= 16, "{} cases", cases.len());
    let models = default_models();
    let models = models.iter().collect::<Vec<_>>();
    without_ambient_logins(|| {
        for case in cases {
            let name = case["name"].as_str().expect("name");
            let input = &case["input"];
            // Upstream's `auth.json`, read as noel reads the file.
            let auth =
                serde_json::from_value::<AuthData>(input["auth"].clone()).expect("parse auth.json");
            let variables = input["env"].as_object().expect("env");
            for (variable, value) in variables {
                env::set_var(variable, value.as_str().expect("value"));
            }
            let asked = input["providers"]
                .as_array()
                .expect("providers")
                .iter()
                .map(|provider| provider.as_str().expect("provider"))
                .collect::<Vec<_>>();
            let has_auth = |provider: &str| {
                asked.contains(&provider) && has_auth_for_provider(&auth, provider, None)
            };
            let configured = asked
                .iter()
                .copied()
                .filter(|provider| has_auth(provider))
                .collect::<Vec<_>>();
            let start = find_initial_model(&models, None, None, &has_auth);
            let answered = serde_json::json!({
                "configured": configured,
                "initialModel": start.map(|model| {
                    serde_json::json!({ "provider": model.provider, "id": model.id })
                }),
            });
            for variable in variables.keys() {
                env::remove_var(variable);
            }

            let mut expected = case["expected"].clone();
            // The one case noel answers differently, on purpose: upstream's
            // own ChatGPT sign-in for `openai`, a flow noel does not have
            // ("Credentials That Make a Provider Available" in the
            // non-parity register).
            if name == "chatgpt-sign-in-stored-for-openai" {
                assert_eq!(expected["configured"], serde_json::json!(["openai"]));
                expected = serde_json::json!({ "configured": [], "initialModel": null });
            }
            assert_eq!(answered, expected, "{name}");
        }
    });
}

/// The states only noel can be in: the Codex CLI's variables, and the
/// login `noel login` used to store under `openai` as well.
#[test]
fn a_chatgpt_login_serves_openai_codex_and_an_api_key_serves_openai() {
    let login = || AuthCredential::OAuth {
        access_token: "login-token".to_string(),
        refresh_token: Some("login-refresh".to_string()),
        expires: 0,
        account_id: None,
    };
    let key = |key: &str| AuthCredential::ApiKey {
        key: key.to_string(),
    };
    let is_login = |auth: &AuthData, provider: &str| {
        matches!(
            auth_for_provider(auth, provider, None),
            Some(ResolvedAuth::ChatGptOAuth { .. })
        )
    };
    without_ambient_logins(|| {
        let mut auth = AuthData::default();
        auth.insert("openai", DEFAULT_ACCOUNT_NAME, login());
        auth.insert("openai-codex", DEFAULT_ACCOUNT_NAME, login());
        assert!(is_login(&auth, "openai-codex"));
        assert_eq!(auth.accounts_for_provider("openai-codex"), ["default"]);
        assert!(!has_auth_for_provider(&auth, "openai", None));
        assert!(auth.accounts_for_provider("openai").is_empty());
        // The login is not in the way of a key, in the environment or
        // stored beside it.
        env::set_var("OPENAI_API_KEY", "env-key");
        assert_eq!(
            auth_for_provider(&auth, "openai", None),
            Some(ResolvedAuth::ApiKey("env-key".to_string()))
        );
        env::remove_var("OPENAI_API_KEY");
        auth.insert("openai", "work", key("work-key"));
        assert_eq!(auth.account_for_resolution("openai"), Some("work"));
        assert_eq!(
            auth_for_provider(&auth, "openai", None),
            Some(ResolvedAuth::ApiKey("work-key".to_string()))
        );

        let mut auth = AuthData::default();
        auth.insert("openai-codex", DEFAULT_ACCOUNT_NAME, key("stored-key"));
        assert!(!has_auth_for_provider(&auth, "openai-codex", None));
        assert!(auth.accounts_for_provider("openai-codex").is_empty());

        // The Codex CLI's login token and its API key.
        let nothing = AuthData::default();
        env::set_var("CODEX_ACCESS_TOKEN", "codex-token");
        assert!(is_login(&nothing, "openai-codex"));
        assert!(!has_auth_for_provider(&nothing, "openai", None));
        env::remove_var("CODEX_ACCESS_TOKEN");
        env::set_var("CODEX_API_KEY", "codex-key");
        assert_eq!(
            auth_for_provider(&nothing, "openai", None),
            Some(ResolvedAuth::ApiKey("codex-key".to_string()))
        );
        assert!(!has_auth_for_provider(&nothing, "openai-codex", None));
        env::remove_var("CODEX_API_KEY");
    });
}

#[test]
fn auth_discovery_matches_upstream_ts_fixture() {
    let fixture = serde_json::from_str::<serde_json::Value>(include_str!(
        "../../../tests/fixtures/ts-parity/auth-discovery.json"
    ))
    .expect("parse fixture");
    assert_eq!(fixture["fakeTokensOnly"], true);
    assert_eq!(
        fixture["redaction"]["fixturesUseOnlyFakeTokens"],
        serde_json::Value::Bool(true)
    );

    let api_key_credential =
        serde_json::from_value::<AuthCredential>(fixture["authJson"]["apiKeyCredential"].clone())
            .expect("parse api key credential");
    let oauth_credential =
        serde_json::from_value::<AuthCredential>(fixture["authJson"]["oauthCredential"].clone())
            .expect("parse oauth credential");
    assert_eq!(
        oauth_credential,
        AuthCredential::OAuth {
            access_token: "stored-codex-access-token".to_string(),
            refresh_token: Some("stored-codex-refresh-token".to_string()),
            expires: 4102444800000,
            account_id: Some("account-id".to_string()),
        }
    );
    let auth = AuthData::from(BTreeMap::from([
        ("anthropic".to_string(), api_key_credential),
        ("openai-codex".to_string(), oauth_credential),
    ]));
    assert_eq!(
        auth_for_provider(&auth, "anthropic", None),
        Some(ResolvedAuth::ApiKey("stored-anthropic-api-key".to_string()))
    );
    assert_eq!(
        auth_for_provider(&auth, "openai-codex", None),
        Some(ResolvedAuth::ChatGptOAuth {
            access_token: "stored-codex-access-token".to_string(),
            refresh_token: Some("stored-codex-refresh-token".to_string()),
            expires: 4102444800000,
            account_id: Some("account-id".to_string())
        })
    );

    let _guard = ENV_LOCK.lock().expect("lock env");
    let saved = [
        save_env("ANTHROPIC_OAUTH_TOKEN"),
        save_env("ANTHROPIC_AUTH_TOKEN"),
        save_env("CLAUDE_CODE_OAUTH_TOKEN"),
        save_env("ANTHROPIC_API_KEY"),
        save_env("OPENAI_API_KEY"),
        save_env("AZURE_OPENAI_API_KEY"),
        save_env("CODEX_API_KEY"),
        save_env("CODEX_ACCESS_TOKEN"),
        save_env("CHATGPT_ACCOUNT_ID"),
    ];
    for (name, _) in saved.iter() {
        env::remove_var(name);
    }

    env::set_var("ANTHROPIC_OAUTH_TOKEN", "claude-oauth-env-token");
    env::set_var("ANTHROPIC_API_KEY", "anthropic-api-key");
    assert_eq!(
        auth_for_provider(&AuthData::default(), "anthropic", None),
        Some(ResolvedAuth::ClaudeCodeOAuth {
            access_token: "claude-oauth-env-token".to_string(),
            refresh_token: None,
            expires: 0,
        })
    );
    assert_eq!(
        auth_for_provider(&auth, "anthropic", None),
        Some(ResolvedAuth::ApiKey("stored-anthropic-api-key".to_string()))
    );

    env::remove_var("ANTHROPIC_OAUTH_TOKEN");
    env::remove_var("ANTHROPIC_API_KEY");
    // Every environment case of the fixture, under upstream's provider
    // id: the variables it sets resolve to the credential it expects.
    let cases = fixture["env"].as_array().expect("env cases");
    assert!(cases.iter().any(|case| case["provider"] == "azure"));
    for case in cases {
        let provider = case["provider"].as_str().expect("provider");
        let variables = case["env"].as_object().expect("env");
        for (name, value) in variables {
            env::set_var(name, value.as_str().expect("value"));
        }
        let token = match auth_for_provider(&AuthData::default(), provider, None) {
            Some(ResolvedAuth::ApiKey(key)) => Some(key),
            Some(ResolvedAuth::ClaudeCodeOAuth { access_token, .. })
            | Some(ResolvedAuth::ChatGptOAuth { access_token, .. }) => Some(access_token),
            None => None,
        };
        assert_eq!(
            token.as_deref(),
            case["expectedApiKey"].as_str(),
            "{provider}"
        );
        for name in variables.keys() {
            env::remove_var(name);
        }
    }

    for (name, value) in saved {
        restore_env(name, value);
    }

    let root = test_dir("noel-config-auth-discovery");
    fs::create_dir_all(root.join(".claude")).expect("create claude dir");
    fs::create_dir_all(root.join(".codex")).expect("create codex dir");
    fs::write(
        root.join(".claude").join(".credentials.json"),
        serde_json::to_string(&fixture["interopLoginFiles"]["claudeCode"]["sample"])
            .expect("encode claude sample"),
    )
    .expect("write claude credentials");
    fs::write(
        root.join(".codex").join("auth.json"),
        serde_json::to_string(&fixture["interopLoginFiles"]["codex"]["sample"])
            .expect("encode codex sample"),
    )
    .expect("write codex auth");
    assert_eq!(
        read_claude_code_oauth_from_path(&root.join(".claude").join(".credentials.json")),
        Some(ResolvedAuth::ClaudeCodeOAuth {
            access_token: "claude-access".to_string(),
            refresh_token: Some("redacted".to_string()),
            expires: 0,
        })
    );
    assert_eq!(
        read_codex_chatgpt_oauth_from_path(&root.join(".codex").join("auth.json")),
        Some(ResolvedAuth::ChatGptOAuth {
            access_token: "codex-access".to_string(),
            refresh_token: Some("redacted".to_string()),
            expires: 0,
            account_id: Some("account-id".to_string())
        })
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn auth_data_v1_file_loads_as_default_account() {
    let auth = serde_json::from_str::<AuthData>(
            r#"{"openai":{"type":"api_key","key":"v1-key"},"anthropic":{"type":"oauth","access_token":"v1-token","expires":0}}"#,
        )
        .expect("parse v1 auth");

    assert_eq!(auth.accounts_for_provider("openai"), vec!["default"]);
    assert_eq!(
        auth_for_provider(&auth, "openai", None),
        Some(ResolvedAuth::ApiKey("v1-key".to_string()))
    );
    assert_eq!(
        auth_for_provider(&auth, "anthropic", None),
        Some(ResolvedAuth::ClaudeCodeOAuth {
            access_token: "v1-token".to_string(),
            refresh_token: None,
            expires: 0,
        })
    );
}

#[test]
fn auth_data_write_emits_v2_shape() {
    let mut auth = AuthData::default();
    auth.insert(
        "openai",
        DEFAULT_ACCOUNT_NAME,
        AuthCredential::ApiKey {
            key: "stored-key".to_string(),
        },
    );

    let written = serde_json::to_string(&auth).expect("serialize auth");
    let value = serde_json::from_str::<serde_json::Value>(&written).expect("parse written");
    assert_eq!(value["openai"]["default"]["type"], "api_key");
    assert_eq!(value["openai"]["default"]["key"], "stored-key");

    let reloaded = serde_json::from_str::<AuthData>(&written).expect("reload v2 auth");
    assert_eq!(reloaded, auth);
}

#[test]
fn auth_data_v2_round_trips_identically() {
    let raw = r#"{"anthropic":{"default":{"type":"api_key","key":"personal"},"work":{"type":"oauth","access_token":"work-token","refresh_token":"work-refresh","expires":4102444800000,"account_id":"acct-1"}}}"#;

    let auth = serde_json::from_str::<AuthData>(raw).expect("parse v2 auth");
    assert_eq!(
        auth.accounts_for_provider("anthropic"),
        vec!["default".to_string(), "work".to_string()]
    );

    let written = serde_json::to_string(&auth).expect("serialize auth");
    let value = serde_json::from_str::<serde_json::Value>(&written).expect("parse written");
    let original = serde_json::from_str::<serde_json::Value>(raw).expect("parse original");
    assert_eq!(value, original);
}

#[test]
fn requested_account_wins_over_default_and_env() {
    let _guard = ENV_LOCK.lock().expect("lock env");
    let saved = save_env("OPENAI_API_KEY");
    env::set_var("OPENAI_API_KEY", "env-key");

    let mut auth = AuthData::default();
    auth.insert(
        "openai",
        DEFAULT_ACCOUNT_NAME,
        AuthCredential::ApiKey {
            key: "default-key".to_string(),
        },
    );
    auth.insert(
        "openai",
        "work",
        AuthCredential::ApiKey {
            key: "work-key".to_string(),
        },
    );

    assert_eq!(
        auth_for_provider(&auth, "openai", Some("work")),
        Some(ResolvedAuth::ApiKey("work-key".to_string()))
    );
    assert_eq!(
        auth_for_provider(&auth, "openai", None),
        Some(ResolvedAuth::ApiKey("default-key".to_string()))
    );
    assert_eq!(
        api_key_for_provider(&auth, "openai", Some("work")),
        Some("work-key".to_string())
    );

    restore_env("OPENAI_API_KEY", saved.1);
}

#[test]
fn sole_account_resolves_without_default_name() {
    let mut auth = AuthData::default();
    auth.insert(
        "openai",
        "work",
        AuthCredential::ApiKey {
            key: "work-key".to_string(),
        },
    );

    assert_eq!(
        auth_for_provider(&auth, "openai", None),
        Some(ResolvedAuth::ApiKey("work-key".to_string()))
    );
    assert!(has_auth_for_provider(&auth, "openai", None));
}

#[test]
fn ambiguous_accounts_fall_through_to_env() {
    let _guard = ENV_LOCK.lock().expect("lock env");
    let saved = save_env("OPENAI_API_KEY");
    env::set_var("OPENAI_API_KEY", "env-key");

    let mut auth = AuthData::default();
    for account in ["personal", "work"] {
        auth.insert(
            "openai",
            account,
            AuthCredential::ApiKey {
                key: format!("{account}-key"),
            },
        );
    }

    assert_eq!(
        auth_for_provider(&auth, "openai", None),
        Some(ResolvedAuth::ApiKey("env-key".to_string()))
    );

    restore_env("OPENAI_API_KEY", saved.1);
}

#[test]
fn named_account_resolves_env_indirection() {
    let _guard = ENV_LOCK.lock().expect("lock env");
    let saved = save_env("WORK_OPENAI_KEY");
    env::set_var("WORK_OPENAI_KEY", "indirected-key");

    let mut auth = AuthData::default();
    auth.insert(
        "openai",
        "work",
        AuthCredential::ApiKey {
            key: "env:WORK_OPENAI_KEY".to_string(),
        },
    );

    assert_eq!(
        auth_for_provider(&auth, "openai", Some("work")),
        Some(ResolvedAuth::ApiKey("indirected-key".to_string()))
    );

    restore_env("WORK_OPENAI_KEY", saved.1);
}

#[test]
fn listed_accounts_combine_stored_and_pseudo_accounts() {
    let mut auth = AuthData::default();
    auth.insert(
        "openai",
        "work",
        AuthCredential::ApiKey {
            key: "work-key".to_string(),
        },
    );

    assert_eq!(
        listed_accounts(&auth, "openai", false, false),
        vec![("work".to_string(), AccountSource::Stored)]
    );
    assert_eq!(
        listed_accounts(&auth, "openai", true, true),
        vec![
            ("work".to_string(), AccountSource::Stored),
            ("env".to_string(), AccountSource::Environment),
            ("codex-import".to_string(), AccountSource::ImportedFile),
        ]
    );
    assert_eq!(
        listed_accounts(&auth, "anthropic", true, true),
        vec![
            ("env".to_string(), AccountSource::Environment),
            ("claude-import".to_string(), AccountSource::ImportedFile),
        ]
    );

    auth.insert(
        "openai",
        "env",
        AuthCredential::ApiKey {
            key: "stored-env".to_string(),
        },
    );
    assert_eq!(
        listed_accounts(&auth, "openai", true, false)
            .iter()
            .filter(|(account, _)| account == "env")
            .count(),
        1
    );
}

#[test]
fn account_for_resolution_mirrors_default_resolution() {
    let mut auth = AuthData::default();
    assert_eq!(auth.account_for_resolution("openai"), None);

    auth.insert(
        "openai",
        "work",
        AuthCredential::ApiKey {
            key: "work-key".to_string(),
        },
    );
    assert_eq!(auth.account_for_resolution("openai"), Some("work"));

    auth.insert(
        "openai",
        "personal",
        AuthCredential::ApiKey {
            key: "personal-key".to_string(),
        },
    );
    assert_eq!(auth.account_for_resolution("openai"), None);

    auth.insert(
        "openai",
        DEFAULT_ACCOUNT_NAME,
        AuthCredential::ApiKey {
            key: "default-key".to_string(),
        },
    );
    assert_eq!(
        auth.account_for_resolution("openai"),
        Some(DEFAULT_ACCOUNT_NAME)
    );
}

#[test]
fn remove_prunes_empty_provider_entry() {
    let mut auth = AuthData::default();
    auth.insert(
        "openai",
        DEFAULT_ACCOUNT_NAME,
        AuthCredential::ApiKey {
            key: "key".to_string(),
        },
    );

    assert!(auth.remove("openai", DEFAULT_ACCOUNT_NAME).is_some());
    assert!(auth.is_empty());
    assert_eq!(serde_json::to_string(&auth).expect("serialize"), "{}");
}

#[test]
fn provider_api_names_match_ts_reference_names() {
    let models = serde_json::from_str::<Vec<ModelDefinition>>(
        r#"[
                {"provider":"openai","id":"gpt","api":"openai-responses"},
                {"provider":"openai-codex","id":"codex","api":"openai-codex-responses"},
                {"provider":"azure","id":"gpt","api":"azure-openai-responses"},
                {"provider":"anthropic","id":"claude","api":"anthropic-messages"},
                {"provider":"google","id":"gemini","api":"google-generative-ai"},
                {"provider":"google-vertex","id":"gemini","api":"google-vertex"},
                {"provider":"amazon-bedrock","id":"claude","api":"bedrock-converse-stream"},
                {"provider":"mistral","id":"devstral","api":"mistral-conversations"}
            ]"#,
    )
    .expect("parse models");

    assert_eq!(models[0].api, ProviderApi::OpenAiResponses);
    assert_eq!(models[1].api, ProviderApi::OpenAiCodexResponses);
    assert_eq!(models[2].api, ProviderApi::AzureOpenAiResponses);
    assert_eq!(models[3].api, ProviderApi::Anthropic);
    assert_eq!(models[4].api, ProviderApi::Google);
    assert_eq!(models[5].api, ProviderApi::GoogleVertex);
    assert_eq!(models[6].api, ProviderApi::Bedrock);
    assert_eq!(models[7].api, ProviderApi::Mistral);
}

/// `model-catalog.json` in full: every upstream model noel can run is a
/// default model with upstream's values. (`noel_ai::catalog` compares the
/// catalog itself; this compares what a config loads.)
/// In a Codewire Sandbox the base-URL variables name the local
/// credential proxy: the provider's built-in models follow them, a
/// `models.json` entry that names its own address keeps it, and other
/// providers are untouched.
#[test]
fn provider_base_url_variables_point_its_models_at_them() {
    let env = |name: &str| match name {
        "ANTHROPIC_BASE_URL" => Some("http://127.0.0.1:4141/_codewire/anthropic".to_string()),
        "OPENAI_BASE_URL" => Some("http://127.0.0.1:4141/_codewire/openai/v1".to_string()),
        _ => None,
    };
    let model = |provider: &str, base_url: Option<&str>| ModelDefinition {
        provider: provider.to_string(),
        id: "m".to_string(),
        base_url: base_url.map(str::to_string),
        ..ModelDefinition::default()
    };
    let urls = |models: Vec<ModelDefinition>| {
        models
            .into_iter()
            .map(|model| model.base_url)
            .collect::<Vec<_>>()
    };
    let builtin = vec![
        model("anthropic", Some("https://api.anthropic.com/v1")),
        model("openai", Some("https://api.openai.com/v1")),
        model("mistral", Some("https://api.mistral.ai/v1")),
    ];
    assert_eq!(
        urls(with_base_url_env(builtin, true, env)),
        [
            Some("http://127.0.0.1:4141/_codewire/anthropic".to_string()),
            Some("http://127.0.0.1:4141/_codewire/openai/v1".to_string()),
            Some("https://api.mistral.ai/v1".to_string()),
        ]
    );
    let explicit = vec![
        model("anthropic", Some("https://gateway.example/anthropic")),
        model("anthropic", None),
    ];
    assert_eq!(
        urls(with_base_url_env(explicit, false, env)),
        [
            Some("https://gateway.example/anthropic".to_string()),
            Some("http://127.0.0.1:4141/_codewire/anthropic".to_string()),
        ]
    );
    // Unset, nothing changes.
    let builtin = vec![model("anthropic", Some("https://api.anthropic.com/v1"))];
    assert_eq!(
        urls(with_base_url_env(builtin, true, |_| None)),
        [Some("https://api.anthropic.com/v1".to_string())]
    );
}

#[test]
fn default_models_are_upstreams_catalog_in_full() {
    let fixture = serde_json::from_str::<serde_json::Value>(include_str!(
        "../../../tests/fixtures/ts-parity/model-catalog.json"
    ))
    .expect("parse model catalog fixture");
    let defaults = default_models();
    let mut compared = 0;
    let mut providers = BTreeSet::new();
    for provider in fixture["providers"].as_array().expect("providers") {
        for record in provider["models"].as_array().expect("models") {
            if record["api"] == "pi-messages" {
                continue;
            }
            let model = defaults
                .iter()
                .find(|model| record["provider"] == *model.provider && record["id"] == *model.id)
                .unwrap_or_else(|| panic!("missing {record}"));
            let mut expected = serde_json::json!({});
            for key in [
                "provider",
                "id",
                "name",
                "api",
                "baseUrl",
                "contextWindow",
                "maxTokens",
                "reasoning",
                "thinkingLevelMap",
                "input",
                "cost",
            ] {
                if let Some(value) = record.get(key) {
                    expected[key] = value.clone();
                }
            }
            // Azure's models have no URL.
            if expected["baseUrl"] == "" {
                expected["baseUrl"] = serde_json::Value::Null;
            }
            assert_eq!(serde_json::to_value(model).expect("serialize"), expected);
            providers.insert(model.provider.clone());
            compared += 1;
        }
    }
    assert!(compared > 1000, "{compared} models compared");
    // Besides upstream's, the port's own providers and nothing else.
    let port_only = defaults
        .iter()
        .map(|model| model.provider.as_str())
        .filter(|provider| !providers.contains(*provider))
        .collect::<BTreeSet<_>>();
    assert_eq!(
        port_only,
        BTreeSet::from(["faux", "kimi-coding-openai", "zai-anthropic", "zai-coding"])
    );
    assert_eq!(defaults.len(), compared + 7);
    assert!(providers.contains("azure") && !providers.contains(LEGACY_AZURE_PROVIDER));
}

#[test]
fn the_former_azure_provider_id_still_names_azure() {
    assert_eq!(canonical_provider("azure-openai-responses"), "azure");
    assert_eq!(canonical_provider("azure"), "azure");
    assert_eq!(canonical_provider("openai"), "openai");

    // An auth.json entry saved under the former id answers to both ids
    // and stays where it is; a new entry goes beside it, not under a
    // second key.
    let mut auth: AuthData = serde_json::from_str(
        r#"{"azure-openai-responses":{"work":{"type":"api_key","key":"saved-key"}}}"#,
    )
    .expect("parse auth");
    for provider in ["azure", "azure-openai-responses"] {
        assert_eq!(auth.accounts_for_provider(provider), ["work"]);
        assert_eq!(auth.account_for_resolution(provider), Some("work"));
        assert_eq!(
            auth_for_provider(&auth, provider, None),
            Some(ResolvedAuth::ApiKey("saved-key".to_string()))
        );
    }
    auth.insert("azure", "home", api_key("home-key"));
    assert_eq!(
        auth.providers.keys().collect::<Vec<_>>(),
        ["azure-openai-responses"]
    );
    assert_eq!(auth.accounts_for_provider("azure"), ["home", "work"]);
    assert!(auth.remove("azure", "work").is_some());
    assert!(auth.remove("azure-openai-responses", "home").is_some());
    assert!(auth.is_empty());
    // Without a saved entry, logins are stored under upstream's id.
    auth.insert("azure-openai-responses", "work", api_key("key"));
    assert_eq!(auth.providers.keys().collect::<Vec<_>>(), ["azure"]);

    // Saved settings that name the provider.
    let settings: Settings = serde_json::from_value(serde_json::json!({
        "defaultProvider": "azure-openai-responses",
        "defaultModel": "gpt-5.2",
        "enabledModels": ["azure-openai-responses/gpt-5.*", "openai/*"],
        "modelThinkingLevels": { "azure-openai-responses/gpt-5.2": "high" }
    }))
    .expect("parse settings");
    let settings = settings.with_canonical_providers();
    assert_eq!(settings.default_provider.as_deref(), Some("azure"));
    assert_eq!(
        settings.enabled_models.as_deref(),
        Some(&["azure/gpt-5.*".to_string(), "openai/*".to_string()][..])
    );
    assert_eq!(
        settings.model_thinking_level("azure", "gpt-5.2"),
        Some("high")
    );

    // A models.json or cached entry under the former id.
    let merged = merge_model_definitions(
        default_models(),
        Vec::new(),
        vec![ModelDefinition {
            provider: "azure-openai-responses".to_string(),
            id: "my-deployment".to_string(),
            api: ProviderApi::AzureOpenAiResponses,
            ..ModelDefinition::default()
        }],
    );
    assert!(merged
        .iter()
        .any(|model| model.provider == "azure" && model.id == "my-deployment"));
    assert!(merged
        .iter()
        .all(|model| model.provider != "azure-openai-responses"));
}

#[test]
fn model_thinking_levels_merge_by_model() {
    let global: Settings = serde_json::from_value(serde_json::json!({
        "defaultThinkingLevel": "low",
        "modelThinkingLevels": { "anthropic/claude-opus-4-8": "xhigh", "openai/gpt-5.5": "high" }
    }))
    .expect("parse global");
    let project: Settings = serde_json::from_value(serde_json::json!({
        "modelThinkingLevels": { "openai/gpt-5.5": "low" }
    }))
    .expect("parse project");
    let settings = global.merge(project);
    assert_eq!(
        settings.model_thinking_level("anthropic", "claude-opus-4-8"),
        Some("xhigh")
    );
    assert_eq!(
        settings.model_thinking_level("openai", "gpt-5.5"),
        Some("low")
    );
    assert_eq!(settings.model_thinking_level("openai", "gpt-5.4"), None);
    assert_eq!(settings.default_thinking_level.as_deref(), Some("low"));
}

#[test]
fn a_model_definition_answers_for_its_thinking_levels() {
    let model = |fields: serde_json::Value| {
        let mut definition = serde_json::json!({ "provider": "local", "id": "test" });
        definition
            .as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        serde_json::from_value::<ModelDefinition>(definition).expect("parse")
    };
    // A definition that says nothing of reasoning has none.
    let bare = model(serde_json::json!({}));
    assert!(!bare.reasoning());
    assert_eq!(bare.supported_thinking_levels(), ["off"]);
    assert_eq!(bare.clamp_thinking_level("high"), "off");
    assert_eq!(bare.traits().max_tokens, None);
    assert_eq!(bare.traits().reasoning, Some(false));
    // A reasoning model without a map stops at high.
    let plain = model(serde_json::json!({ "reasoning": true }));
    assert_eq!(
        plain.supported_thinking_levels(),
        ["off", "minimal", "low", "medium", "high"]
    );
    assert_eq!(plain.clamp_thinking_level("max"), "high");
    // The upstream field names a models.json entry may use.
    let full = model(serde_json::json!({
        "api": "openai-completions", "baseUrl": "http://127.0.0.1:1/v1", "reasoning": true,
        "thinkingLevelMap": { "minimal": null, "xhigh": "xhigh" },
        "input": ["text", "image"], "contextWindow": 32000, "maxTokens": 4000,
        "cost": { "input": 0.5, "output": 1, "cacheRead": 0, "cacheWrite": 0 }
    }));
    assert_eq!(
        full.supported_thinking_levels(),
        ["off", "low", "medium", "high", "xhigh"]
    );
    assert_eq!(full.clamp_thinking_level("minimal"), "low");
    assert_eq!(full.traits().max_tokens, Some(4000));
    assert_eq!(
        full.cost.as_ref().and_then(|cost| cost.input.as_f64()),
        Some(0.5)
    );
    // What a cached listing writes stays what it wrote.
    assert_eq!(
        serde_json::to_value(&bare).expect("serialize"),
        serde_json::json!({
            "provider": "local", "id": "test", "name": null,
            "api": "openai-completions", "baseUrl": null
        })
    );
}

#[test]
fn default_image_models_include_openrouter_targets() {
    let models = default_image_models()
        .into_iter()
        .map(|model| format!("{}/{}", model.provider, model.id))
        .collect::<Vec<_>>();

    for model in [
        "openrouter/google/gemini-3.1-flash-image-preview",
        "openrouter/openai/gpt-5-image",
        "openrouter/black-forest-labs/flux.2-pro",
        "openrouter/openrouter/auto",
    ] {
        assert!(models.iter().any(|candidate| candidate == model));
    }
}

#[test]
fn env_auth_detects_provider_parity_targets() {
    let _guard = ENV_LOCK.lock().expect("lock env");
    let empty_auth = AuthData::default();
    let resolve = |provider: &str| auth_for_provider(&empty_auth, provider, None);
    let saved = [
        save_env("AZURE_OPENAI_API_KEY"),
        save_env("CEREBRAS_API_KEY"),
        save_env("COPILOT_GITHUB_TOKEN"),
        save_env("DEEPSEEK_API_KEY"),
        save_env("FIREWORKS_API_KEY"),
        save_env("GROQ_API_KEY"),
        save_env("HF_TOKEN"),
        save_env("KIMI_API_KEY"),
        save_env("MINIMAX_API_KEY"),
        save_env("MINIMAX_CN_API_KEY"),
        save_env("MOONSHOT_API_KEY"),
        save_env("OPENCODE_API_KEY"),
        save_env("OPENROUTER_API_KEY"),
        save_env("GOOGLE_CLOUD_API_KEY"),
        save_env("AWS_BEARER_TOKEN_BEDROCK"),
        save_env("MISTRAL_API_KEY"),
        save_env("CLOUDFLARE_API_KEY"),
        save_env("TOGETHER_API_KEY"),
        save_env("AI_GATEWAY_API_KEY"),
        save_env("XAI_API_KEY"),
        save_env("XIAOMI_API_KEY"),
        save_env("XIAOMI_TOKEN_PLAN_AMS_API_KEY"),
        save_env("XIAOMI_TOKEN_PLAN_CN_API_KEY"),
        save_env("XIAOMI_TOKEN_PLAN_SGP_API_KEY"),
        save_env("ZAI_API_KEY"),
        save_env("CODEX_ACCESS_TOKEN"),
        save_env("CHATGPT_ACCOUNT_ID"),
    ];

    env::set_var("AZURE_OPENAI_API_KEY", "azure-key");
    env::set_var("CEREBRAS_API_KEY", "cerebras-key");
    env::set_var("COPILOT_GITHUB_TOKEN", "copilot-key");
    env::set_var("DEEPSEEK_API_KEY", "deepseek-key");
    env::set_var("FIREWORKS_API_KEY", "fireworks-key");
    env::set_var("GROQ_API_KEY", "groq-key");
    env::set_var("HF_TOKEN", "hf-key");
    env::set_var("KIMI_API_KEY", "kimi-key");
    env::set_var("MINIMAX_API_KEY", "minimax-key");
    env::set_var("MINIMAX_CN_API_KEY", "minimax-cn-key");
    env::set_var("MOONSHOT_API_KEY", "moonshot-key");
    env::set_var("OPENCODE_API_KEY", "opencode-key");
    env::set_var("OPENROUTER_API_KEY", "openrouter-key");
    env::set_var("GOOGLE_CLOUD_API_KEY", "vertex-key");
    env::set_var("AWS_BEARER_TOKEN_BEDROCK", "bedrock-token");
    env::set_var("MISTRAL_API_KEY", "mistral-key");
    env::set_var("CLOUDFLARE_API_KEY", "cloudflare-key");
    env::set_var("TOGETHER_API_KEY", "together-key");
    env::set_var("AI_GATEWAY_API_KEY", "vercel-key");
    env::set_var("XAI_API_KEY", "xai-key");
    env::set_var("XIAOMI_API_KEY", "xiaomi-key");
    env::set_var("XIAOMI_TOKEN_PLAN_AMS_API_KEY", "xiaomi-ams-key");
    env::set_var("XIAOMI_TOKEN_PLAN_CN_API_KEY", "xiaomi-cn-key");
    env::set_var("XIAOMI_TOKEN_PLAN_SGP_API_KEY", "xiaomi-sgp-key");
    env::set_var("ZAI_API_KEY", "zai-key");
    env::set_var("CODEX_ACCESS_TOKEN", "codex-token");
    env::set_var("CHATGPT_ACCOUNT_ID", "account-id");

    assert_eq!(
        resolve("azure"),
        Some(ResolvedAuth::ApiKey("azure-key".to_string()))
    );
    // The id this port used before upstream's still names the provider.
    assert_eq!(resolve(LEGACY_AZURE_PROVIDER), resolve("azure"));
    assert_eq!(
        resolve("github-copilot"),
        Some(ResolvedAuth::ApiKey("copilot-key".to_string()))
    );
    assert_eq!(
        resolve("openrouter"),
        Some(ResolvedAuth::ApiKey("openrouter-key".to_string()))
    );
    assert_eq!(
        resolve("deepseek"),
        Some(ResolvedAuth::ApiKey("deepseek-key".to_string()))
    );
    assert_eq!(
        resolve("groq"),
        Some(ResolvedAuth::ApiKey("groq-key".to_string()))
    );
    assert_eq!(
        resolve("cerebras"),
        Some(ResolvedAuth::ApiKey("cerebras-key".to_string()))
    );
    assert_eq!(
        resolve("xai"),
        Some(ResolvedAuth::ApiKey("xai-key".to_string()))
    );
    assert_eq!(
        resolve("zai"),
        Some(ResolvedAuth::ApiKey("zai-key".to_string()))
    );
    assert_eq!(
        resolve("huggingface"),
        Some(ResolvedAuth::ApiKey("hf-key".to_string()))
    );
    assert_eq!(
        resolve("together"),
        Some(ResolvedAuth::ApiKey("together-key".to_string()))
    );
    assert_eq!(
        resolve("moonshotai"),
        Some(ResolvedAuth::ApiKey("moonshot-key".to_string()))
    );
    assert_eq!(
        resolve("moonshotai-cn"),
        Some(ResolvedAuth::ApiKey("moonshot-key".to_string()))
    );
    assert_eq!(
        resolve("opencode"),
        Some(ResolvedAuth::ApiKey("opencode-key".to_string()))
    );
    assert_eq!(
        resolve("opencode-go"),
        Some(ResolvedAuth::ApiKey("opencode-key".to_string()))
    );
    assert_eq!(
        resolve("fireworks"),
        Some(ResolvedAuth::ApiKey("fireworks-key".to_string()))
    );
    assert_eq!(
        resolve("minimax"),
        Some(ResolvedAuth::ApiKey("minimax-key".to_string()))
    );
    assert_eq!(
        resolve("minimax-cn"),
        Some(ResolvedAuth::ApiKey("minimax-cn-key".to_string()))
    );
    assert_eq!(
        resolve("kimi-coding"),
        Some(ResolvedAuth::ApiKey("kimi-key".to_string()))
    );
    assert_eq!(
        resolve("xiaomi"),
        Some(ResolvedAuth::ApiKey("xiaomi-key".to_string()))
    );
    assert_eq!(
        resolve("xiaomi-token-plan-cn"),
        Some(ResolvedAuth::ApiKey("xiaomi-cn-key".to_string()))
    );
    assert_eq!(
        resolve("xiaomi-token-plan-ams"),
        Some(ResolvedAuth::ApiKey("xiaomi-ams-key".to_string()))
    );
    assert_eq!(
        resolve("xiaomi-token-plan-sgp"),
        Some(ResolvedAuth::ApiKey("xiaomi-sgp-key".to_string()))
    );
    assert_eq!(
        resolve("vercel-ai-gateway"),
        Some(ResolvedAuth::ApiKey("vercel-key".to_string()))
    );
    assert_eq!(
        resolve("google-vertex"),
        Some(ResolvedAuth::ApiKey("vertex-key".to_string()))
    );
    assert_eq!(
        resolve("amazon-bedrock"),
        Some(ResolvedAuth::ApiKey("bedrock-token".to_string()))
    );
    assert_eq!(
        resolve("mistral"),
        Some(ResolvedAuth::ApiKey("mistral-key".to_string()))
    );
    assert_eq!(
        resolve("cloudflare-ai-gateway"),
        Some(ResolvedAuth::ApiKey("cloudflare-key".to_string()))
    );
    assert_eq!(
        resolve("openai-codex"),
        Some(ResolvedAuth::ChatGptOAuth {
            access_token: "codex-token".to_string(),
            refresh_token: None,
            expires: 0,
            account_id: Some("account-id".to_string())
        })
    );

    for (name, value) in saved {
        restore_env(name, value);
    }
}

#[test]
fn reads_claude_code_oauth_credentials_file() {
    let root = test_dir("noel-config-claude-oauth");
    fs::create_dir_all(&root).expect("create root");
    let path = root.join(".credentials.json");
    fs::write(
            &path,
            r#"{"claudeAiOauth":{"accessToken":"claude-access","refreshToken":"redacted","expiresAt":1}}"#,
        )
        .expect("write credentials");

    assert_eq!(
        read_claude_code_oauth_from_path(&path),
        Some(ResolvedAuth::ClaudeCodeOAuth {
            access_token: "claude-access".to_string(),
            refresh_token: Some("redacted".to_string()),
            expires: 0,
        })
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn reads_codex_chatgpt_oauth_auth_file() {
    let root = test_dir("noel-config-codex-oauth");
    fs::create_dir_all(&root).expect("create root");
    let path = root.join("auth.json");
    fs::write(
            &path,
            r#"{"auth_mode":"chatgpt","tokens":{"access_token":"codex-access","refresh_token":"redacted","account_id":"account-id"}}"#,
        )
        .expect("write auth");

    assert_eq!(
        read_codex_chatgpt_oauth_from_path(&path),
        Some(ResolvedAuth::ChatGptOAuth {
            access_token: "codex-access".to_string(),
            refresh_token: Some("redacted".to_string()),
            expires: 0,
            account_id: Some("account-id".to_string())
        })
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn codex_auth_file_captures_refresh_and_jwt_expiry() {
    let root = test_dir("noel-config-codex-jwt");
    fs::create_dir_all(&root).expect("create root");
    let path = root.join("auth.json");
    let access_token = fake_jwt(r#"{"exp":4102444800}"#);
    fs::write(
            &path,
            format!(
                r#"{{"auth_mode":"chatgpt","last_refresh":"2026-01-01T00:00:00Z","tokens":{{"access_token":"{access_token}","refresh_token":"codex-refresh","account_id":"account-id"}}}}"#
            ),
        )
        .expect("write auth");

    assert_eq!(
        read_codex_chatgpt_oauth_from_path(&path),
        Some(ResolvedAuth::ChatGptOAuth {
            access_token,
            refresh_token: Some("codex-refresh".to_string()),
            expires: 4102444800,
            account_id: Some("account-id".to_string())
        })
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn codex_auth_file_without_refresh_or_jwt_is_lenient() {
    let root = test_dir("noel-config-codex-lenient");
    fs::create_dir_all(&root).expect("create root");
    let path = root.join("auth.json");
    fs::write(&path, r#"{"tokens":{"access_token":"opaque-token"}}"#).expect("write auth");

    assert_eq!(
        read_codex_chatgpt_oauth_from_path(&path),
        Some(ResolvedAuth::ChatGptOAuth {
            access_token: "opaque-token".to_string(),
            refresh_token: None,
            expires: 0,
            account_id: None,
        })
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn claude_credentials_capture_refresh_and_expiry() {
    let root = test_dir("noel-config-claude-expiry");
    fs::create_dir_all(&root).expect("create root");
    let path = root.join(".credentials.json");
    fs::write(
            &path,
            r#"{"claudeAiOauth":{"accessToken":"claude-access","refreshToken":"claude-refresh","expiresAt":4102444800000}}"#,
        )
        .expect("write credentials");

    assert_eq!(
        read_claude_code_oauth_from_path(&path),
        Some(ResolvedAuth::ClaudeCodeOAuth {
            access_token: "claude-access".to_string(),
            refresh_token: Some("claude-refresh".to_string()),
            expires: 4102444800,
        })
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn claude_credentials_without_refresh_or_expiry_are_lenient() {
    let root = test_dir("noel-config-claude-lenient");
    fs::create_dir_all(&root).expect("create root");
    let path = root.join(".credentials.json");
    fs::write(
        &path,
        r#"{"claudeAiOauth":{"accessToken":"claude-access"}}"#,
    )
    .expect("write credentials");

    assert_eq!(
        read_claude_code_oauth_from_path(&path),
        Some(ResolvedAuth::ClaudeCodeOAuth {
            access_token: "claude-access".to_string(),
            refresh_token: None,
            expires: 0,
        })
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn stored_oauth_threads_refresh_metadata_with_env_indirection() {
    let _guard = ENV_LOCK.lock().expect("lock env");
    let saved = save_env("CODEX_REFRESH_TOKEN");
    env::set_var("CODEX_REFRESH_TOKEN", "indirected-refresh");

    let mut auth = AuthData::default();
    auth.insert(
        "openai-codex",
        DEFAULT_ACCOUNT_NAME,
        AuthCredential::OAuth {
            access_token: "codex-access".to_string(),
            refresh_token: Some("env:CODEX_REFRESH_TOKEN".to_string()),
            expires: 4102444800,
            account_id: Some("account-id".to_string()),
        },
    );

    let resolved = auth_for_provider(&auth, "openai-codex", None).expect("resolved");
    assert_eq!(resolved.refresh_token(), Some("indirected-refresh"));
    assert_eq!(resolved.expires(), Some(4102444800));

    restore_env("CODEX_REFRESH_TOKEN", saved.1);
}

#[test]
fn expiry_helpers_treat_zero_as_unknown() {
    assert!(!is_expired(None, 1000));
    assert!(!expires_within(None, 1000, 60));
    assert!(is_expired(Some(1000), 1000));
    assert!(!is_expired(Some(1001), 1000));
    assert!(expires_within(Some(1000), 999, 60));
    assert!(!expires_within(Some(1000), 100, 60));
    assert!(expires_within(Some(u64::MAX), u64::MAX - 1, 60));
}

fn fake_jwt(payload: &str) -> String {
    let encode = |json: &str| URL_SAFE_NO_PAD.encode(json.as_bytes());
    format!(
        "{}.{}.signature",
        encode(r#"{"alg":"none"}"#),
        encode(payload)
    )
}

#[test]
fn usage_cache_round_trips_through_file() {
    let root = test_dir("noel-config-usage-cache");
    let path = root.join("agent").join("usage-cache.json");
    let mut cache = UsageCache {
        refreshed_at: 1_000,
        accounts: BTreeMap::new(),
    };
    cache.accounts.insert(
        "openai-codex:default".to_string(),
        AccountUsage {
            plan: Some("plus".to_string()),
            windows: vec![UsageWindow {
                label: "5h".to_string(),
                used_pct: 42.5,
                resets_at: Some(1_800_000_000),
                active: None,
                severity: None,
            }],
            balance: Some(Balance {
                amount: 10.5,
                currency: "USD".to_string(),
            }),
            fetched_at: 1_000,
        },
    );

    write_usage_cache(&path, &cache).expect("write usage cache");
    let loaded = read_usage_cache(&path).expect("read usage cache");
    assert_eq!(loaded, Some(cache));

    let missing = read_usage_cache(&root.join("missing.json")).expect("missing cache");
    assert_eq!(missing, None);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn write_file_atomic_round_trips_contents() {
    let root = test_dir("noel-config-atomic-write");
    fs::create_dir_all(&root).expect("create root");
    let path = root.join("state.json");

    write_file_atomic(&path, br#"{"ok":true}"#).expect("write");

    assert_eq!(fs::read(&path).expect("read"), br#"{"ok":true}"#);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn write_file_atomic_overwrites_existing_file() {
    let root = test_dir("noel-config-atomic-overwrite");
    fs::create_dir_all(&root).expect("create root");
    let path = root.join("state.json");
    fs::write(&path, b"old").expect("write old");

    write_file_atomic(&path, b"new").expect("write new");

    assert_eq!(fs::read_to_string(&path).expect("read"), "new");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn write_file_atomic_leaves_no_temp_file() {
    let root = test_dir("noel-config-atomic-clean");
    fs::create_dir_all(&root).expect("create root");
    let path = root.join("state.json");

    write_file_atomic(&path, b"data").expect("write");

    let entries: Vec<_> = fs::read_dir(&root).expect("read dir").collect();
    assert_eq!(entries.len(), 1);
    let _ = fs::remove_dir_all(root);
}

fn api_key(key: &str) -> AuthCredential {
    AuthCredential::ApiKey {
        key: key.to_string(),
    }
}

#[cfg(unix)]
fn mode_of(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).expect("metadata").permissions().mode() & 0o777
}

#[test]
fn auth_file_updates_are_private_and_change_only_their_entry() {
    let root = test_dir("noel-config-auth-update");
    let auth_path = root.join("agent").join("auth.json");

    update_auth_file(&auth_path, |auth| {
        auth.insert("anthropic", DEFAULT_ACCOUNT_NAME, api_key("first"))
    })
    .expect("first update");
    #[cfg(unix)]
    {
        assert_eq!(mode_of(&auth_path), 0o600);
        assert_eq!(mode_of(auth_path.parent().unwrap()), 0o700);
    }

    // Another process rewrote the file (world-readable) since this one
    // read it: an update keeps its entries and restores owner-only mode.
    let mut current = read_auth_file(&auth_path).expect("read");
    current.insert("anthropic", DEFAULT_ACCOUNT_NAME, api_key("rotated"));
    current.insert("openai", DEFAULT_ACCOUNT_NAME, api_key("other"));
    fs::write(
        &auth_path,
        serde_json::to_string(&current).expect("serialize"),
    )
    .expect("rewrite");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&auth_path, fs::Permissions::from_mode(0o644)).expect("chmod");
    }
    update_auth_file(&auth_path, |auth| {
        auth.insert("google", "work", api_key("google"))
    })
    .expect("second update");
    let stored = read_auth_file(&auth_path).expect("read back");
    assert_eq!(
        stored.credential("anthropic", DEFAULT_ACCOUNT_NAME),
        Some(&api_key("rotated"))
    );
    assert_eq!(
        stored.credential("openai", DEFAULT_ACCOUNT_NAME),
        Some(&api_key("other"))
    );
    assert_eq!(
        stored.credential("google", "work"),
        Some(&api_key("google"))
    );
    #[cfg(unix)]
    assert_eq!(mode_of(&auth_path), 0o600);
    let mut names = fs::read_dir(auth_path.parent().unwrap())
        .expect("read dir")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .into_string()
                .expect("utf-8")
        })
        .collect::<Vec<_>>();
    names.sort();
    assert_eq!(names, ["auth.json", "auth.json.flock"]);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn auth_file_update_never_overwrites_a_file_it_cannot_parse() {
    let root = test_dir("noel-config-auth-unparsable");
    fs::create_dir_all(&root).expect("create root");
    let auth_path = root.join("auth.json");
    fs::write(&auth_path, "{ not json").expect("write");

    let result = update_auth_file(&auth_path, |auth| {
        auth.insert("anthropic", DEFAULT_ACCOUNT_NAME, api_key("key"))
    });

    assert!(matches!(result, Err(ConfigError::Parse { .. })));
    assert_eq!(fs::read_to_string(&auth_path).expect("read"), "{ not json");
    let _ = fs::remove_dir_all(root);
}

/// Durable's example (spec section 8.7, upstream 922c3ff2).
#[test]
fn compaction_resolves_per_conversation_and_model() {
    let settings: CompactionSettings = serde_json::from_value(serde_json::json!({
        "keepRecentTokens": 20000,
        "modelOverrides": {"local/small-32k": {"keepRecentTokens": 8000, "enabled": true}}
    }))
    .unwrap();
    let conversation: CompactionSettings =
        serde_json::from_value(serde_json::json!({"keepRecentTokens": 60000})).unwrap();
    let large = resolve_compaction(&settings, Some(&conversation), "large/1m");
    assert_eq!(large.keep_recent_tokens, Some(60000));
    // The settings' model override beats the conversation's general field.
    let small = resolve_compaction(&settings, Some(&conversation), "local/small-32k");
    assert_eq!(small.keep_recent_tokens, Some(8000));
    // Without the conversation's: the settings'.
    assert_eq!(
        resolve_compaction(&settings, None, "large/1m").keep_recent_tokens,
        Some(20000)
    );
    // A conversation's `enabled: false` turns compaction off for every
    // model, even one whose settings override enables it.
    let off: CompactionSettings =
        serde_json::from_value(serde_json::json!({"enabled": false})).unwrap();
    assert_eq!(
        resolve_compaction(&settings, Some(&off), "local/small-32k").enabled,
        Some(false)
    );
    // Unless the conversation's own override for that model says so.
    let on_for_small: CompactionSettings = serde_json::from_value(serde_json::json!({
        "enabled": false,
        "modelOverrides": {"local/small-32k": {"enabled": true}}
    }))
    .unwrap();
    assert_eq!(
        resolve_compaction(&settings, Some(&on_for_small), "local/small-32k").enabled,
        Some(true)
    );
}

#[test]
fn auth_file_lock_excludes_other_holders_until_released() {
    let root = test_dir("noel-config-auth-lock");
    let auth_path = root.join("auth.json");

    let held = AuthFileLock::try_acquire(&auth_path)
        .expect("lock")
        .expect("free lock");
    // flock locks belong to the open file, so a second open in the same
    // process contends exactly like another process would.
    assert!(AuthFileLock::try_acquire(&auth_path)
        .expect("try lock")
        .is_none());
    drop(held);
    assert!(AuthFileLock::try_acquire(&auth_path)
        .expect("try lock")
        .is_some());
    let _ = fs::remove_dir_all(root);
}

/// A symlinked `auth.json` shares its target's lock (upstream
/// 081a6e87), so two agent dirs pointing at one file cannot both write.
#[cfg(unix)]
#[test]
fn a_symlinked_auth_file_shares_its_targets_lock() {
    let root = test_dir("noel-config-auth-lock-link");
    let shared = root.join("shared");
    let agent = root.join("agent");
    fs::create_dir_all(&shared).unwrap();
    fs::create_dir_all(&agent).unwrap();
    fs::write(shared.join("auth.json"), "{}").unwrap();
    std::os::unix::fs::symlink(shared.join("auth.json"), agent.join("auth.json")).unwrap();
    let held = AuthFileLock::try_acquire(&shared.join("auth.json"))
        .expect("lock")
        .expect("free lock");
    assert!(AuthFileLock::try_acquire(&agent.join("auth.json"))
        .expect("try lock")
        .is_none());
    drop(held);
    let _ = fs::remove_dir_all(root);
}

/// A project written for pi loads whole from its `.pi/`: settings,
/// extensions with their manifests, themes, skills, prompt templates and
/// the prompt files. A `.noel/` directory replaces it and never adds to
/// it.
#[test]
fn projects_written_for_pi_load_from_pi_until_they_have_a_noel_dir() {
    let root = test_dir("noel-config-pi-project");
    let cwd = root.join("repo");
    let write = |path: &str, content: &str| {
        let path = cwd.join(path);
        fs::create_dir_all(path.parent().expect("parent")).expect("create dir");
        fs::write(path, content).expect("write file");
    };
    let skill = |name: &str| format!("---\nname: {name}\ndescription: A skill.\n---\nDo it.\n");
    write(".pi/settings.json", r#"{"packages":["package"]}"#);
    write(".pi/extensions/tool", "pi extension");
    write(".pi/extensions/tool.pi-extension.json", "{}");
    write(".pi/themes/dusk.json", r#"{"name":"dusk"}"#);
    write(".pi/skills/release/SKILL.md", &skill("release"));
    write(".pi/prompts/fix.md", "Fix $1.\n");
    write(".pi/SYSTEM.md", "pi system\n");
    write(".pi/APPEND_SYSTEM.md", "pi append\n");
    write(
        "package/package.json",
        r#"{"pi":{"skills":["skills/pi-key.md"]}}"#,
    );
    write("package/skills/pi-key.md", &skill("pi-key"));
    write("package/skills/noel-key.md", &skill("noel-key"));
    let load = || {
        let paths = ConfigPaths {
            cwd: cwd.clone(),
            agent_dir: root.join("agent"),
            session_dir: root.join("agent/sessions"),
            settings_path: root.join("missing-settings.json"),
            project_settings_path: project_config_dir(&cwd).join("settings.json"),
            auth_path: root.join("missing-auth.json"),
            models_path: root.join("missing-models.json"),
            model_cache_path: root.join("missing-model-cache.json"),
            keybindings_path: root.join("missing-keybindings.json"),
        };
        load_config_with_resources(paths, true, &ResourceOptions::default()).expect("load config")
    };
    let files = |resources: &[ResourceFile]| {
        let names = resources.iter().map(|resource| resource.name.clone());
        names.collect::<Vec<_>>()
    };
    let skills = |config: &LoadedConfig| {
        let names = config.skills.iter().map(|skill| skill.name.clone());
        names.collect::<Vec<_>>()
    };

    let config = load();
    assert_eq!(
        config.paths.project_settings_path,
        cwd.join(".pi/settings.json")
    );
    assert_eq!(files(&config.extensions), ["tool"]);
    assert_eq!(files(&config.themes), ["dusk"]);
    assert_eq!(skills(&config), ["release", "pi-key"]);
    assert_eq!(
        config.skills[0].file_path,
        cwd.join(".pi/skills/release/SKILL.md")
    );
    assert_eq!(config.prompt_templates.len(), 1);
    assert_eq!(config.prompt_templates[0].name, "fix");
    assert_eq!(config.system_prompt.as_deref(), Some("pi system\n"));
    assert_eq!(config.append_system_prompt, ["pi append\n"]);
    assert!(config.diagnostics.is_empty(), "{:?}", config.diagnostics);

    // A package's `noel` key is read before its `pi` key.
    write(
        "package/package.json",
        r#"{"noel":{"skills":["skills/noel-key.md"]},"pi":{"skills":["skills/pi-key.md"]}}"#,
    );
    assert_eq!(skills(&load()), ["release", "noel-key"]);

    // With a `.noel/`, nothing of `.pi/` is read: not its settings (the
    // package), nor any resource the new directory lacks.
    write(".noel/skills/deploy/SKILL.md", &skill("deploy"));
    let config = load();
    assert_eq!(
        config.paths.project_settings_path,
        cwd.join(".noel/settings.json")
    );
    assert_eq!(skills(&config), ["deploy"]);
    assert!(config.extensions.is_empty() && config.themes.is_empty());
    assert!(config.prompt_templates.is_empty());
    assert_eq!(config.system_prompt, None);
    assert!(config.append_system_prompt.is_empty());

    let _ = fs::remove_dir_all(root);
}

/// The parity tests' one substitution: upstream's fixtures name the
/// product, its variables and a project's directory, and nothing else of
/// upstream's is renamed.
#[test]
fn upstream_text_takes_noels_names_only_for_what_noel_renamed() {
    assert_eq!(
        with_noel_names("running inside <product>, a coding agent harness"),
        "running inside noel, a coding agent harness"
    );
    assert_eq!(
        with_noel_names("You can inspect PI_* environment variables"),
        "You can inspect NOEL_* environment variables"
    );
    assert_eq!(
        with_noel_names("<location><tmp>/repo/.pi/skills/release/SKILL.md</location>"),
        "<location><tmp>/repo/.noel/skills/release/SKILL.md</location>"
    );
    assert_eq!(with_noel_names("<tmp>/repo/.pi"), "<tmp>/repo/.noel");
    assert_eq!(with_noel_names(".pi/settings.json"), ".noel/settings.json");
    for kept in [
        "PI_CODING_AGENT_DIR",
        "An untrusted project's .pi files are ignored.",
        "<tmp>/repo/.pip/cache",
        "<tmp>/repo/.agents/skills/deploy/SKILL.md",
        "https://github.com/earendil-works/pi",
        "pi-messages",
    ] {
        assert_eq!(with_noel_names(kept), kept);
    }
    assert_eq!(
        value_with_noel_names(&serde_json::json!({
            "repo/.pi/SYSTEM.md": ["<tmp>/repo/.pi", 1, null],
        })),
        serde_json::json!({ "repo/.noel/SYSTEM.md": ["<tmp>/repo/.noel", 1, null] })
    );
}

#[test]
fn pi_agent_dir_is_imported_once_without_files_noel_cannot_load() {
    let root = test_dir("noel-config-pi-import");
    let from = root.join(".pi/agent");
    let to = root.join(".noel");
    fs::create_dir_all(from.join("sessions/archive")).expect("create sessions");
    fs::create_dir_all(from.join("skills")).expect("create skills");
    fs::write(from.join("settings.json"), r#"{"theme":"dark"}"#).expect("write settings");
    fs::write(
        from.join("auth.json"),
        r#"{"openai":{"default":{"type":"api_key","key":"dummy"}}}"#,
    )
    .expect("write auth");
    fs::write(from.join("models.json"), "{ not json").expect("write models");
    fs::write(from.join("keybindings.json"), r#"{"submit":["enter"]}"#).expect("write keybindings");
    fs::write(from.join("model-cache.json"), "{}").expect("write model cache");
    fs::write(from.join("skills/review.md"), "review").expect("write skill");
    fs::write(from.join("sessions/archive/old.jsonl"), "{}\n").expect("write session");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(from.join("auth.json"), fs::Permissions::from_mode(0o600))
            .expect("restrict auth");
        std::os::unix::fs::symlink("/elsewhere/skill", from.join("skills/linked"))
            .expect("link skill");
    }

    let import = import_agent_dir(&from, &to)
        .expect("import")
        .expect("imported");
    assert_eq!(
        import.imported,
        [
            "settings.json",
            "keybindings.json",
            "auth.json",
            "skills",
            "sessions"
        ]
    );
    assert_eq!(import.skipped.len(), 1);
    assert_eq!(import.skipped[0].0, "models.json");
    assert!(
        import.skipped[0].1.contains("failed to parse"),
        "{:?}",
        import.skipped
    );
    assert_eq!(
        fs::read_to_string(to.join("sessions/archive/old.jsonl")).expect("read session"),
        "{}\n"
    );
    assert!(!to.join("models.json").exists());
    assert!(!to.join("model-cache.json").exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(to.join("auth.json"))
            .expect("auth")
            .permissions();
        assert_eq!(mode.mode() & 0o777, 0o600);
        assert_eq!(
            fs::read_link(to.join("skills/linked")).expect("read link"),
            Path::new("/elsewhere/skill")
        );
    }
    assert_eq!(
        fs::read_to_string(from.join("models.json")).expect("pi models"),
        "{ not json"
    );
    assert_eq!(import_agent_dir(&from, &to).expect("second run"), None);
    let entries = fs::read_dir(&root)
        .expect("read root")
        .map(|entry| entry.expect("entry").file_name())
        .collect::<BTreeSet<_>>();
    assert_eq!(entries, BTreeSet::from([".noel".into(), ".pi".into()]));

    let _ = fs::remove_dir_all(root);
}

#[cfg(unix)]
#[test]
fn failed_pi_import_leaves_nothing_behind_and_runs_again() {
    use std::os::unix::fs::PermissionsExt;
    let root = test_dir("noel-config-pi-import-failure");
    let from = root.join(".pi/agent");
    let to = root.join(".noel");
    fs::create_dir_all(from.join("sessions")).expect("create sessions");
    fs::write(from.join("settings.json"), "{}").expect("write settings");
    let unreadable = from.join("sessions/unreadable.jsonl");
    fs::write(&unreadable, "{}\n").expect("write session");
    fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000)).expect("chmod");
    // Privileged users read it anyway, so the failure cannot be staged.
    if fs::read(&unreadable).is_err() {
        let error = import_agent_dir(&from, &to).expect_err("import fails");
        assert!(error.to_string().contains("unreadable.jsonl"), "{error}");
        let entries = fs::read_dir(&root)
            .expect("read root")
            .map(|entry| entry.expect("entry").file_name())
            .collect::<Vec<_>>();
        assert_eq!(entries, [".pi"]);
    }
    fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o600)).expect("chmod");
    assert!(import_agent_dir(&from, &to).expect("import").is_some());
    assert!(to.join("sessions/unreadable.jsonl").exists());

    let _ = fs::remove_dir_all(root);
}

fn test_dir(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("{name}-{}-{}", std::process::id(), unique_suffix()))
}

/// `load_config` with neither the developer's `~/.agents/skills` nor
/// the environment's resource flags: a test's config is its temp tree
/// alone, whatever the machine running it has at home.
fn load_config_hermetic(
    paths: ConfigPaths,
    project_trusted: bool,
) -> Result<LoadedConfig, ConfigError> {
    load_config_with_resources(
        paths,
        project_trusted,
        &ResourceOptions {
            home_dir: None,
            ..ResourceOptions::default()
        },
    )
}

fn unique_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default()
}

fn save_env(name: &'static str) -> (&'static str, Option<String>) {
    (name, env::var(name).ok())
}

fn restore_env(name: &str, value: Option<String>) {
    match value {
        Some(value) => env::set_var(name, value),
        None => env::remove_var(name),
    }
}
