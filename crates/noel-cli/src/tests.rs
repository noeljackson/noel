use super::*;

use noel_config::{PromptTemplate, Skill, SourceInfo, SourceScope};
use noel_core::{
    run_user_turn_streaming_events_with_media, ConversationMessage, MessageRole, RunControl,
    TurnEvent,
};
use std::io::BufRead;

/// The tool settings a command line leaves, starting from `settings`.
fn tool_flags(arguments: &[&str], settings: serde_json::Value) -> Result<Settings> {
    let arguments = std::iter::once("noel").chain(arguments.iter().copied());
    let cli = Cli::try_parse_from(normalized_cli_args(arguments.map(String::from)))?;
    let mut settings: Settings = serde_json::from_value(settings)?;
    apply_tool_flags(&cli, &mut settings)?;
    Ok(settings)
}

#[test]
fn tool_flags_select_tools_as_upstream_does() {
    let names = |names: &[&str]| Some(names.iter().map(ToString::to_string).collect::<Vec<_>>());
    let none = serde_json::json!({});

    // Plain names and patterns are an allowlist.
    let settings = tool_flags(&["--tools", "read, grep,mcp_*"], none.clone()).unwrap();
    assert_eq!(settings.enabled_tools, names(&["read", "grep", "mcp_*"]));
    assert_eq!(settings.default_tools, None);

    // Only +name/-name entries change the default selection instead.
    let settings = tool_flags(&["-t", "-bash,+grep,-write"], none.clone()).unwrap();
    assert_eq!(settings.enabled_tools, None);
    let defaults = settings.default_tools.expect("default selection");
    assert!(defaults.contains(&"grep".to_string()) && defaults.contains(&"read".to_string()));
    assert!(!defaults.contains(&"bash".to_string()) && !defaults.contains(&"write".to_string()));
    let settings = tool_flags(
        &["--tools", "+ls"],
        serde_json::json!({ "defaultTools": ["read", "-read", "grep"] }),
    )
    .unwrap();
    assert_eq!(settings.default_tools, names(&["grep", "ls"]));

    // --no-builtin-tools empties the default selection; extension tools
    // are not part of it, so they stay.
    let settings = tool_flags(&["-nbt"], serde_json::json!({ "defaultTools": ["read"] })).unwrap();
    assert_eq!(
        (settings.default_tools, settings.enabled_tools),
        (names(&[]), None)
    );
    let settings = tool_flags(&["-nbt"], serde_json::json!({ "enabledTools": ["read"] }));
    assert_eq!(settings.unwrap().enabled_tools, None);
    let settings = tool_flags(&["--no-builtin-tools", "--tools", "+read"], none.clone()).unwrap();
    assert_eq!(
        (settings.default_tools, settings.enabled_tools),
        (names(&["read"]), None)
    );

    // --no-tools leaves only what --tools names.
    let settings = tool_flags(&["--no-tools"], none.clone()).unwrap();
    assert_eq!(settings.enabled_tools, names(&[]));
    let settings = tool_flags(&["-nt", "--tools", "+read"], none.clone()).unwrap();
    assert_eq!(settings.enabled_tools, names(&["read"]));
    let settings = tool_flags(&["--no-tools", "--tools", "read,ext_*"], none.clone()).unwrap();
    assert_eq!(settings.enabled_tools, names(&["read", "ext_*"]));

    let settings = tool_flags(&["-xt", "web_*,todo"], none.clone()).unwrap();
    assert_eq!(settings.excluded_tools, ["web_*", "todo"]);

    for (tools, error) in [
        (
            "read,+grep",
            "--tools: tool names cannot be mixed with +name or -name entries",
        ),
        (
            "+re*",
            "--tools: +name and -name entries take exact tool names, not patterns: +re*",
        ),
    ] {
        let failure = tool_flags(&["--tools", tools], none.clone()).unwrap_err();
        assert_eq!(failure.to_string(), error);
    }
}

#[test]
fn diff_panel_lists_files_under_cwd_relative_to_it() {
    let entries = collect_diff_file_entries(
        Path::new("/no-such-dir/work/cwd"),
        &[
            "/no-such-dir/work/cwd/src/file.txt".to_string(),
            "/elsewhere/notes.md".to_string(),
            "already/relative.rs".to_string(),
        ],
    );
    let paths = entries
        .iter()
        .map(|entry| entry.path.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        paths,
        vec!["src/file.txt", "/elsewhere/notes.md", "already/relative.rs"]
    );
}

fn picker_summary(first_prompt: Option<&str>, unfinished: bool) -> noel_core::SessionSummary {
    noel_core::SessionSummary {
        path: PathBuf::from("/tmp/ignored.jsonl"),
        session_id: "1791668953663-3417381-0".into(),
        cwd: PathBuf::from("/repo"),
        name: None,
        labels: Default::default(),
        parent_session_id: None,
        branch_summary: None,
        modified: Some(std::time::SystemTime::now()),
        message_count: 12,
        first_prompt: first_prompt.map(str::to_string),
        active_model: Some("zai-coding/glm-5.3".into()),
        has_unfinished_turn: unfinished,
    }
}

/// HTML comments are hidden (upstream tui 54280c4f): inline, on a row
/// of their own, and across rows.
#[test]
fn html_comments_are_not_shown() {
    assert_eq!(
        without_html_comments("a <!-- b --> c", false),
        ("a  c".to_string(), false)
    );
    assert_eq!(
        without_html_comments("<!-- release-notes:start -->", false),
        (String::new(), false)
    );
    assert_eq!(
        without_html_comments("x <!-- open", false),
        ("x ".to_string(), true)
    );
    assert_eq!(
        without_html_comments("still inside", true),
        (String::new(), true)
    );
    assert_eq!(
        without_html_comments("end --> after", true),
        (" after".to_string(), false)
    );
}

#[test]
fn assistant_markdown_renders_styled_spans_but_plain_logical_lines() {
    let entry = TuiEntry {
        kind: TuiEntryKind::Assistant,
        text: "run `cargo build` then **commit**\nsee [docs](https://example.org)".to_string(),
    };
    let palette = ThemePalette::default();
    // Logical lines (selection/copy source) stay plain and complete,
    // including the markdown syntax and link URL.
    let logical = render_entry_lines(std::slice::from_ref(&entry), &palette);
    let logical_text: Vec<String> = logical
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect();
    assert_eq!(
        logical_text,
        vec![
            format!("{BULLET} run `cargo build` then **commit**"),
            "  see [docs](https://example.org)".to_string(),
            "".to_string()
        ]
    );
    // Display rows get styles after wrapping; anchors unchanged.
    let mut rows = pending_entry_rows(
        &entry,
        0,
        80,
        &palette,
        &PreviewState::default(),
        EntryScrollback::default(),
    );
    let anchor_starts: Vec<TranscriptPosition> = rows.iter().map(|row| row.start).collect();
    restyle_rows_with_markdown(
        &mut rows,
        &entry,
        |line| logical_text.get(line).cloned(),
        Style::default().fg(palette.foreground),
        palette.accent,
    );
    // Anchors identical after restyling.
    assert_eq!(
        anchor_starts,
        rows.iter().map(|row| row.start).collect::<Vec<_>>()
    );
    let display: Vec<String> = rows
        .iter()
        .map(|row| {
            row.line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect();
    // Bold span carries the modifier; link row displays only "docs".
    let bold = rows[0].line.spans.iter().any(|span| {
        span.content.contains("commit")
            && !span.content.contains("**")
            && span.style.add_modifier.contains(Modifier::BOLD)
    });
    assert!(bold, "bold marker not styled: {display:?}");
    let code =
        rows[0].line.spans.iter().any(|span| {
            span.content.contains("cargo build") && span.style.fg == Some(palette.accent)
        });
    assert!(code, "code span not accented: {display:?}");
    assert_eq!(display[1], "  see docs");
    let link = rows[1]
        .line
        .spans
        .iter()
        .find(|span| span.content == "docs")
        .expect("link span");
    assert!(link.style.add_modifier.contains(Modifier::UNDERLINED));
}

#[test]
fn ordered_lists_display_sequential_numbers() {
    // A source that numbers every item 1. (or restarts numbers) still
    // displays 1. 2. 3., matching upstream's list renderer; a plain
    // line ends the sequence and a later list starts over at its own
    // first marker.
    let entry = TuiEntry {
        kind: TuiEntryKind::Assistant,
        text: "intro:\n1. first\n1. second\n3. third skipped number\n\nplain\n\n5. five\n6. six"
            .to_string(),
    };
    let palette = ThemePalette::default();
    let logical = render_entry_lines(std::slice::from_ref(&entry), &palette);
    let logical_text: Vec<String> = logical
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect();
    let mut rows = pending_entry_rows(
        &entry,
        0,
        80,
        &palette,
        &PreviewState::default(),
        EntryScrollback::default(),
    );
    restyle_rows_with_markdown(
        &mut rows,
        &entry,
        |line| logical_text.get(line).cloned(),
        Style::default().fg(palette.foreground),
        palette.accent,
    );
    let display: Vec<String> = rows
        .iter()
        .map(|row| {
            row.line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect();
    let item_text = |needle: &str| {
        display
            .iter()
            .find(|line| line.contains(needle))
            .cloned()
            .unwrap_or_else(|| panic!("missing {needle}: {display:?}"))
    };
    // The first logical line carries the "● " echo prefix, so the
    // list starts on the second line.
    assert_eq!(item_text("intro"), "● intro:");
    assert_eq!(item_text("first"), "  1. first");
    assert_eq!(item_text("second"), "  2. second");
    assert_eq!(
        item_text("third skipped number"),
        "  3. third skipped number"
    );
    assert_eq!(item_text("plain"), "  plain");
    // The blank line ended the list, so this one restarts at its own 5.
    assert_eq!(item_text("five"), "  5. five");
    assert_eq!(item_text("six"), "  6. six");
}

#[test]
fn branch_selector_nests_children_and_marks_the_current() {
    let root = std::env::temp_dir().join(format!("noel-cli-tree-sel-{}", unique_temp_suffix()));
    use serde_json::json;
    fs::create_dir_all(&root).unwrap();
    let session_dir = root.join("sessions");
    fs::create_dir_all(&session_dir).unwrap();
    let write = |id: &str, prompt: &str, parent: Option<&str>| {
        let path = session_dir.join(format!("{id}.jsonl"));
        let mut records =
            vec![json!({"type": "started", "session_id": id, "cwd": root.join("cwd")})];
        if let Some(parent) = parent {
            records.push(json!({
                "type": "metadata",
                "name": null,
                "labels": [],
                "parent_session_id": parent
            }));
        }
        let message = json!({"type": "message", "message": {"role": "user", "content": prompt}});
        records.push(message);
        let mut text = String::new();
        for record in records {
            text.push_str(&record.to_string());
            text.push('\n');
        }
        fs::write(path, text).unwrap();
    };
    write("parent", "fix the login flow", None);
    std::thread::sleep(std::time::Duration::from_millis(20));
    write("fork-a", "try oauth instead", Some("parent"));
    std::thread::sleep(std::time::Duration::from_millis(20));
    write("fork-b", "try device code", Some("parent"));

    let mut config = account_test_config(AuthData::default());
    config.paths.session_dir = session_dir.clone();
    let items = session_tree_items(&config, Some("fork-a")).expect("items");
    assert_eq!(items.len(), 3, "{items:?}");
    let parent_row = items.iter().find(|i| i.value == "parent").unwrap();
    assert!(parent_row.label.contains("(2 branches)"), "{items:?}");
    assert!(!parent_row.active);
    let fork_a = items.iter().find(|i| i.value == "fork-a").unwrap();
    assert!(fork_a.label.contains("• "), "{items:?}");
    assert!(fork_a.active);
    // Children carry connectors; the last is the corner.
    assert!(items.iter().any(|i| i.label.contains("├─ ")), "{items:?}");
    assert!(items.iter().any(|i| i.label.contains("└─ ")), "{items:?}");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn session_tree_renders_branches_with_connectors_and_marks_the_current() {
    let root = std::env::temp_dir().join(format!("noel-cli-tree-{}", unique_temp_suffix()));
    use serde_json::json;
    fs::create_dir_all(&root).unwrap();
    let session_dir = root.join("sessions");
    fs::create_dir_all(&session_dir).unwrap();
    let write = |id: &str, prompt: &str, parent: Option<&str>| {
        let path = session_dir.join(format!("{id}.jsonl"));
        let mut records =
            vec![json!({"type": "started", "session_id": id, "cwd": root.join("cwd")})];
        if let Some(parent) = parent {
            records.push(json!({"type": "metadata", "name": null, "labels": [], "parent_session_id": parent}));
        }
        records.push(json!({"type": "message", "message": {"role": "user", "content": prompt, "media": [], "tool_call_id": null, "tool_name": null, "tool_calls": [], "aborted": false, "thinking": ""}}));
        let mut text = String::new();
        for record in records {
            text.push_str(&record.to_string());
            text.push('\n');
        }
        fs::write(path, text).unwrap();
    };
    write("parent", "fix the login flow", None);
    std::thread::sleep(std::time::Duration::from_millis(20));
    write("fork-a", "try oauth instead", Some("parent"));
    std::thread::sleep(std::time::Duration::from_millis(20));
    write("fork-b", "try device code", Some("parent"));
    let tree = format_session_tree(&session_dir, Some("fork-a")).unwrap();
    let parent_row = tree
        .lines()
        .find(|line| line.contains("fix the login flow"))
        .unwrap();
    let fork_a_row = tree
        .lines()
        .find(|line| line.contains("try oauth instead"))
        .unwrap();
    let fork_b_row = tree
        .lines()
        .find(|line| line.contains("try device code"))
        .unwrap();
    // The parent leads with no bullet; both forks are its children with
    // connectors, the last one a corner.
    assert!(fork_a_row.contains("• "), "current session marked: {tree}");
    assert!(fork_b_row.contains("└─ "), "last child corners: {tree}");
    assert!(
        tree.lines().any(|line| line.contains("├─ ")),
        "a non-last child branches: {tree}"
    );
    assert!(
        parent_row.contains("(2 branches)"),
        "branch count noted: {tree}"
    );
    // Every session appears exactly once.
    assert_eq!(tree.lines().count(), 3, "{tree}");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn session_picker_label_shows_topic_count_model_and_unfinished() {
    let label = format_session_picker_label(&picker_summary(
        Some("fix the retry loop\nmore details"),
        true,
    ));
    assert_eq!(
        label,
        "fix the retry loop · 0s · 12 msg · zai-coding/glm-5.3 · unfinished"
    );
    // Long topics truncate with an ellipsis, one line only.
    let long = "x".repeat(80);
    let label = format_session_picker_label(&picker_summary(Some(&long), false));
    assert!(label.starts_with(&format!("{}… · ", "x".repeat(60))));
    assert!(!label.contains('\n'))
}

#[test]
fn session_picker_label_falls_back_to_session_id() {
    let label = format_session_picker_label(&picker_summary(None, false));
    assert_eq!(
        label,
        "1791668953663-3417381-0 · 0s · 12 msg · zai-coding/glm-5.3"
    );
}

#[test]
fn relative_time_label_uses_compact_units() {
    let label = |secs: u64| {
        relative_time_label(std::time::SystemTime::now() - std::time::Duration::from_secs(secs))
    };
    assert_eq!(label(5), "5s");
    assert_eq!(label(90), "1m");
    assert_eq!(label(7_200), "2h");
    assert_eq!(label(3 * 86_400), "3d");
}

fn clipboard_test_png(width: u32, height: u32) -> Vec<u8> {
    let image = image::DynamicImage::new_rgba8(width, height);
    let mut output = Cursor::new(Vec::new());
    image
        .write_to(&mut output, image::ImageFormat::Png)
        .unwrap();
    output.into_inner()
}

#[test]
fn clipboard_images_validate_resize_and_honor_block_setting() {
    let mut config = minimal_test_config();
    let png = clipboard_test_png(2100, 10);
    let media = clipboard_media(png.clone(), &config).unwrap();
    assert_eq!(media.width, Some(2000));
    assert!(media.path.is_none());
    assert_eq!(media.mime_type, "image/png");
    assert!(!media.data_base64.is_empty());
    config.settings.images = Some(ImageSettings {
        auto_resize: Some(false),
        block_images: Some(false),
    });
    assert_eq!(
        clipboard_media(png.clone(), &config).unwrap().width,
        Some(2100)
    );
    config.settings.images.as_mut().unwrap().block_images = Some(true);
    assert!(clipboard_media(png, &config)
        .unwrap_err()
        .to_string()
        .contains("blocked"));
    assert!(clipboard_media(b"not an image".to_vec(), &minimal_test_config()).is_err());
}

#[test]
fn auto_resize_downscales_large_png() {
    let original = png_bytes(3000, 1000);
    let resized = resize_image_if_needed(&original, "image/png")
        .expect("resize image")
        .expect("resized");

    assert_eq!(resized.mime_type, "image/png");
    let (width, height) = png_dimensions(&resized.bytes).expect("png dimensions");
    assert_eq!((width, height), (2000, 667));
    assert!(resized.bytes.len() < original.len());
}

#[test]
fn auto_resize_leaves_small_png_unchanged() {
    let original = png_bytes(10, 10);
    let resized = resize_image_if_needed(&original, "image/png").expect("resize image");

    assert!(resized.is_none());
}

#[test]
fn auto_resize_ignores_decode_failures() {
    let invalid_png_header = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01\x08\x04\0\0\0";
    let resized = resize_image_if_needed(invalid_png_header, "image/png").expect("resize image");

    assert!(resized.is_none());
}

#[test]
fn extension_protocol_rejects_unknown_manifest_protocol() {
    let root = std::env::temp_dir().join(format!(
        "noel-cli-extension-protocol-{}",
        unique_temp_suffix()
    ));
    fs::create_dir_all(&root).expect("create temp dir");
    let path = root.join("bad-ext");
    fs::write(&path, "").expect("write extension");
    fs::write(
        root.join("bad-ext.noel-extension.json"),
        r#"{"protocol":"bogus"}"#,
    )
    .expect("write manifest");
    let extension = ResourceFile {
        name: "bad-ext".to_string(),
        path,
        content: String::new(),
    };

    let error = extension_protocol(&extension).expect_err("reject protocol");

    assert!(error.to_string().contains("unsupported protocol bogus"));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn config_resource_state_enable_disable_updates_settings() {
    let root = std::env::temp_dir().join(format!(
        "noel-cli-config-resource-state-{}",
        unique_temp_suffix()
    ));
    fs::create_dir_all(&root).expect("create temp dir");
    let path = root.join("settings.json");

    mutate_settings_resource_state(&path, "extensions", "assist", true).expect("disable resource");
    mutate_settings_resource_state(&path, "extensions", "assist", false).expect("enable resource");
    mutate_settings_resource_state(&path, "prompts", "fix", true).expect("disable prompt");

    let settings = serde_json::from_str::<serde_json::Value>(
        &fs::read_to_string(&path).expect("read settings"),
    )
    .expect("parse settings");

    assert_eq!(
        settings["disabledResources"]["extensions"],
        serde_json::json!([])
    );
    assert_eq!(
        settings["disabledResources"]["prompts"],
        serde_json::json!(["fix"])
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn command_completions_include_loaded_resources() {
    let config = LoadedConfig {
        paths: test_config_paths(),
        settings: Settings::default(),
        auth: AuthData::default(),
        models: Vec::new(),
        model_scope: Vec::new(),
        image_models: Vec::new(),
        keybindings: Vec::new(),
        context_files: Vec::new(),
        extensions: vec![test_resource("json-ext")],
        skills: vec![test_skill("review", "Review code.\nCarefully.")],
        agents: Vec::new(),
        mcp_servers: Vec::new(),
        prompt_templates: vec![test_template(
            "fix",
            "Fix a bug",
            Some("<issue>"),
            "Fix $1.",
        )],
        themes: vec![test_resource("dark")],
        diagnostics: Vec::new(),
        system_prompt: None,
        append_system_prompt: Vec::new(),
    };

    assert_eq!(
        command_completions(&config, "/extension:j"),
        ["/extension:json-ext"]
    );
    // A skill and a template complete with what upstream shows: the
    // argument hint, the scope, and the description on one line.
    // The picker's rows carry the descriptions `/complete` used to inline.
    let entries = crate::tui::slash_completion_entries(&config, "/skill:r");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].command, "/skill:review");
    assert_eq!(entries[0].description, "[u] Review code. Carefully.");

    assert_eq!(
        resource_commands::format_skills(&config),
        "/skill:review\t[u] Review code. Carefully.\t/skills/review/SKILL.md"
    );
    assert_eq!(
        resource_commands::format_prompts(&config),
        "/fix\t<issue> \u{2014} [u] Fix a bug\t/prompts/fix.md"
    );
    let empty = minimal_test_config();
    assert_eq!(resource_commands::format_skills(&empty), "no skills");
    assert_eq!(resource_commands::format_prompts(&empty), "no prompts");
}

/// noel's own `/prompt <name> [input]`: `{{input}}` stands for the input,
/// which otherwise follows the template.
#[test]
fn prompt_command_fills_the_input_placeholder() {
    let fill = resource_commands::fill_prompt_template;
    assert_eq!(fill("fix {{input}} now", "the bug"), "fix the bug now");
    assert_eq!(fill("fix {{input}}", ""), "fix ");
    assert_eq!(fill("review", "the diff"), "review\n\nthe diff");
    assert_eq!(fill("review $1", ""), "review $1");
}

/// Typed input expands before it is submitted: a template fills in, and
/// a skill's command stands for its file.
#[test]
fn typed_prompts_expand_skills_and_templates() {
    let root = std::env::temp_dir().join(format!("noel-cli-typed-{}", unique_temp_suffix()));
    fs::create_dir_all(root.join("review")).unwrap();
    let skill_file = root.join("review/SKILL.md");
    fs::write(
        &skill_file,
        "---\ndescription: Review code.\n---\nCheck it.\n",
    )
    .unwrap();
    let mut config = minimal_test_config();
    config.skills = vec![Skill {
        file_path: skill_file.clone(),
        base_dir: root.join("review"),
        ..test_skill("review", "Review code.")
    }];
    config.prompt_templates = vec![test_template("fix", "Fix a bug", None, "Fix $1.")];

    let template = TypedPrompt::expand(&config, "/fix 42".into());
    assert_eq!(template.prompt, "Fix 42.");
    let skill = TypedPrompt::expand(&config, "/skill:review the parser".into());
    assert_eq!(
            skill.prompt,
            format!(
                "<skill name=\"review\" location=\"{}\">\nReferences are relative to {}.\n\nCheck it.\n</skill>\n\nthe parser",
                skill_file.display(),
                root.join("review").display()
            )
        );
    for text in ["/skill:nope x", "/nope x", "plain", "/read notes.txt"] {
        assert_eq!(TypedPrompt::expand(&config, text.into()).prompt, text);
    }
    // A prompt a command built is sent as it is.
    assert_eq!(TypedPrompt::from("/fix 42".to_string()).prompt, "/fix 42");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn themes_are_available_without_resources_and_do_not_duplicate_builtins() {
    let mut config = account_test_config(AuthData::default());
    let runtime = account_test_runtime("faux", None);
    config.themes = vec![test_resource("dark"), test_resource("custom")];
    assert_eq!(
        theme_names(&config),
        ["system", "light", "dark", "kimi", "custom"]
    );
    let selector = selector_for_kind(&config, &runtime, "theme").unwrap();
    assert!(selector.items[0].active);
    assert!(command_completions(&config, "/theme k").contains(&"/theme kimi".to_string()));
    assert!(command_completions(&config, "/accent m").contains(&"/accent magenta".to_string()));
    assert!(format_themes(&config).contains("system\nlight\ndark\nkimi"));
}

#[test]
fn diff_and_todo_widgets_use_semantic_colors() {
    use ratatui::{backend::TestBackend, style::Color};
    for name in BUILTIN_THEMES {
        let palette = TerminalTheme::builtin(name).unwrap().palette;
        assert_eq!(
            colorized_diff_line("+added", &palette).style.fg,
            Some(palette.success)
        );
        assert_eq!(
            colorized_diff_line("-removed", &palette).style.fg,
            Some(palette.error)
        );
        assert_eq!(
            colorized_diff_line("@@ hunk", &palette).style.fg,
            Some(palette.accent)
        );
        let app = TuiApp {
            todos: vec![
                TodoItem {
                    content: "completed".into(),
                    status: TodoStatus::Completed,
                },
                TodoItem {
                    content: "active".into(),
                    status: TodoStatus::InProgress,
                },
                TodoItem {
                    content: "pending".into(),
                    status: TodoStatus::Pending,
                },
            ],
            ..TuiApp::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(30, 6)).unwrap();
        terminal
            .draw(|frame| draw_todo_panel(frame, frame.area(), &app, &palette))
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(2, 2)].fg, palette.success);
        assert_eq!(buffer[(2, 3)].fg, palette.accent);
        assert_eq!(buffer[(2, 4)].fg, palette.muted);
        assert_eq!(buffer[(4, 4)].fg, Color::Reset);
    }
}

fn account_test_config(auth: AuthData) -> LoadedConfig {
    LoadedConfig {
        paths: test_config_paths(),
        settings: Settings::default(),
        auth,
        models: Vec::new(),
        model_scope: Vec::new(),
        image_models: Vec::new(),
        keybindings: Vec::new(),
        context_files: Vec::new(),
        extensions: Vec::new(),
        skills: Vec::new(),
        agents: Vec::new(),
        mcp_servers: Vec::new(),
        prompt_templates: Vec::new(),
        themes: Vec::new(),
        diagnostics: Vec::new(),
        system_prompt: None,
        append_system_prompt: Vec::new(),
    }
}

fn account_test_runtime(provider: &str, active_account: Option<&str>) -> Runtime {
    let mut session = SessionState::new("session-1", PathBuf::from("."));
    session.active_model = Some(ModelRef {
        provider: provider.to_string(),
        id: "echo".to_string(),
    });
    session.active_account = active_account.map(str::to_string);
    Runtime::new(session, ReloadableSystems::default())
}

#[test]
fn account_selector_lists_stored_accounts_with_auto_entry() {
    let mut auth = AuthData::default();
    auth.insert(
        "faux",
        "default",
        AuthCredential::ApiKey {
            key: "default-key".to_string(),
        },
    );
    auth.insert(
        "faux",
        "work",
        AuthCredential::ApiKey {
            key: "work-key".to_string(),
        },
    );
    let config = account_test_config(auth);
    let runtime = account_test_runtime("faux", Some("work"));

    let items = account_selector_items(&config, &runtime);
    assert_eq!(
        items
            .iter()
            .map(|item| (item.label.as_str(), item.value.as_str(), item.active))
            .collect::<Vec<_>>(),
        vec![
            ("auto (default resolution)", "", false),
            ("default", "default", false),
            ("work", "work", true),
        ]
    );
}

#[test]
fn account_picker_reads_auth_from_disk_not_the_snapshot() {
    let root = std::env::temp_dir().join(format!("noel-cli-acct-disk-{}", unique_temp_suffix()));
    fs::create_dir_all(&root).unwrap();
    let auth_path = root.join("auth.json");
    // The startup snapshot knew one stale account.
    let mut snapshot = AuthData::default();
    snapshot.insert("zeta", "old", AuthCredential::ApiKey { key: "old".into() });
    // Disk has the old one, a new one, and an OAuth token that the
    // snapshot would call long expired but is fine on disk.
    let mut disk = AuthData::default();
    disk.insert("zeta", "old", AuthCredential::ApiKey { key: "old".into() });
    disk.insert(
        "zeta",
        "added-later",
        AuthCredential::ApiKey { key: "new".into() },
    );
    disk.insert(
        "zeta",
        "oauth",
        AuthCredential::OAuth {
            access_token: "a".into(),
            refresh_token: Some("r".into()),
            expires: unix_seconds().unwrap_or_default() + 7 * 3600,
            account_id: None,
        },
    );
    noel_config::write_auth_file(&auth_path, &disk).unwrap();
    let mut config = account_test_config(snapshot);
    config.paths.auth_path = auth_path.clone();

    let items = account_selector_items_for(&config, "zeta", None);

    // Every stored account on disk is offered, including the one added
    // after startup.
    assert!(
        items.iter().any(|item| item.value == "added-later"),
        "{items:?}"
    );
    // The OAuth account is not marked expiring: its disk expiry is hours
    // away, whatever the snapshot said.
    let oauth = items.iter().find(|item| item.value == "oauth").unwrap();
    assert!(!oauth.label.contains("expires soon"), "{}", oauth.label);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn account_selector_labels_env_pseudo_account() {
    let saved = std::env::var("OPENROUTER_API_KEY").ok();
    // Test setup: the environment is process-wide, and writing it is unsafe since Rust 2024 while another thread may read it.
    unsafe { std::env::set_var("OPENROUTER_API_KEY", "test-openrouter-key") };
    let config = account_test_config(AuthData::default());
    let runtime = account_test_runtime("openrouter", None);

    let items = account_selector_items(&config, &runtime);
    assert!(items.iter().any(|item| {
        item.value == "env" && item.label == "env (environment, read-only)" && !item.active
    }));
    assert!(items[0].active);

    match saved {
        // Test setup: the environment is process-wide, and writing it is unsafe since Rust 2024 while another thread may read it.
        Some(value) => unsafe { std::env::set_var("OPENROUTER_API_KEY", value) },
        // Test setup: the environment is process-wide, and writing it is unsafe since Rust 2024 while another thread may read it.
        None => unsafe { std::env::remove_var("OPENROUTER_API_KEY") },
    }
}

#[test]
fn select_account_message_binds_and_clears_account() {
    let mut auth = AuthData::default();
    auth.insert(
        "faux",
        "work",
        AuthCredential::ApiKey {
            key: "work-key".to_string(),
        },
    );
    let mut config = account_test_config(auth);
    let mut runtime = account_test_runtime("faux", None);

    let message = select_from_selector_message(&mut config, &mut runtime, "/select account work")
        .expect("select account");
    assert_eq!(message, "account: work");
    assert_eq!(runtime.session().active_account, Some("work".to_string()));

    let message = select_from_selector_message(&mut config, &mut runtime, "/select account auto")
        .expect("clear account");
    assert_eq!(message, "account: auto");
    assert_eq!(runtime.session().active_account, None);
}

#[test]
fn logout_rewrites_only_its_entry_and_never_persists_runtime_keys() {
    let root = std::env::temp_dir().join(format!("noel-cli-logout-{}", unique_temp_suffix()));
    let auth_path = root.join("agent").join("auth.json");
    let oauth = |token: &str, expires| AuthCredential::OAuth {
        access_token: format!("{token}-access"),
        refresh_token: Some(format!("{token}-refresh")),
        expires,
        account_id: None,
    };
    let openai = AuthCredential::ApiKey {
        key: "openai-key".to_string(),
    };
    // auth.json now holds tokens another process refreshed since startup.
    let mut current = AuthData::default();
    current.insert(
        "anthropic",
        DEFAULT_ACCOUNT_NAME,
        oauth("rotated", 4_000_000_000),
    );
    current.insert("openai", DEFAULT_ACCOUNT_NAME, openai.clone());
    noel_config::write_auth_file(&auth_path, &current).expect("seed auth.json");
    // This session's startup snapshot: the stale tokens plus a value that
    // came from `--api-key` and must stay in memory.
    let mut snapshot = AuthData::default();
    snapshot.insert("anthropic", DEFAULT_ACCOUNT_NAME, oauth("stale", 1));
    snapshot.insert("openai", DEFAULT_ACCOUNT_NAME, openai);
    snapshot.insert(
        "google",
        DEFAULT_ACCOUNT_NAME,
        AuthCredential::ApiKey {
            key: "cli-only-key".to_string(),
        },
    );
    let mut config = account_test_config(snapshot);
    config.paths.auth_path = auth_path.clone();

    assert!(remove_stored_auth(&mut config, "openai", DEFAULT_ACCOUNT_NAME).unwrap());

    let content = fs::read_to_string(&auth_path).expect("read auth.json");
    assert!(!content.contains("cli-only-key"), "{content}");
    assert!(!content.contains("stale-"), "{content}");
    let stored = noel_config::read_auth_file(&auth_path).expect("parse auth.json");
    assert_eq!(
        stored.credential("anthropic", DEFAULT_ACCOUNT_NAME),
        Some(&oauth("rotated", 4_000_000_000))
    );
    assert!(stored.credential("openai", DEFAULT_ACCOUNT_NAME).is_none());
    assert!(config
        .auth
        .credential("openai", DEFAULT_ACCOUNT_NAME)
        .is_none());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn reload_keeps_the_trust_decision_and_cli_overrides() {
    // The developer's own `~/.agents/skills` must not load into the
    // test's config: isolate like the CLI test harness does, and put
    // the isolation back afterwards.
    let saved_isolation = std::env::var_os(noel_config::ENV_TEST_ISOLATION);
    // Test setup: the environment is process-wide, and writing it is unsafe since Rust 2024 while another thread may read it.
    unsafe { std::env::set_var(noel_config::ENV_TEST_ISOLATION, "1") };
    let root =
        std::env::temp_dir().join(format!("noel-cli-config-source-{}", unique_temp_suffix()));
    let agent = root.join("agent");
    let project = root.join("project");
    fs::create_dir_all(&agent).expect("create agent dir");
    fs::create_dir_all(project.join(".noel/extensions")).expect("create project extensions");
    fs::write(
        project.join(".noel/settings.json"),
        r#"{"shellCommandPrefix":"echo untrusted"}"#,
    )
    .expect("write project settings");
    fs::write(
        project.join(".noel/extensions/project.md"),
        "project extension",
    )
    .expect("write project extension");
    fs::write(project.join("AGENTS.md"), "project rules").expect("write context file");
    let resource = |name: &str, content: &str| {
        let path = root.join(format!("{name}.md"));
        fs::write(&path, content).expect("write cli resource");
        path.display().to_string()
    };
    let extension = resource("ext", "cli ext");
    let skill = resource(
        "skill",
        "---\nname: cli-skill\ndescription: From a flag.\n---\n",
    );
    let template = resource("tpl", "cli tpl");
    let cli = Cli::try_parse_from([
        "noel",
        "--system-prompt",
        "cli system",
        "--append-system-prompt",
        "cli append",
        "--no-context-files",
        "--tools",
        "read",
        "--provider",
        "anthropic",
        "--api-key",
        "cli-key",
        "-e",
        &extension,
        "--skill",
        &skill,
        "--prompt-template",
        &template,
    ])
    .expect("parse cli");
    let source = ConfigSource {
        cli,
        cwd: project.clone(),
        project_trusted: false,
    };
    let paths = ConfigPaths {
        cwd: project.clone(),
        agent_dir: agent.clone(),
        session_dir: agent.join("sessions"),
        settings_path: agent.join("settings.json"),
        project_settings_path: project.join(".noel/settings.json"),
        auth_path: agent.join("auth.json"),
        models_path: agent.join("models.json"),
        model_cache_path: agent.join("model-cache.json"),
        keybindings_path: agent.join("keybindings.json"),
    };

    let config = source.load(paths).expect("reload config");

    assert_eq!(config.settings.shell_command_prefix, None);
    let contents = |resources: &[ResourceFile]| {
        resources
            .iter()
            .map(|resource| resource.content.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(contents(&config.extensions), ["cli ext"]);
    assert_eq!(config.skills.len(), 1);
    assert_eq!(config.skills[0].name, "cli-skill");
    assert_eq!(config.prompt_templates.len(), 1);
    assert_eq!(config.prompt_templates[0].name, "tpl");
    assert_eq!(config.prompt_templates[0].content, "cli tpl");
    assert_eq!(config.system_prompt.as_deref(), Some("cli system"));
    assert_eq!(config.append_system_prompt, ["cli append"]);
    assert!(config.context_files.is_empty());
    assert_eq!(
        config.settings.enabled_tools,
        Some(vec!["read".to_string()])
    );
    assert_eq!(
        auth_for_provider(&config.auth, "anthropic", None),
        Some(ResolvedAuth::ApiKey("cli-key".to_string()))
    );
    match saved_isolation {
        // Test setup: the environment is process-wide, and writing it is unsafe since Rust 2024 while another thread may read it.
        Some(value) => unsafe { std::env::set_var(noel_config::ENV_TEST_ISOLATION, value) },
        // Test setup: the environment is process-wide, and writing it is unsafe since Rust 2024 while another thread may read it.
        None => unsafe { std::env::remove_var(noel_config::ENV_TEST_ISOLATION) },
    }
    let _ = fs::remove_dir_all(root);
}

/// Every `discovery` case of `system-prompt.json`: from the same files,
/// trust decision, and flags, startup gives the prompt builder what
/// upstream's resource loader gives its own. Each case runs twice: on
/// the tree as upstream wrote it, which is a project written for pi
/// (`.pi/`), and on the same tree under noel's names (`.noel/`).
#[test]
fn prompt_and_context_files_match_ts_discovery() {
    let fixture = serde_json::from_str::<serde_json::Value>(include_str!(
        "../../../tests/fixtures/ts-parity/system-prompt.json"
    ))
    .expect("parse TS parity fixture");
    let cases = fixture["discovery"].as_array().expect("discovery cases");
    assert_eq!(cases.len(), 15);
    let layouts = cases.iter().flat_map(|case| {
        [
            ("as upstream wrote it", case.clone()),
            (
                "with noel's names",
                noel_config::value_with_noel_names(case),
            ),
        ]
    });
    for (layout, case) in layouts {
        let name = format!("{} ({layout})", case["name"].as_str().expect("case name"));
        let root =
            std::env::temp_dir().join(format!("noel-cli-discovery-{}", unique_temp_suffix()));
        let text = |value: &serde_json::Value| {
            let text = value.as_str().expect("fixture string");
            text.replace("<tmp>", &root.display().to_string())
                .replace("<bom>", "\u{feff}")
        };
        let (agent, cwd) = (root.join("agent"), root.join(text(&case["cwd"])));
        let dirs = case["dirs"].as_array().expect("dirs").iter().map(text);
        for dir in dirs.chain(["agent".to_string(), text(&case["cwd"])]) {
            fs::create_dir_all(root.join(dir)).expect("create dir");
        }
        for (path, content) in case["files"].as_object().expect("files") {
            let path = root.join(path);
            fs::create_dir_all(path.parent().expect("parent")).expect("create dir");
            fs::write(path, text(content)).expect("write file");
        }

        let flags = &case["flags"];
        let mut args = vec!["noel".to_string()];
        if !flags["systemPrompt"].is_null() {
            args.extend(["--system-prompt".to_string(), text(&flags["systemPrompt"])]);
        }
        for value in flags["appendSystemPrompt"].as_array().expect("flags") {
            args.extend(["--append-system-prompt".to_string(), text(value)]);
        }
        if flags["noContextFiles"] == true {
            args.push("--no-context-files".to_string());
        }
        let source = ConfigSource {
            cli: Cli::try_parse_from(args).expect("parse cli"),
            cwd: cwd.clone(),
            project_trusted: case["trusted"].as_bool().expect("trusted"),
        };
        let config = source
            .load(ConfigPaths {
                cwd: cwd.clone(),
                agent_dir: agent.clone(),
                session_dir: agent.join("sessions"),
                settings_path: agent.join("settings.json"),
                project_settings_path: noel_config::project_config_dir(&cwd).join("settings.json"),
                auth_path: agent.join("auth.json"),
                models_path: agent.join("models.json"),
                model_cache_path: agent.join("model-cache.json"),
                keybindings_path: agent.join("keybindings.json"),
            })
            .expect("load config");

        let result = &case["result"];
        assert_eq!(
            config.system_prompt.as_deref(),
            result["customPrompt"].as_str(),
            "{name}: custom prompt"
        );
        assert_eq!(
            config.append_system_prompt.join("\n\n"),
            text(&result["appendSystemPrompt"]),
            "{name}: appended prompt"
        );
        // Directories above the case's tree are not part of the case.
        let context_files = config
            .context_files
            .iter()
            .filter(|file| file.path.starts_with(&root))
            .map(|file| (file.path.display().to_string(), file.content.clone()))
            .collect::<Vec<_>>();
        let expected = result["contextFiles"].as_array().expect("context files");
        let expected = expected
            .iter()
            .map(|file| (text(&file["path"]), text(&file["content"])))
            .collect::<Vec<_>>();
        assert_eq!(context_files, expected, "{name}: context files");
        assert_eq!(config.diagnostics, Vec::<String>::new(), "{name}");
        let _ = fs::remove_dir_all(root);
    }
}

#[test]
fn append_flags_add_to_the_setting() {
    // The fixture above covers upstream: flags replace APPEND_SYSTEM.md.
    // The `appendSystemPrompt` setting is noel's own, and flags add to it.
    let root =
        std::env::temp_dir().join(format!("noel-cli-append-setting-{}", unique_temp_suffix()));
    let agent = root.join("agent");
    fs::create_dir_all(&agent).expect("create agent dir");
    fs::write(agent.join("APPEND_SYSTEM.md"), "from the file").expect("write append file");
    fs::write(
        agent.join("settings.json"),
        r#"{"appendSystemPrompt":["from the setting"]}"#,
    )
    .expect("write settings");
    let source = ConfigSource {
        cli: Cli::try_parse_from(["noel", "--append-system-prompt", "from the flag"])
            .expect("parse cli"),
        cwd: root.clone(),
        project_trusted: false,
    };
    let config = source
        .load(ConfigPaths {
            cwd: root.clone(),
            agent_dir: agent.clone(),
            session_dir: agent.join("sessions"),
            settings_path: agent.join("settings.json"),
            project_settings_path: root.join(".noel/settings.json"),
            auth_path: agent.join("auth.json"),
            models_path: agent.join("models.json"),
            model_cache_path: agent.join("model-cache.json"),
            keybindings_path: agent.join("keybindings.json"),
        })
        .expect("load config");
    assert_eq!(
        config.append_system_prompt,
        ["from the setting", "from the flag"]
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn startup_prefers_models_with_credentials_over_faux() {
    // Made-up providers have no environment or login-file credentials,
    // so only `config.auth` decides which ones are usable.
    let model = |provider: &str, id: &str| ModelDefinition {
        context_window: None,
        provider: provider.to_string(),
        id: id.to_string(),
        name: None,
        api: if provider == "faux" {
            ConfigProviderApi::Faux
        } else {
            ConfigProviderApi::Anthropic
        },
        base_url: None,
        ..ModelDefinition::default()
    };
    let key = |key: &str| AuthCredential::ApiKey {
        key: key.to_string(),
    };
    let mut config = account_test_config(AuthData::default());
    config.models = vec![
        model("faux", "echo"),
        model("keyed", "first"),
        model("unkeyed", "saved"),
    ];
    let pick = |config: &LoadedConfig, args: &[&str], offline: bool| {
        let mut config = config.clone();
        let mut runtime = account_test_runtime("faux", None);
        runtime.set_active_model(None).expect("clear model");
        let cli = Cli::try_parse_from(std::iter::once("noel").chain(args.iter().copied()))
            .expect("parse cli");
        let selection =
            resolve_cli_selection(&cli, &mut config, offline).expect("read the command line");
        select_initial_model(&mut runtime, &config, &selection, offline).expect("select model");
        runtime
            .session()
            .active_model
            .as_ref()
            .map(|model| format!("{}/{}", model.provider, model.id))
    };

    // Nothing has credentials: faux is the last resort.
    assert_eq!(pick(&config, &[], false).as_deref(), Some("faux/echo"));

    // A model with credentials wins over faux; a provider's usual model
    // (upstream defaultModelPerProvider) wins over catalog order.
    config
        .auth
        .insert("keyed", DEFAULT_ACCOUNT_NAME, key("keyed-key"));
    assert_eq!(pick(&config, &[], false).as_deref(), Some("keyed/first"));
    config
        .auth
        .insert("anthropic", DEFAULT_ACCOUNT_NAME, key("anthropic-key"));
    config.models.extend([
        model("anthropic", "claude-opus-4-1-20250805"),
        model("anthropic", "claude-opus-5-5"),
    ]);
    assert_eq!(
        pick(&config, &[], false).as_deref(),
        Some("anthropic/claude-opus-5-5")
    );

    // A saved default counts only when its provider has credentials.
    config.settings.default_provider = Some("unkeyed".to_string());
    config.settings.default_model = Some("saved".to_string());
    assert_eq!(
        pick(&config, &[], false).as_deref(),
        Some("anthropic/claude-opus-5-5")
    );
    config.settings.default_provider = Some("keyed".to_string());
    config.settings.default_model = Some("first".to_string());
    assert_eq!(pick(&config, &[], false).as_deref(), Some("keyed/first"));

    // Offline only faux can run; an explicit --model still wins.
    assert_eq!(pick(&config, &[], true).as_deref(), Some("faux/echo"));
    assert_eq!(
        pick(&config, &["--model", "faux/echo"], false).as_deref(),
        Some("faux/echo")
    );
    assert_eq!(
        pick(&config, &["--model", "unkeyed/saved"], true).as_deref(),
        Some("unkeyed/saved")
    );
}

#[tokio::test]
async fn provider_construction_tolerates_bound_account_without_matching_credential() {
    let mut config = account_test_config(AuthData::default());
    config.models.push(ModelDefinition {
        context_window: None,
        provider: "faux".to_string(),
        id: "echo".to_string(),
        name: None,
        api: ConfigProviderApi::Faux,
        base_url: None,
        ..ModelDefinition::default()
    });
    let runtime = account_test_runtime("faux", Some("work"));

    provider_for_runtime(&runtime, &config, false)
        .await
        .expect("faux provider builds regardless of account binding");
}

#[tokio::test]
async fn every_model_request_sends_the_credential_current_when_it_is_sent() {
    use std::io::BufReader;
    let root = std::env::temp_dir().join(format!("noel-cli-fresh-auth-{}", unique_temp_suffix()));
    fs::create_dir_all(&root).unwrap();
    let auth_path = root.join("auth.json");
    let credential = |access_token: &str, expires| AuthCredential::OAuth {
        access_token: access_token.to_string(),
        refresh_token: None,
        expires,
        account_id: Some("account".to_string()),
    };
    let save = |access_token: &str| {
        let mut auth = AuthData::default();
        auth.insert(
            "openai-codex",
            "default",
            credential(access_token, 4_000_000_000),
        );
        fs::write(&auth_path, serde_json::to_string(&auth).unwrap()).unwrap();
    };
    // The model endpoint records the bearer token of each request.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        (0..2)
                .map(|_| {
                    let (stream, _) = listener.accept().unwrap();
                    let mut reader = BufReader::new(stream);
                    let (mut authorization, mut length) = (String::new(), 0);
                    loop {
                        let mut line = String::new();
                        reader.read_line(&mut line).unwrap();
                        let Some((name, value)) = line.trim_end().split_once(": ") else {
                            if line.trim_end().is_empty() {
                                break;
                            }
                            continue;
                        };
                        match name.to_ascii_lowercase().as_str() {
                            "authorization" => authorization = value.to_string(),
                            "content-length" => length = value.parse().unwrap(),
                            _ => {}
                        }
                    }
                    reader.read_exact(&mut vec![0; length]).unwrap();
                    let body = r#"{"output_text":"ok"}"#;
                    write!(
                        reader.into_inner(),
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .unwrap();
                    authorization
                })
                .collect::<Vec<_>>()
    });
    // The startup snapshot predates a refresh: its token long expired.
    let mut snapshot = AuthData::default();
    snapshot.insert("openai-codex", "default", credential("startup-access", 1));
    let mut config = account_test_config(snapshot);
    config.paths.auth_path = auth_path.clone();
    config.models.push(ModelDefinition {
        context_window: None,
        provider: "openai-codex".to_string(),
        id: "echo".to_string(),
        name: None,
        api: ConfigProviderApi::OpenAi,
        base_url: Some(url),
        ..ModelDefinition::default()
    });
    let runtime = account_test_runtime("openai-codex", None);
    let request = || ProviderRequest {
        max_output_tokens: None,
        system_prompt: None,
        messages: Vec::new(),
        tools: Vec::new(),
    };

    save("first-access");
    let provider = provider_for_runtime(&runtime, &config, false)
        .await
        .unwrap();
    provider.complete(request()).await.unwrap();
    save("second-access");
    provider.complete(request()).await.unwrap();
    assert_eq!(
        server.join().unwrap(),
        ["Bearer first-access", "Bearer second-access"]
    );
    fs::remove_dir_all(root).unwrap();
}

/// With failover on, the `/account` selector says what `auto` does and
/// that choosing an account pins it.
#[test]
fn account_selector_names_failover_and_pinning_when_failover_is_on() {
    let (config, runtime) = failover_fixture(
        "failover-selector",
        "http://127.0.0.1:9",
        &["first", "second"],
        1,
    );
    let labels = |config: &LoadedConfig| {
        account_selector_items(config, &runtime)
            .into_iter()
            .map(|item| item.label)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        labels(&config),
        [
            "auto (failover: sticky account, switches on limits)",
            "first (pins account; failover disabled while pinned)",
            "second (pins account; failover disabled while pinned)",
        ]
    );
    let mut off = config;
    off.settings.failover = None;
    assert_eq!(
        labels(&off),
        ["auto (default resolution)", "first", "second"]
    );
}

/// What the failover endpoint answers a request with.
enum Answer {
    /// A whole reply with this text.
    Says(&'static str),
    /// This HTTP status.
    Status(u16),
    /// A 429 whose `retry-after` header holds this.
    Limited(&'static str),
    /// Anthropic's 529: the provider is overloaded.
    Overloaded,
    /// The start of a reply, then the connection drops.
    BreaksOff,
}

/// A local chat-completions endpoint for the failover tests. It answers
/// a request as `answer` says for the credential the request carries and
/// the number of requests that credential made before. Returns its URL
/// and the credentials of the requests it got, in order.
fn failover_endpoint(
    answer: impl Fn(&str, usize) -> Answer + Send + 'static,
) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    use std::io::BufReader;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let seen = requests.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut reader = BufReader::new(stream.unwrap());
            let (mut credential, mut length) = (String::new(), 0);
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let Some((name, value)) = line.trim_end().split_once(": ") else {
                    if line.trim_end().is_empty() {
                        break;
                    }
                    continue;
                };
                match name.to_ascii_lowercase().as_str() {
                    "authorization" => credential = value.trim_start_matches("Bearer ").to_string(),
                    "content-length" => length = value.parse().unwrap(),
                    _ => {}
                }
            }
            reader.read_exact(&mut vec![0; length]).unwrap();
            let earlier = {
                let mut seen = seen.lock().unwrap();
                seen.push(credential.clone());
                seen.iter().filter(|seen| **seen == credential).count() - 1
            };
            let json = |status: &str, headers: &str, body: String| {
                format!(
                        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\n{headers}content-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    )
            };
            let response = match answer(&credential, earlier) {
                    // A chat completion, and the same text as the ChatGPT
                    // endpoint of an `openai-codex` login words it.
                    Answer::Says(text) => json(
                        "200 OK",
                        "",
                        serde_json::json!({
                            "choices": [{"message": {"content": text}, "finish_reason": "stop"}],
                            "output_text": text,
                        })
                        .to_string(),
                    ),
                    Answer::Status(status) => json(
                        &format!("{status} Synthetic"),
                        "",
                        r#"{"error":"synthetic"}"#.to_string(),
                    ),
                    Answer::Limited(retry_after) => json(
                        "429 Too Many Requests",
                        &format!("retry-after: {retry_after}\r\n"),
                        r#"{"error":"synthetic"}"#.to_string(),
                    ),
                    Answer::Overloaded => json(
                        "529 Overloaded",
                        "",
                        r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#
                            .to_string(),
                    ),
                    // One chunk of a chunked stream and no end of it.
                    Answer::BreaksOff => {
                        let event =
                            "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n";
                        format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n{:x}\r\n{event}\r\n",
                            event.len()
                        )
                    }
                };
            // The client may have gone: a dropped request is not a failure here.
            let _ = reader.into_inner().write_all(response.as_bytes());
        }
    });
    (url, requests)
}

/// A config whose provider `provider` has one model at `url` and the
/// API-key accounts `accounts` (each with the key `<account>-key`), with
/// failover enabled and an agent directory of its own, and a runtime on
/// that model that retries twice after `backoff_ms`. Every failover test
/// has a provider id of its own, which its notices name: notices are
/// the process's.
fn failover_fixture(
    provider: &str,
    url: &str,
    accounts: &[&str],
    backoff_ms: u64,
) -> (LoadedConfig, Runtime) {
    let mut auth = AuthData::default();
    for account in accounts {
        auth.insert(
            provider,
            account,
            AuthCredential::ApiKey {
                key: format!("{account}-key"),
            },
        );
    }
    let mut config = account_test_config(auth);
    fs::create_dir_all(&config.paths.agent_dir).unwrap();
    config.settings.failover = Some(noel_config::FailoverSettings {
        enabled: Some(true),
        ..Default::default()
    });
    config.models.push(ModelDefinition {
        provider: provider.to_string(),
        id: "echo".to_string(),
        api: ConfigProviderApi::OpenAi,
        base_url: Some(url.to_string()),
        ..ModelDefinition::default()
    });
    let mut session = SessionState::new("session-1", PathBuf::from("."));
    session.active_model = Some(ModelRef {
        provider: provider.to_string(),
        id: "echo".to_string(),
    });
    let mut systems = ReloadableSystems::default();
    systems.retry.max_retries = 2;
    systems.retry.base_delay_ms = backoff_ms;
    (config, Runtime::new(session, systems))
}

/// What the pools of `provider` saved in the fixture's agent directory.
fn saved_failover(config: &LoadedConfig, provider: &str) -> account_failover::SavedProvider {
    account_failover::saved_state(
        &config.paths.agent_dir.join(account_failover::STATE_FILE),
        provider,
    )
}

/// One turn on a provider built for it, as every mode runs one: how it
/// ended, and how many retries the run loop started.
async fn failover_turn(
    runtime: &mut Runtime,
    config: &LoadedConfig,
) -> (std::result::Result<String, String>, usize) {
    let provider = provider_for_runtime(runtime, config, false).await.unwrap();
    let mut retries = 0;
    let run = RunControl::default();
    let turn = run_user_turn_streaming_events_with_media(
        runtime,
        provider.as_ref(),
        "hello".into(),
        Vec::new(),
        &run,
        |event| retries += usize::from(matches!(event, TurnEvent::RetryStarted(_))),
    );
    let result = tokio::time::timeout(Duration::from_secs(30), turn)
        .await
        .expect("the turn waits for no long backoff");
    (result.map_err(|error| error.to_string()), retries)
}

/// Failover acts before the run loop's retry: a 429 with no output
/// moves the request to the next account inside the provider, so the
/// run loop sees a reply, starts no retry and waits for no backoff. The
/// choice outlives the pool, which every request builds anew.
#[tokio::test]
async fn a_rate_limited_account_fails_over_inside_the_request_without_a_retry() {
    let provider = "failover-switch";
    let (url, requests) = failover_endpoint(|credential, _| match credential {
        "first-key" => Answer::Status(429),
        _ => Answer::Says("from the second account"),
    });
    // A backoff would outlast the turn's time limit.
    let (config, mut runtime) = failover_fixture(provider, &url, &["first", "second"], 60_000);

    let (reply, retries) = failover_turn(&mut runtime, &config).await;
    assert_eq!(reply.as_deref(), Ok("from the second account"));
    assert_eq!(retries, 0);
    assert_eq!(*requests.lock().unwrap(), ["first-key", "second-key"]);
    // What the TUI shows after the turn: both accounts, and until when.
    let notices = account_failover::notices_given(&format!("{provider} account "));
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(
        notices[0].starts_with("failover-switch account first is rate limited until ")
            && notices[0].ends_with(" UTC; switched to second"),
        "{notices:?}"
    );

    // The next turn has a new provider and its request a new pool: the
    // second account still carries the session and the first is still
    // limited, so it is not asked again.
    let (reply, retries) = failover_turn(&mut runtime, &config).await;
    assert_eq!(reply.as_deref(), Ok("from the second account"));
    assert_eq!(retries, 0);
    assert_eq!(
        *requests.lock().unwrap(),
        ["first-key", "second-key", "second-key"]
    );
    let saved = saved_failover(&config, provider);
    assert_eq!(saved.current.as_deref(), Some("second"));
    assert_eq!(saved.limited_until.keys().collect::<Vec<_>>(), ["first"]);
    assert_eq!(
        account_failover::notices_given(&format!("{provider} account ")).len(),
        1
    );
}

/// A rate limit lasts until the time the provider's `retry-after` gives,
/// and five minutes when the answer gives none.
#[tokio::test]
async fn a_rate_limit_lasts_until_the_reset_the_provider_gives() {
    for (provider, answer) in [
        ("failover-retry-after", Some("7200")),
        ("failover-no-retry-after", None),
    ] {
        let (url, _) = failover_endpoint(move |credential, _| match (credential, answer) {
            ("first-key", Some(seconds)) => Answer::Limited(seconds),
            ("first-key", None) => Answer::Status(429),
            _ => Answer::Says("ok"),
        });
        let (config, mut runtime) = failover_fixture(provider, &url, &["first", "second"], 1);
        let before = unix_seconds().unwrap() as i64;
        let (reply, _) = failover_turn(&mut runtime, &config).await;
        assert_eq!(reply.as_deref(), Ok("ok"));
        let wait = if answer.is_some() {
            7_200
        } else {
            account_failover::DEFAULT_COOLDOWN_SECONDS
        };
        let until = saved_failover(&config, provider).limited_until["first"];
        assert!(
            (before + wait..=before + wait + 2).contains(&until),
            "{provider}: {until} is not {wait} s after {before}"
        );
    }
}

/// With no account left the turn ends at once. The pool's error is noel's
/// own, not a provider's answer, so the run loop does not retry it: each
/// account is asked once, and none again while they are limited.
#[tokio::test]
async fn an_exhausted_pool_ends_the_turn_and_is_not_retried() {
    let (url, requests) = failover_endpoint(|_, _| Answer::Status(429));
    let (config, mut runtime) =
        failover_fixture("failover-exhausted", &url, &["first", "second"], 60_000);

    let (reply, retries) = failover_turn(&mut runtime, &config).await;
    let error = reply.expect_err("no account is left");
    assert!(
        error.contains(
            "every failover-exhausted account is rate limited; the first may be asked again at "
        ) && error.contains("first: request failed with status 429 Too Many Requests")
            && error.contains("second: request failed with status 429 Too Many Requests"),
        "{error}"
    );
    assert_eq!(retries, 0);
    assert_eq!(*requests.lock().unwrap(), ["first-key", "second-key"]);

    // Both accounts are limited: the next turn sends nothing.
    let (reply, retries) = failover_turn(&mut runtime, &config).await;
    let error = reply.expect_err("both accounts are limited");
    assert!(
        error.contains("every failover-exhausted account is rate limited")
            && error.ends_with(" UTC"),
        "{error}"
    );
    assert_eq!(retries, 0);
    assert_eq!(*requests.lock().unwrap(), ["first-key", "second-key"]);
}

/// Failures that are not a rate limit before any output stay with the
/// account in use and are the run loop's to retry: an overloaded
/// provider (529), and a reply that breaks off. Neither moves to the
/// other account or sets one aside.
#[tokio::test]
async fn other_failures_are_retried_by_the_run_loop_on_the_same_account() {
    let provider = "failover-retry";
    let (url, requests) = failover_endpoint(|_, earlier| match earlier {
        0 => Answer::Overloaded,
        1 => Answer::BreaksOff,
        _ => Answer::Says("the whole reply"),
    });
    let (config, mut runtime) = failover_fixture(provider, &url, &["first", "second"], 1);

    let (reply, retries) = failover_turn(&mut runtime, &config).await;
    assert_eq!(reply.as_deref(), Ok("the whole reply"));
    assert_eq!(retries, 2);
    assert_eq!(
        *requests.lock().unwrap(),
        ["first-key", "first-key", "first-key"]
    );
    let saved = saved_failover(&config, provider);
    assert_eq!(saved.current.as_deref(), Some("first"));
    assert!(saved.limited_until.is_empty(), "{saved:?}");
    assert!(account_failover::notices_given(&format!("{provider} account ")).is_empty());
}

/// An account whose 5h usage window the usage cache shows at the
/// threshold is passed over before the provider limits it, and the
/// provider's own `exhaustionThresholdPct` moves the threshold.
#[tokio::test]
async fn an_account_at_its_usage_threshold_is_passed_over() {
    for (provider, threshold, first_serves) in [
        ("failover-usage", None, false),
        ("failover-usage-high", Some(99), true),
    ] {
        let (url, requests) = failover_endpoint(|credential, _| match credential {
            "first-key" => Answer::Says("first"),
            _ => Answer::Says("second"),
        });
        let (mut config, mut runtime) = failover_fixture(provider, &url, &["first", "second"], 1);
        if let Some(threshold) = threshold {
            config.settings.failover.as_mut().unwrap().providers.insert(
                provider.to_string(),
                noel_config::FailoverProviderSettings {
                    exhaustion_threshold_pct: Some(threshold),
                    ..Default::default()
                },
            );
        }
        let now = unix_seconds().unwrap() as i64;
        let mut cache = noel_config::UsageCache::default();
        for (account, used_pct) in [("first", 95.0), ("second", 10.0)] {
            cache.accounts.insert(
                format!("{provider}:{account}"),
                noel_config::AccountUsage {
                    plan: None,
                    windows: vec![noel_config::UsageWindow {
                        label: "5h".to_string(),
                        used_pct,
                        resets_at: Some(now + 3_600),
                        active: None,
                        severity: None,
                    }],
                    balance: None,
                    fetched_at: now,
                },
            );
        }
        noel_config::write_usage_cache(&config.paths.agent_dir.join("usage-cache.json"), &cache)
            .unwrap();

        let (reply, _) = failover_turn(&mut runtime, &config).await;
        let expected = if first_serves { "first" } else { "second" };
        assert_eq!(reply.as_deref(), Ok(expected), "{provider}");
        assert_eq!(*requests.lock().unwrap(), [format!("{expected}-key")]);
    }
}

/// An account whose credential cannot be refreshed is left out, with one
/// notice however many turns it is left out of, and the others serve.
/// Only a provider with a login of its own expires one: `anthropic`.
#[tokio::test]
async fn an_account_that_cannot_refresh_is_left_out_of_the_pool() {
    let provider = "anthropic";
    let (url, requests) = failover_endpoint(|_, _| Answer::Says("from the second account"));
    let (mut config, mut runtime) = failover_fixture(provider, &url, &["second"], 1);
    // Expired, and nothing to refresh it with.
    config.auth.insert(
        provider,
        "first",
        AuthCredential::OAuth {
            access_token: "first-access".to_string(),
            refresh_token: None,
            expires: 1,
            account_id: None,
        },
    );
    for _ in 0..2 {
        let (reply, _) = failover_turn(&mut runtime, &config).await;
        assert_eq!(reply.as_deref(), Ok("from the second account"));
    }
    assert_eq!(*requests.lock().unwrap(), ["second-key", "second-key"]);
    let notices = account_failover::notices_given(&format!("{provider} account "));
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(
        notices[0].starts_with("anthropic account first is left out of failover: "),
        "{notices:?}"
    );
}

/// Without failover a rate limit is the run loop's, as on a build
/// without this feature: the session's account is asked again after
/// each backoff and no other account is tried. That holds when failover
/// is off (the default) and when `/account` pins an account.
#[tokio::test]
async fn a_pinned_account_or_disabled_failover_does_not_rotate() {
    for pinned in [true, false] {
        let provider = if pinned {
            "failover-pinned"
        } else {
            "failover-off"
        };
        let (url, requests) = failover_endpoint(|_, _| Answer::Status(429));
        let (mut config, mut runtime) = failover_fixture(provider, &url, &["default", "second"], 1);
        if pinned {
            runtime
                .set_active_account(Some("second".to_string()))
                .unwrap();
        } else {
            config.settings.failover = None;
        }
        let account = if pinned { "second-key" } else { "default-key" };

        let (reply, retries) = failover_turn(&mut runtime, &config).await;
        let error = reply.expect_err("the account stays rate limited");
        assert!(
            error.starts_with("request failed with status 429 Too Many Requests"),
            "{error}"
        );
        assert_eq!(retries, 2);
        assert_eq!(*requests.lock().unwrap(), [account, account, account]);
        assert_eq!(
            saved_failover(&config, provider),
            account_failover::SavedProvider::default()
        );
    }
}

/// Named accounts without a `default` make a provider's models
/// available when failover rotates between them; without failover the
/// session has to pin one.
#[test]
fn named_accounts_make_a_provider_usable_when_failover_rotates() {
    let (mut config, _) = failover_fixture(
        "failover-usable",
        "http://127.0.0.1:9",
        &["first", "second"],
        1,
    );
    assert!(provider_usable(&config, "failover-usable", false));
    assert!(!provider_usable(&config, "failover-usable", true));
    config.settings.failover = None;
    assert!(!provider_usable(&config, "failover-usable", false));
}

/// A key given with `--api-key` serves the run: failover is off, so the
/// stored accounts cannot take its place.
#[test]
fn an_api_key_on_the_command_line_turns_failover_off() {
    let (mut config, _) = failover_fixture(
        "failover-cli-key",
        "http://127.0.0.1:9",
        &["first", "second"],
        1,
    );
    let cli = Cli::try_parse_from([
        "noel",
        "--model",
        "failover-cli-key/echo",
        "--api-key",
        "cli-key",
    ])
    .unwrap();
    apply_cli_overrides(&cli, Path::new("."), &mut config).unwrap();
    assert_eq!(config.settings.failover, None);
    assert!(!failover_enabled_for(&config, "failover-cli-key"));
    assert_eq!(
        auth_for_provider(&config.auth, "failover-cli-key", None),
        Some(ResolvedAuth::ApiKey("cli-key".to_string()))
    );
}

/// What the run loop asks the provider about the model does not depend
/// on failover: the thinking level it records, and the output limit it
/// measures a cut-off reply against.
#[tokio::test]
async fn failover_keeps_the_thinking_level_and_the_output_limit() {
    let (mut config, runtime) = failover_fixture(
        "failover-traits",
        "http://127.0.0.1:9",
        &["first", "second"],
        1,
    );
    config.settings.default_thinking_level = Some("high".to_string());
    config.models[0].reasoning = Some(true);
    config.models[0].max_tokens = Some(4_096);
    let with_failover = provider_for_runtime(&runtime, &config, false)
        .await
        .unwrap();
    config.settings.failover = None;
    let without = provider_for_runtime(&runtime, &config, false)
        .await
        .unwrap();
    assert_eq!(with_failover.thinking_level().as_deref(), Some("high"));
    assert_eq!(with_failover.output_limit(), Some(4_096));
    assert_eq!(with_failover.thinking_level(), without.thinking_level());
    assert_eq!(with_failover.output_limit(), without.output_limit());
}

/// The pool holds only the accounts the provider can use, each with the
/// credential that is current when the request is sent. `openai-codex`
/// takes ChatGPT logins: its stored API key is no candidate. A login
/// that expired in the startup snapshot is read again from auth.json,
/// under its lock, as a single account's is.
#[tokio::test]
async fn the_pool_has_the_providers_own_accounts_with_current_credentials() {
    let root =
        std::env::temp_dir().join(format!("noel-cli-failover-auth-{}", unique_temp_suffix()));
    fs::create_dir_all(&root).unwrap();
    let auth_path = root.join("auth.json");
    let stored = |generation: &str, expires| {
        let mut auth = AuthData::default();
        for account in ["alpha", "beta"] {
            auth.insert(
                "openai-codex",
                account,
                AuthCredential::OAuth {
                    access_token: format!("{account}-{generation}"),
                    refresh_token: None,
                    expires,
                    account_id: Some(account.to_string()),
                },
            );
        }
        // Sorts between the two logins, where a candidate would be
        // tried after the first.
        auth.insert(
            "openai-codex",
            "apikey",
            AuthCredential::ApiKey {
                key: "stray-key".to_string(),
            },
        );
        auth
    };
    let save = |generation: &str| {
        let auth = stored(generation, 4_000_000_000);
        fs::write(&auth_path, serde_json::to_string(&auth).unwrap()).unwrap();
    };
    let (url, requests) = failover_endpoint(|credential, _| {
        if credential.starts_with("alpha") {
            Answer::Status(429)
        } else {
            Answer::Says("ok")
        }
    });
    // The startup snapshot predates a refresh: its tokens long expired.
    let mut config = account_test_config(stored("startup", 1));
    config.paths.auth_path = auth_path.clone();
    config.settings.failover = Some(noel_config::FailoverSettings {
        enabled: Some(true),
        ..Default::default()
    });
    config.models.push(ModelDefinition {
        provider: "openai-codex".to_string(),
        id: "echo".to_string(),
        api: ConfigProviderApi::OpenAi,
        base_url: Some(url),
        ..ModelDefinition::default()
    });
    assert!(failover_enabled_for(&config, "openai-codex"));
    // `openai` takes API keys: of these accounts only one is its own.
    assert!(!failover_enabled_for(&config, "openai"));
    let runtime = account_test_runtime("openai-codex", None);
    let request = || ProviderRequest {
        max_output_tokens: None,
        system_prompt: None,
        messages: Vec::new(),
        tools: Vec::new(),
    };

    save("first");
    let provider = provider_for_runtime(&runtime, &config, false)
        .await
        .unwrap();
    provider.complete(request()).await.unwrap();
    save("second");
    provider.complete(request()).await.unwrap();
    assert_eq!(
        *requests.lock().unwrap(),
        ["alpha-first", "beta-first", "beta-second"]
    );
    fs::remove_dir_all(root).unwrap();
}

/// A final reply cut off below the model's own output limit ran out of
/// context: it is dropped, the session is compacted, and the request is
/// sent again. One cut off at that limit stands, because a smaller
/// context would not make it longer. The limit is the catalog's
/// `maxTokens`, as for upstream's `isRecoverableLength`.
#[tokio::test]
async fn a_reply_cut_off_below_the_catalogs_output_limit_is_an_overflow() {
    use serde_json::json;
    use std::io::BufReader;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    // A built-in model served over chat completions, here by a local
    // endpoint that answers each request with the next reply.
    let model = noel_ai::catalog::provider_base_model("deepseek").expect("a built-in model");
    assert_eq!(model.api, AiProviderApi::OpenAi);
    let limit = model.max_tokens;
    let says =
        |text: &str| json!({"choices": [{"message": {"content": text}, "finish_reason": "stop"}]});
    let cut_off = |output: u64| {
        json!({
            "choices": [{"message": {"content": "This answer is cut"}, "finish_reason": "length"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": output},
        })
    };
    let serve = |replies: Vec<serde_json::Value>| {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let served = Arc::new(AtomicUsize::new(0));
        let count = served.clone();
        std::thread::spawn(move || {
            for (stream, reply) in listener.incoming().zip(replies) {
                let mut reader = BufReader::new(stream.unwrap());
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line.trim_end().is_empty() {
                        break;
                    }
                    let line = line.to_ascii_lowercase();
                    if let Some(value) = line.strip_prefix("content-length: ") {
                        length = value.trim().parse().unwrap();
                    }
                }
                reader.read_exact(&mut vec![0; length]).unwrap();
                count.fetch_add(1, Ordering::SeqCst);
                let body = reply.to_string();
                write!(
                        reader.into_inner(),
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .unwrap();
            }
        });
        (url, served)
    };

    // What the turn answers and how many requests it took, when its
    // first reply stops at `length` after `output` tokens.
    let turn = |output: u64| async move {
        let (url, served) = serve(vec![
            cut_off(output),
            says("Summary of the first turn."),
            says("The whole answer."),
        ]);
        let mut auth = AuthData::default();
        auth.insert(
            &model.provider,
            DEFAULT_ACCOUNT_NAME,
            AuthCredential::ApiKey {
                key: "test-key".to_string(),
            },
        );
        let mut config = account_test_config(auth);
        config.models.push(ModelDefinition {
            base_url: Some(url),
            ..ModelDefinition::from(model)
        });
        let mut session = SessionState::new("session-1", PathBuf::from("."));
        session.active_model = Some(ModelRef {
            provider: model.provider.clone(),
            id: model.id.clone(),
        });
        // An older turn, long enough that its summary is shorter.
        let mut systems = ReloadableSystems::default();
        systems.compaction.keep_recent_tokens = 1;
        let mut runtime = Runtime::new(session, systems);
        let first_answer = "The migration adds a nullable column, backfills it in batches of \
                a thousand rows, then makes it required; the rollback drops the column again, \
                and both directions were run against a copy of the production schema.";
        for (role, content) in [
            (MessageRole::User, "first"),
            (MessageRole::Assistant, first_answer),
        ] {
            runtime
                .push_message(ConversationMessage {
                    role,
                    content: content.into(),
                    thinking: String::new(),
                    media: Vec::new(),
                    tool_call_id: None,
                    tool_name: None,
                    tool_calls: Vec::new(),
                    aborted: false,
                })
                .unwrap();
        }
        let provider = provider_for_runtime(&runtime, &config, false)
            .await
            .unwrap();
        assert_eq!(provider.output_limit(), Some(limit));
        let answer = run_user_turn(&mut runtime, provider.as_ref(), "second".into())
            .await
            .unwrap();
        (answer, served.load(Ordering::SeqCst))
    };

    // Below the limit: the reply, the summary, and the request again.
    assert_eq!(turn(limit - 1).await, ("The whole answer.".to_string(), 3));
    // At the limit the model wrote all it can.
    assert_eq!(turn(limit).await, ("This answer is cut".to_string(), 1));
}

#[test]
fn model_selection_format_is_compact_and_unlabeled() {
    let model = ModelRef {
        provider: "openai-codex".to_string(),
        id: "gpt-5.5".to_string(),
    };

    assert_eq!(
        format_model_selection(&model, Some("xhigh")),
        "openai-codex/gpt-5.5 xhigh"
    );
    assert_eq!(format_model_selection(&model, None), "openai-codex/gpt-5.5");
}

/// A reasoning model with this thinking level map (upstream
/// `thinkingLevelMap`); without entries it supports `off` to `high`.
fn thinking_test_model(provider: &str, id: &str, map: serde_json::Value) -> ModelDefinition {
    ModelDefinition {
        provider: provider.to_string(),
        id: id.to_string(),
        api: ConfigProviderApi::Anthropic,
        reasoning: Some(true),
        thinking_level_map: Some(serde_json::from_value(map).expect("level map")),
        ..ModelDefinition::default()
    }
}

/// A config that knows `models`, with a stored key for each of their
/// providers. The providers are made up, so the models can run and no
/// other can, whatever keys the test run's environment holds.
fn thinking_test_config(models: Vec<ModelDefinition>) -> LoadedConfig {
    let mut config = minimal_test_config();
    for model in &models {
        config.auth.insert(
            &model.provider,
            DEFAULT_ACCOUNT_NAME,
            AuthCredential::ApiKey {
                key: "test-key".to_string(),
            },
        );
    }
    config.models = models;
    config
}

fn test_model_ref(provider: &str, id: &str) -> ModelRef {
    ModelRef {
        provider: provider.to_string(),
        id: id.to_string(),
    }
}

fn top_levels() -> serde_json::Value {
    serde_json::json!({ "xhigh": "xhigh", "max": "max" })
}

#[test]
fn a_model_scope_narrows_the_pickers_and_keeps_every_model_known() {
    let mut config = thinking_test_config(vec![
        thinking_test_model("alpha", "flash", serde_json::json!({})),
        thinking_test_model("beta", "haiku", serde_json::json!({})),
        thinking_test_model("beta", "opus", top_levels()),
    ]);
    let runtime = Runtime::new(
        SessionState::new("session-1", PathBuf::from(".")),
        ReloadableSystems::default(),
    );
    let rows = |config: &LoadedConfig, kind: &str| {
        selector_for_kind(config, &runtime, kind)
            .unwrap()
            .items
            .into_iter()
            .map(|item| item.value)
            .collect::<Vec<_>>()
    };
    assert_eq!(rows(&config, "model").len(), 3);
    let cli = Cli::try_parse_from(["noel", "--models", "beta/op*,flash:low"]).expect("parse cli");
    apply_cli_overrides(&cli, Path::new("."), &mut config).expect("apply the scope");
    assert_eq!(rows(&config, "model"), ["alpha/flash", "beta/opus"]);
    assert_eq!(rows(&config, "provider"), ["alpha", "beta"]);
    assert_eq!(config.models.len(), 3);
    assert_eq!(config.model_scope[1].thinking_level.as_deref(), Some("low"));
    // A model outside the scope can still be named in full.
    assert_eq!(
        resolve_cli(
            &Cli::try_parse_from(["noel", "--model", "beta/haiku"]).unwrap(),
            &config,
            false
        )
        .and_then(|resolved| resolved.model().cloned())
        .map(|model| model.id),
        Some("haiku".to_string())
    );
}

#[tokio::test]
async fn providers_are_built_with_the_clamped_thinking_level() {
    let config = thinking_test_config(vec![
        ModelDefinition {
            reasoning: Some(false),
            ..thinking_test_model("alpha", "plain", serde_json::json!({}))
        },
        thinking_test_model("beta", "haiku", serde_json::json!({})),
    ]);
    let mut runtime = Runtime::new(
        SessionState::new("session-1", PathBuf::from(".")),
        ReloadableSystems::default(),
    );
    runtime
        .set_active_model(Some(test_model_ref("beta", "haiku")))
        .unwrap();
    // A saved level the model lacks reaches the provider clamped.
    runtime
        .set_active_thinking_level(Some("max".to_string()))
        .unwrap();
    let provider = provider_for_runtime(&runtime, &config, false)
        .await
        .expect("provider");
    assert_eq!(provider.thinking_level().as_deref(), Some("high"));
    // Off is a level too, where the model can turn thinking off.
    runtime
        .set_active_thinking_level(Some("off".to_string()))
        .unwrap();
    let provider = provider_for_runtime(&runtime, &config, false)
        .await
        .expect("provider");
    assert_eq!(provider.thinking_level().as_deref(), Some("off"));
    // A model without reasoning is sent no level.
    runtime
        .set_active_model(Some(test_model_ref("alpha", "plain")))
        .unwrap();
    runtime
        .set_active_thinking_level(Some("high".to_string()))
        .unwrap();
    let provider = provider_for_runtime(&runtime, &config, false)
        .await
        .expect("provider");
    assert_eq!(provider.thinking_level(), None);
}

/// From the session and the model definition to the request on the
/// wire: the level clamped to the model and named as the model names
/// it, and the model's output cap.
#[tokio::test]
async fn a_request_carries_the_models_thinking_level_and_output_cap() {
    use std::io::BufReader;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        (0..2)
                .map(|_| {
                    let (stream, _) = listener.accept().unwrap();
                    let mut reader = BufReader::new(stream);
                    let mut length = 0;
                    loop {
                        let mut line = String::new();
                        reader.read_line(&mut line).unwrap();
                        if line.trim_end().is_empty() {
                            break;
                        }
                        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length: ") {
                            length = value.trim().parse().unwrap();
                        }
                    }
                    let mut request = vec![0; length];
                    reader.read_exact(&mut request).unwrap();
                    let body = r#"{"output_text":"ok"}"#;
                    write!(
                        reader.into_inner(),
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .unwrap();
                    serde_json::from_slice::<serde_json::Value>(&request).unwrap()
                })
                .collect::<Vec<_>>()
    });
    let mut auth = AuthData::default();
    auth.insert(
        "local",
        DEFAULT_ACCOUNT_NAME,
        AuthCredential::ApiKey {
            key: "local-key".to_string(),
        },
    );
    let mut config = account_test_config(auth);
    config.paths.auth_path = std::env::temp_dir().join(format!(
        "noel-cli-request-traits-{}/auth.json",
        unique_temp_suffix()
    ));
    let definition = |id: &str, extra: serde_json::Value| {
        let mut model = serde_json::json!({
            "provider": "local", "id": id, "api": "openai-responses", "baseUrl": url,
        });
        model
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        serde_json::from_value::<ModelDefinition>(model).unwrap()
    };
    config.models = vec![
        definition(
            "thinker",
            serde_json::json!({
                "reasoning": true,
                "thinkingLevelMap": { "minimal": null, "xhigh": "extra-hard" },
                "contextWindow": 32000,
                "maxTokens": 4000,
            }),
        ),
        definition("plain", serde_json::json!({})),
    ];
    let mut runtime = account_test_runtime("local", None);
    let request = || ProviderRequest {
        max_output_tokens: None,
        system_prompt: None,
        messages: Vec::new(),
        tools: Vec::new(),
    };
    // `max` is above what the model supports.
    runtime
        .set_active_thinking_level(Some("max".to_string()))
        .unwrap();
    for id in ["thinker", "plain"] {
        runtime
            .set_active_model(Some(test_model_ref("local", id)))
            .unwrap();
        let provider = provider_for_runtime(&runtime, &config, false)
            .await
            .unwrap();
        provider.complete(request()).await.unwrap();
    }
    let requests = server.join().unwrap();
    assert_eq!(requests[0]["model"], "thinker");
    assert_eq!(requests[0]["reasoning"]["effort"], "extra-hard");
    assert_eq!(requests[0]["max_output_tokens"], 4000);
    // Nothing is said of thinking or a cap to a model that has neither.
    assert_eq!(requests[1]["model"], "plain");
    assert_eq!(requests[1].get("reasoning"), None);
    assert_eq!(requests[1].get("max_output_tokens"), None);
}

#[test]
fn reload_keeps_the_custom_model_the_command_line_named() {
    let root = std::env::temp_dir().join(format!("noel-cli-custom-{}", unique_temp_suffix()));
    let agent = root.join("agent");
    fs::create_dir_all(&agent).unwrap();
    let cli = Cli::try_parse_from(["noel", "--model", "anthropic/claude-next:high"]).unwrap();
    let source = ConfigSource {
        cli,
        cwd: root.clone(),
        project_trusted: false,
    };
    let config = source
        .load(ConfigPaths {
            cwd: root.clone(),
            agent_dir: agent.clone(),
            session_dir: agent.join("sessions"),
            settings_path: agent.join("settings.json"),
            project_settings_path: root.join(".noel/settings.json"),
            auth_path: agent.join("auth.json"),
            models_path: agent.join("models.json"),
            model_cache_path: agent.join("model-cache.json"),
            keybindings_path: agent.join("keybindings.json"),
        })
        .expect("reload config");
    let custom = configured_model(&config, &test_model_ref("anthropic", "claude-next"))
        .expect("the custom model");
    let base = noel_ai::catalog::provider_base_model("anthropic").unwrap();
    assert_eq!(custom.max_tokens, Some(base.max_tokens));
    assert_eq!(custom.name.as_deref(), Some("claude-next"));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn an_api_key_goes_to_the_provider_the_model_resolves_to() {
    let mut config = minimal_test_config();
    config.models = noel_ai::catalog::builtin_models()
        .iter()
        .map(ModelDefinition::from)
        .collect();
    let provider = |args: &[&str]| {
        let cli = Cli::try_parse_from(["noel"].into_iter().chain(args.iter().copied())).unwrap();
        infer_cli_provider(&cli, &config)
    };
    assert_eq!(
        provider(&["--model", "anthropic/claude-opus-4-8"]).as_deref(),
        Some("anthropic")
    );
    // A custom id still names its provider, and so does the former
    // Azure id.
    assert_eq!(
        provider(&["--model", "mistral/next"]).as_deref(),
        Some("mistral")
    );
    assert_eq!(
        provider(&["--provider", "azure-openai-responses", "--model", "gpt-5.4"]).as_deref(),
        Some("azure")
    );
    assert_eq!(provider(&[]), None);
}

#[test]
fn live_entries_are_not_finalized_until_complete() {
    let mut app = TuiApp::default();
    app.push(TuiEntryKind::User, "hello");
    let live = app.push_placeholder(TuiEntryKind::Assistant, "Working...");
    assert_eq!(live, 1);
    assert_eq!(app.finalized_entry_count(), 1);

    app.replace_entry(live, "done");
    app.finish_live_entry();
    assert_eq!(app.finalized_entry_count(), 2);
}

#[test]
fn thinking_visibility_can_be_toggled_without_losing_entries() {
    let entries = vec![
        TuiEntry {
            kind: TuiEntryKind::Thinking,
            text: "summary".into(),
        },
        TuiEntry {
            kind: TuiEntryKind::Assistant,
            text: "answer".into(),
        },
    ];
    let shown = visible_transcript_with_thinking(&entries, 80, 24, false, &ThemePalette::default());
    let hidden = visible_transcript_with_thinking(&entries, 80, 24, true, &ThemePalette::default());
    assert_eq!(shown.entries_used, 2);
    assert_eq!(hidden.entries_used, 1);
    assert!(!hidden
        .lines
        .iter()
        .flat_map(|line| &line.spans)
        .any(|span| span.content.contains("summary")));
    assert_eq!(
        visible_transcript_with_thinking(&entries, 80, 24, false, &ThemePalette::default())
            .entries_used,
        2
    );
}

#[test]
fn thinking_is_dimmed_and_separate_from_answer() {
    let lines = render_entry_lines(
        &[
            TuiEntry {
                kind: TuiEntryKind::Thinking,
                text: "summary".into(),
            },
            TuiEntry {
                kind: TuiEntryKind::Assistant,
                text: "answer".into(),
            },
        ],
        &ThemePalette::default(),
    );
    let summary = lines
        .iter()
        .flat_map(|line| &line.spans)
        .find(|span| span.content.contains("summary"))
        .unwrap();
    assert_eq!(summary.style.fg, Some(ThemePalette::default().muted));
    assert!(summary.style.add_modifier.contains(Modifier::ITALIC));
    assert!(render_entry_lines(
        &[TuiEntry {
            kind: TuiEntryKind::Thinking,
            text: String::new()
        }],
        &ThemePalette::default()
    )
    .is_empty());
}

#[test]
fn transcript_height_accounts_for_wrapped_streaming_text() {
    let lines = vec![Line::from("123456"), Line::from("ab")];
    assert_eq!(rendered_lines_height(&lines, 3), 3);
}

#[test]
fn transcript_lines_drop_trailing_entry_spacer_for_viewport() {
    let mut lines = render_entry_lines(
        &[TuiEntry {
            kind: TuiEntryKind::Assistant,
            text: "done".to_string(),
        }],
        &ThemePalette::default(),
    );
    while lines.last().map(Line::width) == Some(0) {
        lines.pop();
    }

    assert_eq!(lines.last().map(Line::width), Some(6));
}

fn scrollback_test_terminal(width: u16, height: u16) -> Terminal<ratatui::backend::TestBackend> {
    Terminal::with_options(
        ratatui::backend::TestBackend::new(width, height),
        TerminalOptions {
            viewport: Viewport::Inline(height.saturating_sub(2).max(1)),
        },
    )
    .unwrap()
}

fn painted_transcript(terminal: &Terminal<ratatui::backend::TestBackend>) -> Vec<String> {
    buffer_rows(terminal.backend().scrollback())
        .into_iter()
        .chain(buffer_rows(terminal.backend().buffer()))
        .map(|line| line.trim_end().to_string())
        .collect()
}

#[test]
fn streamed_screenfuls_preserve_every_row_once_and_leave_the_draft_intact() {
    let config = minimal_test_config();
    let mut app = TuiApp {
        turn_running: true,
        ..Default::default()
    };
    let entry = app.push_placeholder(TuiEntryKind::Assistant, "");
    app.set_input("my draft");
    let mut terminal = scrollback_test_terminal(40, 18);
    // Include a single event larger than the viewport and several follow-on paints.
    for range in [0..70, 70..85, 85..100] {
        let delta = range
            .map(|index| format!("stream-{index:03}\n"))
            .collect::<String>();
        app.append_entry(entry, &delta);
        redraw_tui(&mut terminal, &mut app, &config).unwrap();
    }
    app.finish_live_entry();
    redraw_tui(&mut terminal, &mut app, &config).unwrap();
    let rows = painted_transcript(&terminal);
    for index in 0..100 {
        let marker = format!("stream-{index:03}");
        assert_eq!(
            rows.iter().filter(|line| line.contains(&marker)).count(),
            1,
            "{marker}"
        );
    }
    assert_eq!(app.input, "my draft");
    assert_eq!(app.input_cursor, 8);
    assert!(rows.iter().any(|line| line.contains("│ > my draft")));
}

#[test]
fn scrollback_watermarks_survive_rewrap_and_appending_to_a_partial_line() {
    let config = minimal_test_config();
    let mut app = TuiApp {
        turn_running: true,
        ..Default::default()
    };
    let entry = app.push_placeholder(
        TuiEntryKind::Assistant,
        "abcdefghijklmnopqrstuvwxyz0123456789".repeat(8),
    );
    let mut terminal = scrollback_test_terminal(20, 10);
    redraw_tui(&mut terminal, &mut app, &config).unwrap();
    let before = app.transcript_scrollback.entries[entry].position;
    assert!(before.byte > 0);
    let previous = buffer_rows(terminal.backend().scrollback())
        .into_iter()
        .map(|row| row.trim_end().to_string())
        .collect::<Vec<_>>()
        .join("");
    assert!(!previous.is_empty());
    for (width, height) in [(33, 16), (17, 8), (50, 25)] {
        terminal.backend_mut().resize(width, height);
        terminal.resize(Rect::new(0, 0, width, height)).unwrap();
        app.append_entry(entry, "-next-界e\u{301}-");
        redraw_tui(&mut terminal, &mut app, &config).unwrap();
        // Native history is terminal-owned and may reflow differently. Check
        // logical painted text using the watermark rather than row counts.
        let watermark = app.transcript_scrollback.entries[entry].position;
        assert!(watermark >= before);
        let all = render_entry_lines(&app.entries, &ThemePalette::default());
        let pending = pending_entry_rows(
            &app.entries[entry],
            entry,
            width as usize,
            &ThemePalette::default(),
            &PreviewState::default(),
            app.transcript_scrollback.entries[entry],
        );
        assert_eq!(
            pending
                .iter()
                .map(|row| line_text(&row.line))
                .collect::<String>(),
            all[0]
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()[watermark.byte..]
        );
    }
}

#[test]
fn transcript_wrap_uses_terminal_cells_and_keeps_graphemes_and_styles() {
    use ratatui::style::Color;
    let style = Style::default().fg(Color::Cyan);
    let line = Line::from(vec![
        Span::raw("● "),
        Span::styled("界界e\u{301}👩‍💻\tend", style),
    ]);
    let rows = wrap_positioned_line(line, 0, 8);
    let text = rows
        .iter()
        .map(|row| line_text(&row.line))
        .collect::<String>();
    assert_eq!(text, "● 界界e\u{301}👩‍💻    end");
    assert!(rows.iter().all(|row| row.line.width() <= 8));
    assert!(rows
        .iter()
        .flat_map(|row| &row.line.spans)
        .filter(|span| !span.content.contains('●'))
        .all(|span| span.style == style));
}

#[test]
fn temporary_panels_do_not_commit_hidden_transcript_rows() {
    let config = minimal_test_config();
    let mut app = TuiApp::default();
    app.push(TuiEntryKind::Assistant, "long answer\n".repeat(50));
    let mut terminal = scrollback_test_terminal(40, 20);
    app.overlay = Overlay::Selector(TuiSelectorState::new(
        "model",
        Selector::new("model", vec![]),
        "",
        None,
    ));
    redraw_tui(&mut terminal, &mut app, &config).unwrap();
    terminal.backend().assert_scrollback_empty();
    app.close_overlay(|o| matches!(o, Overlay::Selector(_)));
    app.overlay = Overlay::DiffPanel(DiffPanelState {
        files: vec![],
        selected: 0,
        detail: None,
    });
    redraw_tui(&mut terminal, &mut app, &config).unwrap();
    terminal.backend().assert_scrollback_empty();
    app.close_overlay(|o| matches!(o, Overlay::DiffPanel(_)));
    redraw_tui(&mut terminal, &mut app, &config).unwrap();
    assert!(terminal.backend().scrollback().area.height > 0);
}

#[test]
fn transcript_render_is_bounded_while_streaming() {
    let mut app = TuiApp::default();
    for index in 0..1_000 {
        app.push(TuiEntryKind::Assistant, format!("old response {index}"));
    }
    app.push_placeholder(TuiEntryKind::Assistant, "word ".repeat(300));

    let visible = visible_transcript(&app.entries, 20, 12);

    assert_eq!(visible.entries_used, 1);
    assert!(visible.visual_height >= 12);
}

#[test]
fn transcript_render_is_bounded_after_streaming() {
    let mut app = TuiApp::default();
    for index in 0..1_000 {
        app.push(TuiEntryKind::Assistant, format!("old response {index}"));
    }

    let visible = visible_transcript(&app.entries, 80, 20);

    assert!(visible.entries_used < app.entries.len());
    assert!(visible.entries_used <= 24);
    assert!(visible.visual_height >= 20);
}

#[test]
fn transcript_render_keeps_latest_entry_after_wrapped_history() {
    let entries = vec![
        TuiEntry {
            kind: TuiEntryKind::Assistant,
            text: "stream".repeat(120),
        },
        TuiEntry {
            kind: TuiEntryKind::System,
            text: "queued> draft while streaming".to_string(),
        },
        TuiEntry {
            kind: TuiEntryKind::Assistant,
            text: "[faux/echo] draft while streaming".to_string(),
        },
    ];

    let visible = visible_transcript(&entries, 80, 8);
    let text = visible
        .lines
        .iter()
        .map(line_text)
        .collect::<Vec<_>>()
        .join("\n");

    assert!(text.contains("[faux/echo] draft while streaming"));
}

#[test]
fn auto_restart_debounces_binary_changes() {
    let dir = std::env::temp_dir().join(format!("noel-autorestart-test-{}", std::process::id()));
    fs::create_dir_all(&dir).expect("create temp dir");
    let exe = dir.join("noel");
    fs::write(&exe, b"v1").expect("write binary");
    let original = fs::metadata(&exe)
        .expect("metadata")
        .modified()
        .expect("mtime");
    let mut restart = AutoRestart {
        executable: exe.clone(),
        modified: Some(original),
        pending: None,
    };

    assert!(!restart.should_restart().expect("check"));

    let rebuilt = original + Duration::from_secs(10);
    fs::File::options()
        .write(true)
        .open(&exe)
        .expect("open")
        .set_modified(rebuilt)
        .expect("set mtime");
    assert!(!restart.should_restart().expect("first sighting"));
    assert!(!restart.should_restart().expect("within debounce"));

    restart.pending = restart
        .pending
        .map(|(mtime, since)| (mtime, since - RESTART_DEBOUNCE));
    assert!(restart.should_restart().expect("after debounce"));

    let rebuilt_again = rebuilt + Duration::from_secs(10);
    fs::File::options()
        .write(true)
        .open(&exe)
        .expect("open")
        .set_modified(rebuilt_again)
        .expect("set mtime");
    assert!(!restart.should_restart().expect("new build resets debounce"));

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn resume_hint_flags_interrupted_turns_and_pending_queue() {
    let mut session = SessionState::new("session-1", PathBuf::from("."));
    assert!(hint_of(&session).is_none());

    session.messages.push(ConversationMessage {
        thinking: String::new(),
        role: MessageRole::Tool,
        content: "partial".to_string(),
        media: Vec::new(),
        tool_call_id: Some("call_1".to_string()),
        tool_name: Some("bash".to_string()),
        tool_calls: Vec::new(),
        aborted: false,
    });
    let hint = hint_of(&session).expect("mid-turn hint");
    assert!(hint.contains("mid-turn"));
    assert!(hint.contains("continuing automatically"));

    session.queue = vec![noel_core::QueuedEntry::follow_up("follow up")];
    let hint = hint_of(&session).expect("mid-turn and queue hint");
    assert!(hint.contains("mid-turn"));
    // Durable's continue places the queue first.
    assert!(hint.contains("with 1 queued message(s) first"), "{hint}");
}

fn hint_of(session: &SessionState) -> Option<String> {
    resume_hint(session.has_unfinished_turn(), session.queue.len())
}

#[tokio::test]
async fn completed_slash_read_reopens_without_mid_turn_warning() {
    let root = std::env::temp_dir().join(format!("noel-direct-resume-{}", unique_temp_suffix()));
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("fixture.txt"), "complete output").unwrap();
    let (store, state) = SessionStore::create(&root.join("sessions"), root.clone()).unwrap();
    let path = store.path().to_path_buf();
    let mut runtime = Runtime::with_store(state, ReloadableSystems::default(), store);
    let provider = noel_ai::create_provider(ProviderConfig {
        model: ModelRef {
            provider: "faux".into(),
            id: "echo".into(),
        },
        api: AiProviderApi::Faux,
        base_url: None,
        auth: ProviderAuth::None,
        thinking_level: None,
        thinking_budget_tokens: None,
        session_id: None,
        traits: noel_ai::ModelTraits::default(),
    });
    let output = run_user_turn(&mut runtime, provider.as_ref(), "/read fixture.txt".into())
        .await
        .unwrap();
    assert_eq!(output, "complete output");
    assert_eq!(
        runtime.session().messages.last().unwrap().role,
        MessageRole::Tool
    );
    let (_store, state) = SessionStore::open(path).unwrap();
    assert!(hint_of(&state).is_none());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn activity_status_shows_phase_and_pending_followups() {
    let started = Instant::now();
    let waiting = activity_status(started, &Activity::Waiting, 0);
    assert!(waiting.contains("waiting"));
    assert!(waiting.contains("esc to interrupt"));
    assert!(!waiting.contains("queued"));

    let tool = activity_status(started, &Activity::Tool("bash".to_string()), 2);
    assert!(tool.contains("running bash"));
    assert!(tool.contains("+2 queued"));
}

#[test]
fn prompt_history_navigates_with_draft_restore() {
    let mut app = TuiApp::default();
    app.editor_state.record_history("first");
    app.editor_state.record_history("second");
    app.set_input("draft");

    app.history_previous();
    assert_eq!(app.input, "second");
    app.history_previous();
    assert_eq!(app.input, "first");
    app.history_previous();
    assert_eq!(app.input, "first");

    app.history_next();
    assert_eq!(app.input, "second");
    app.history_next();
    assert_eq!(app.input, "draft");
}

#[test]
fn bracketed_paste_appends_to_input_and_exits_history_navigation() {
    let mut app = TuiApp::default();
    app.editor_state.record_history("old prompt");
    app.history_previous();
    assert_eq!(app.input, "old prompt");

    app.paste_text(" pasted");
    assert_eq!(app.input, "old prompt pasted");
    assert_eq!(input_area_height(&app, Rect::new(0, 0, 80, 20), 0), 3);
    app.history_next();
    assert_eq!(app.input, "old prompt pasted");
}

#[test]
fn shift_enter_adds_newline_to_input() {
    let mut app = TuiApp::default();
    app.set_input("first");
    app.editor_state.record_history("old prompt");
    app.history_previous();
    assert_eq!(app.input, "old prompt");

    app.set_input("first");
    app.insert_input_newline();
    app.insert_input_str("second");

    assert_eq!(app.input, "first\nsecond");
    let bindings = keybinding_map(&minimal_test_config());
    assert!(bindings.matches("newline", "shift+enter"));
    assert!(bindings.matches("newline", "ctrl+j"));
    assert!(!bindings.matches("newline", "enter"));
    app.history_next();
    assert_eq!(app.input, "first\nsecond");
}

#[test]
fn single_line_input_is_centered_between_rounded_borders() {
    use ratatui::backend::TestBackend;
    let mut app = TuiApp {
        show_hardware_cursor: true,
        ..Default::default()
    };
    app.set_input("draft");
    let mut terminal = Terminal::new(TestBackend::new(20, 3)).unwrap();
    terminal
        .draw(|frame| {
            draw_input(frame, frame.area(), &app, &ThemePalette::default());
            set_tui_cursor(frame, frame.area(), &app);
        })
        .unwrap();
    assert_eq!(
        buffer_rows(terminal.backend().buffer()),
        [
            "╭──────────────────╮",
            "│ > draft          │",
            "╰──────────────────╯",
        ]
    );
    terminal.backend_mut().assert_cursor_position((9, 1));
}

#[test]
fn typed_newlines_expand_input_area_and_pastes_show_numbered_markers() {
    let mut app = TuiApp::default();
    app.set_input("one");
    for line in ["two", "three", "four"] {
        app.insert_input_newline();
        for ch in line.chars() {
            app.push_input_char(ch);
        }
    }
    assert_eq!(input_layout(&app, 76).rows.len(), 4);
    assert_eq!(input_area_height(&app, Rect::new(0, 0, 80, 20), 0), 6);
    let layout = input_layout(&app, 76);
    let lines = render_input_lines(
        &app,
        Rect::new(0, 0, 76, 4),
        Style::default(),
        Style::default(),
    );
    assert_eq!(layout.cursor_row, 3);
    assert_eq!(
        lines.iter().map(line_text).collect::<Vec<_>>(),
        ["> one", "  two", "  three", "  four",]
    );

    let mut pasted = TuiApp::default();
    pasted.paste_text("one\ntwo\nthree\nfour");
    assert_eq!(input_layout(&pasted, 76).rows.len(), 1);
    assert_eq!(input_area_height(&pasted, Rect::new(0, 0, 80, 20), 0), 3);
    let lines = render_input_lines(
        &pasted,
        Rect::new(0, 0, 76, 1),
        Style::default(),
        Style::default(),
    );
    assert_eq!(line_text(&lines[0]), "> [paste #1 +4 lines]");
    assert_eq!(pasted.input, "one\ntwo\nthree\nfour");
}

#[test]
fn boxed_editor_wraps_and_grows_upward_then_shrinks_on_resize() {
    use ratatui::backend::TestBackend;
    let mut app = TuiApp {
        show_hardware_cursor: true,
        ..Default::default()
    };
    app.set_input("abcdefghijklmnop");
    let config = minimal_test_config();
    for (width, height, expected_rows, cursor) in [
        (
            12,
            5,
            vec!["│ > abcdef │", "│   ghijkl │", "│   mnop   │"],
            (8, 7),
        ),
        (24, 3, vec!["│ > abcdefghijklmnop   │"], (20, 7)),
    ] {
        assert_eq!(input_desired_height(&app, width), height);
        let mut terminal = Terminal::new(TestBackend::new(width, 10)).unwrap();
        terminal
            .draw(|frame| draw_tui(frame, &app, &config))
            .unwrap();
        let rows = buffer_rows(terminal.backend().buffer());
        assert_eq!(&rows[(10 - height) as usize..8], expected_rows.as_slice());
        assert!(rows[(9 - height) as usize].starts_with('╭'));
        assert!(rows[8].starts_with('╰'));
        terminal.backend_mut().assert_cursor_position(cursor);
    }
    app.cursor_to_line_start();
    let lines = render_input_lines(
        &app,
        Rect::new(0, 0, 8, 1),
        Style::default(),
        Style::default(),
    );
    assert_eq!(line_text(&lines[0]), "> abcdef");
    app.set_input("abc界界界e\u{301}");
    let mut terminal = Terminal::new(TestBackend::new(12, 4)).unwrap();
    terminal
        .draw(|frame| {
            draw_input(frame, frame.area(), &app, &ThemePalette::default());
            set_tui_cursor(frame, frame.area(), &app);
        })
        .unwrap();
    assert_eq!(
        buffer_rows(terminal.backend().buffer()),
        [
            "╭──────────╮",
            "│ > abc界  │",
            "│   界界e\u{301}  │",
            "╰──────────╯"
        ]
    );
    terminal.backend_mut().assert_cursor_position((9, 2));
}

#[test]
fn editor_wrap_reserves_an_insertion_cell_and_preserves_graphemes() {
    let mut app = TuiApp::default();
    app.set_input("abcdef");
    let layout = input_layout(&app, 8);
    assert_eq!(layout.rows, ["abcdef", ""]);
    assert_eq!((layout.cursor_row, layout.cursor_column), (1, 0));
    app.move_cursor_left();
    let layout = input_layout(&app, 8);
    assert_eq!((layout.cursor_row, layout.cursor_column), (0, 5));
    app.set_input("ab界e\u{301}界\u{301}xy");
    let layout = input_layout(&app, 8);
    assert_eq!(layout.rows, ["ab界e\u{301}", "界\u{301}xy"]);
    app.move_cursor_left();
    app.move_cursor_left();
    app.move_cursor_left();
    assert_eq!(&app.input[app.input_cursor..], "界\u{301}xy");
    app.move_cursor_right();
    app.pop_input_char();
    assert_eq!(app.input, "ab界e\u{301}xy");
    assert_eq!(input_layout(&app, 8).rows, ["ab界e\u{301}x", "y"]);
}

#[test]
fn tall_drafts_are_bounded_and_scroll_to_the_cursor() {
    let mut app = TuiApp::default();
    app.set_input(
        (0..50)
            .map(|index| format!("row {index}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    assert_eq!(input_desired_height(&app, 80), MAX_INPUT_ROWS as u16 + 2);
    assert_eq!(input_area_height(&app, Rect::new(0, 0, 80, 6), 0), 4);
    let visible = |app: &TuiApp| {
        render_input_lines(
            app,
            Rect::new(0, 0, 76, 2),
            Style::default(),
            Style::default(),
        )
        .iter()
        .map(line_text)
        .collect::<Vec<_>>()
    };
    assert_eq!(visible(&app), ["> row 48", "  row 49"]);
    app.input_cursor = 0;
    assert_eq!(visible(&app), ["> row 0", "  row 1"]);
    assert_eq!(app.input.lines().count(), 50);
}

#[test]
fn large_paste_markers_keep_full_text_and_allow_surrounding_edits() {
    let mut app = TuiApp::default();
    let paste = (0..500)
        .map(|index| format!("line {index}\n"))
        .collect::<String>();
    app.insert_input_str("before  after");
    app.input_cursor = 7;
    app.paste_text(&paste);
    assert_eq!(app.input, format!("before {paste} after"));
    assert_eq!(app.display_input().0, "before [paste #1 +500 lines] after");
    assert_eq!(input_desired_height(&app, 80), 3);
    app.insert_input_str("!");
    app.move_cursor_left();
    app.move_cursor_left();
    assert_eq!(app.input_cursor, 7);
    assert_eq!(app.display_input().1, 7);
    app.insert_input_str("prefix ");
    assert_eq!(
        app.display_input().0,
        "before prefix [paste #1 +500 lines]! after"
    );
    app.cursor_to_line_end();
    assert_eq!(app.input_cursor, app.input.len());
    app.cursor_to_line_start();
    assert_eq!(app.input_cursor, 0);
    app.input_cursor = app.input_pastes[0].start;
    app.move_cursor_right();
    app.pop_input_char();
    assert_eq!(app.input, "before prefix ! after");
    assert!(app.input_pastes.is_empty());
}

#[test]
fn multiple_pastes_keep_ids_and_ranges_when_deleted_or_cleared() {
    let mut app = TuiApp::default();
    app.paste_text("one\ntwo");
    app.insert_input_str(" / ");
    app.paste_text("three\nfour\nfive");
    app.cursor_to_line_start();
    app.paste_text("first\nsecond");
    assert_eq!(
        app.display_input().0,
        "[paste #3 +2 lines][paste #1 +2 lines] / [paste #2 +3 lines]"
    );
    app.delete_input_char();
    assert_eq!(
        app.display_input().0,
        "[paste #3 +2 lines] / [paste #2 +3 lines]"
    );
    app.move_cursor_left();
    app.delete_input_char();
    assert_eq!(app.display_input().0, " / [paste #2 +3 lines]");
    app.clear_input();
    app.paste_text("new\npaste");
    assert_eq!(app.display_input().0, "[paste #4 +2 lines]");
    app.clear_input();
    assert_eq!(app.display_input(), (String::new(), 0));
}

#[test]
fn paste_normalizes_terminal_newlines_and_compacts_long_single_lines() {
    let mut app = TuiApp::default();
    app.paste_text("first\r\nsecond\rthird");
    assert_eq!(app.input, "first\nsecond\nthird");
    assert_eq!(app.display_input().0, "[paste #1 +3 lines]");
    app.clear_input();
    let text = "界".repeat(LARGE_PASTE_CHARS + 1);
    app.paste_text(&text);
    assert_eq!(app.input, text);
    assert_eq!(app.display_input().0, "[paste #2 +1 line]");
    app.clear_input();
    app.paste_text("");
    app.paste_text("short paste");
    assert!(app.input_pastes.is_empty());
    assert_eq!(app.display_input().0, "short paste");
}

#[test]
fn history_navigation_restores_paste_markers_and_the_draft_cursor() {
    let mut app = TuiApp::default();
    app.editor_state.record_history("old prompt");
    app.insert_input_str("draft ");
    app.paste_text("one\ntwo");
    app.move_cursor_left();
    let before = app.display_input();
    let cursor = app.input_cursor;
    app.history_previous();
    assert_eq!(app.display_input().0, "old prompt");
    app.history_next();
    assert_eq!(app.display_input(), before);
    assert_eq!(app.input_cursor, cursor);
    assert_eq!(app.input, "draft one\ntwo");
}

fn buffer_rows(buffer: &ratatui::buffer::Buffer) -> Vec<String> {
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .filter_map(|x| {
                    // Wide graphemes own the following cell, which is a blank
                    // placeholder in ratatui's buffer, not an extra character.
                    let cell = &buffer[(x, y)];
                    if x > 0 && buffer[(x - 1, y)].symbol().width() > 1 {
                        None
                    } else {
                        Some(cell.symbol())
                    }
                })
                .collect()
        })
        .collect()
}

#[test]
fn clear_visible_resets_transient_tui_state_only() {
    let mut app = TuiApp {
        overlay: Overlay::Multiline(vec!["one".to_string()]),
        live_entry_index: Some(0),
        ..TuiApp::default()
    };
    app.set_input("draft");
    app.push(TuiEntryKind::User, "hello");
    app.editor_state.record_history("old prompt");
    app.history_previous();

    app.clear_visible();

    assert!(app.entries.is_empty());
    assert!(app.input.is_empty());
    assert!(app.multiline().is_none());
    assert!(app.selector().is_none());
    assert!(app.live_entry_index.is_none());
    assert_eq!(input_layout(&app, 76).rows.len(), 1);
    assert_eq!(app.editor_state.history(), ["old prompt"]);
}

#[test]
fn slash_command_matches_follow_current_input() {
    let config = LoadedConfig {
        paths: test_config_paths(),
        settings: Settings::default(),
        auth: AuthData::default(),
        models: Vec::new(),
        model_scope: Vec::new(),
        image_models: Vec::new(),
        keybindings: Vec::new(),
        context_files: Vec::new(),
        extensions: vec![test_resource("json-ext")],
        skills: Vec::new(),
        agents: Vec::new(),
        mcp_servers: Vec::new(),
        prompt_templates: Vec::new(),
        themes: Vec::new(),
        diagnostics: Vec::new(),
        system_prompt: None,
        append_system_prompt: Vec::new(),
    };
    let mut app = TuiApp::default();
    app.set_input("/cl");

    let matches = slash_command_matches(&config, &app);
    let commands: Vec<&str> = matches.iter().map(|m| m.command.as_str()).collect();
    assert!(commands.contains(&"/clear"));
    assert!(commands.contains(&"/clone [id|name|path]"));
    // Every builtin row carries its COMMAND_HELP description.
    let clear = matches.iter().find(|m| m.command == "/clear").unwrap();
    assert_eq!(clear.description, "clear visible transcript");

    app.set_input("not slash");
    assert!(slash_command_matches(&config, &app).is_empty());

    app.set_input("/extension:j");
    assert_eq!(
        slash_command_matches(&config, &app)
            .iter()
            .map(|m| m.command.as_str())
            .collect::<Vec<_>>(),
        vec!["/extension:json-ext"]
    );
}

#[test]
fn slash_list_navigates_and_picks_upstream_select_semantics() {
    let config = LoadedConfig {
        paths: test_config_paths(),
        settings: Settings::default(),
        auth: AuthData::default(),
        models: Vec::new(),
        model_scope: Vec::new(),
        image_models: Vec::new(),
        keybindings: Vec::new(),
        context_files: Vec::new(),
        extensions: Vec::new(),
        skills: Vec::new(),
        agents: Vec::new(),
        mcp_servers: Vec::new(),
        prompt_templates: Vec::new(),
        themes: Vec::new(),
        diagnostics: Vec::new(),
        system_prompt: None,
        append_system_prompt: Vec::new(),
    };
    let mut app = TuiApp::default();
    app.set_input("/e");
    assert_eq!(app.slash_selection, None);

    // Arrows move the selection, wrapping at both ends as upstream does.
    slash_move_selection(&mut app, &config, 1);
    assert_eq!(app.slash_selection, Some(1));
    slash_move_selection(&mut app, &config, -2);
    assert_eq!(
        app.slash_selection,
        Some(slash_command_matches(&config, &app).len() - 1)
    );
    slash_move_selection(&mut app, &config, 1);
    assert_eq!(app.slash_selection, Some(0));

    // The picked completion drops its placeholder: the draft becomes the
    // command head plus a space, ready for arguments.
    let (completion, _) = slash_selected_match(&app, &config).unwrap();
    apply_slash_completion(&mut app, &completion);
    assert_eq!(
        app.input,
        format!("{} ", completion.split_once(' ').unwrap().0)
    );
    // Accepting rewrote the input, so the selection reset with it.
    assert_eq!(app.slash_selection, None);

    // A draft that is exactly the selected command has nothing to pick.
    app.set_input("/editor");
    app.slash_selection = Some(0);
    let (command, _) = slash_selected_match(&app, &config).unwrap();
    assert_eq!(command, "/editor [text]");
    apply_slash_completion(&mut app, &command);
    assert_eq!(app.input, "/editor ");
}

#[test]
fn dogfood_restart_drops_model_cli_overrides() {
    let args = [
        "--continue",
        "--model",
        "faux/echo",
        "--provider=openai",
        "--theme",
        "dark",
    ]
    .into_iter()
    .map(OsString::from);

    let filtered = restart_args(args, None);

    assert_eq!(
        filtered,
        ["--continue", "--theme", "dark"]
            .into_iter()
            .map(OsString::from)
            .collect::<Vec<_>>()
    );
}

#[test]
fn dogfood_restart_reopens_the_session_it_had_open() {
    let rewrite = |args: &[&str]| {
        restart_args(
            args.iter().map(OsString::from),
            Some(Path::new("/sessions/current.jsonl")),
        )
        .into_iter()
        .map(|arg| arg.into_string().unwrap())
        .collect::<Vec<_>>()
    };
    let reopen = ["--session", "/sessions/current.jsonl"];

    // `make dogfood` runs `--continue --model faux/echo`.
    assert_eq!(rewrite(&["--continue", "--model", "faux/echo"]), reopen);
    for selection in [
        &["-c"][..],
        &["-r"],
        &["--resume"],
        &["--fork", "abc"],
        &["--fork=abc"],
        &["--session", "older.jsonl"],
        &["--session=older.jsonl"],
        &["--session-id", "named"],
        &["--session-id=named"],
        &["--no-session"],
    ] {
        assert_eq!(rewrite(selection), reopen, "{selection:?}");
    }
    assert_eq!(
        rewrite(&[
            "-cpn",
            "label",
            "--session-dir",
            "sessions",
            "--theme",
            "dark",
            "-rt",
            "read",
            "--",
            "-c",
        ]),
        [
            "-pn",
            "label",
            "--session-dir",
            "sessions",
            "--theme",
            "dark",
            "-t",
            "read",
            "--session",
            "/sessions/current.jsonl",
            "--",
            "-c",
        ]
    );
    // Without a session file (`--no-session`) the selection stays.
    assert_eq!(
        restart_args(
            ["--no-session", "--model", "faux/echo"].map(OsString::from),
            None
        ),
        [OsString::from("--no-session")]
    );
}

/// Paths in a fresh directory under the system temp dir, so a test never
/// reads or writes the real `~/.noel` or the repository (nothing is
/// created until a test writes).
fn test_config_paths() -> ConfigPaths {
    let root = std::env::temp_dir().join(format!("noel-cli-unit-{}", unique_temp_suffix()));
    let agent_dir = root.join("agent");
    ConfigPaths {
        cwd: PathBuf::from("."),
        session_dir: agent_dir.join("sessions"),
        settings_path: agent_dir.join("settings.json"),
        project_settings_path: root.join("project/.noel/settings.json"),
        auth_path: agent_dir.join("auth.json"),
        models_path: agent_dir.join("models.json"),
        model_cache_path: agent_dir.join("model-cache.json"),
        keybindings_path: agent_dir.join("keybindings.json"),
        agent_dir,
    }
}

pub(super) fn minimal_test_config() -> LoadedConfig {
    LoadedConfig {
        paths: test_config_paths(),
        settings: Settings::default(),
        auth: AuthData::default(),
        models: Vec::new(),
        model_scope: Vec::new(),
        image_models: Vec::new(),
        keybindings: Vec::new(),
        context_files: Vec::new(),
        extensions: Vec::new(),
        skills: Vec::new(),
        agents: Vec::new(),
        mcp_servers: Vec::new(),
        prompt_templates: Vec::new(),
        themes: Vec::new(),
        diagnostics: Vec::new(),
        system_prompt: None,
        append_system_prompt: Vec::new(),
    }
}

#[test]
fn input_cursor_inserts_and_deletes_at_cursor() {
    let mut app = TuiApp::default();
    for ch in "helo".chars() {
        app.push_input_char(ch);
    }
    app.move_cursor_left();
    app.push_input_char('l');
    assert_eq!(app.input, "hello");
    assert_eq!(app.input_cursor, 4);

    app.cursor_to_line_start();
    app.push_input_char('>');
    assert_eq!(app.input, ">hello");

    app.cursor_to_line_end();
    app.move_cursor_left();
    app.pop_input_char();
    assert_eq!(app.input, ">helo");
    assert_eq!(app.input_cursor, 4);
}

#[test]
fn line_navigation_respects_newlines() {
    let mut app = TuiApp::default();
    app.set_input("first\nsecond");

    app.cursor_to_line_start();
    assert_eq!(app.input_cursor, 6);
    app.move_cursor_left();
    assert_eq!(app.input_cursor, 5);
    app.cursor_to_line_start();
    assert_eq!(app.input_cursor, 0);
    app.cursor_to_line_end();
    assert_eq!(app.input_cursor, 5);
    app.move_cursor_right();
    app.cursor_to_line_end();
    assert_eq!(app.input_cursor, 12);
}

#[test]
fn quit_requires_double_press_within_window() {
    let mut app = TuiApp::default();
    let now = Instant::now();

    assert!(!app.quit_requested(now));
    assert!(app.quit_requested(now + Duration::from_secs(1)));
}

#[test]
fn quit_arm_does_not_touch_the_draft() {
    let mut app = TuiApp::default();
    app.set_input("draft");

    assert!(!app.quit_requested(Instant::now()));
    assert_eq!(app.input, "draft");
}

#[test]
fn quit_arm_expires() {
    let mut app = TuiApp::default();
    let now = Instant::now();

    assert!(!app.quit_requested(now));
    assert!(!app.quit_requested(now + QUIT_CONFIRM_WINDOW));
}

#[test]
fn key_event_name_formats_modifiers() {
    assert_eq!(
        key_event_name(&KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL)),
        Some("ctrl+a".to_string())
    );
    assert_eq!(
        key_event_name(&KeyEvent::new(KeyCode::Home, KeyModifiers::NONE)),
        Some("home".to_string())
    );
    assert_eq!(
        key_event_name(&KeyEvent::new(KeyCode::Left, KeyModifiers::SUPER)),
        Some("super+left".to_string())
    );
    assert_eq!(
        key_event_name(&KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)),
        Some("shift+enter".to_string())
    );
}

#[test]
fn persist_default_model_writes_settings_and_preserves_other_keys() {
    let root = std::env::temp_dir().join(format!(
        "noel-persist-model-test-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    fs::create_dir_all(&root).expect("create temp dir");
    let settings_path = root.join("settings.json");
    fs::write(&settings_path, "{\n  \"theme\": \"dark\"\n}\n").expect("seed settings");
    let mut config = LoadedConfig {
        paths: ConfigPaths {
            settings_path: settings_path.clone(),
            ..test_config_paths()
        },
        settings: Settings::default(),
        auth: AuthData::default(),
        models: Vec::new(),
        model_scope: Vec::new(),
        image_models: Vec::new(),
        keybindings: Vec::new(),
        context_files: Vec::new(),
        extensions: Vec::new(),
        skills: Vec::new(),
        agents: Vec::new(),
        mcp_servers: Vec::new(),
        prompt_templates: Vec::new(),
        themes: Vec::new(),
        diagnostics: Vec::new(),
        system_prompt: None,
        append_system_prompt: Vec::new(),
    };
    let model = ModelRef {
        provider: "anthropic".to_string(),
        id: "claude-sonnet-4-6".to_string(),
    };

    persist_default_model(&mut config, &model).expect("persist default model");

    assert_eq!(
        config.settings.default_provider.as_deref(),
        Some("anthropic")
    );
    assert_eq!(
        config.settings.default_model.as_deref(),
        Some("claude-sonnet-4-6")
    );
    let written: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&settings_path).expect("read settings"))
            .expect("parse settings");
    assert_eq!(written["defaultProvider"], "anthropic");
    assert_eq!(written["defaultModel"], "claude-sonnet-4-6");
    assert_eq!(written["theme"], "dark");
    let _ = fs::remove_dir_all(root);
}

pub(super) fn line_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>()
}

#[test]
fn model_lists_are_grouped_by_provider_and_id() {
    let models = vec![
        test_model("openai-codex", "gpt-5.6-terra-wm"),
        test_model("anthropic", "claude-sonnet-4-6"),
        test_model("openai-codex", "gpt-5.6-luna-wm"),
    ];

    let sorted = sorted_models(&models)
        .into_iter()
        .map(|model| format!("{}/{}", model.provider, model.id))
        .collect::<Vec<_>>();

    assert_eq!(
        sorted,
        [
            "anthropic/claude-sonnet-4-6",
            "openai-codex/gpt-5.6-luna-wm",
            "openai-codex/gpt-5.6-terra-wm",
        ]
    );
}

#[test]
fn codex_discovery_uses_selectable_model_slugs_only() {
    let response = serde_json::json!({
        "models": [
            {"slug": "gpt-5.6-terra", "visibility": "list"},
            {"slug": "gpt-6-sol", "visibility": "list"},
            {"slug": "gpt-5.6-luna", "visibility": "list"},
            {"slug": "codex-auto-review", "visibility": "hide"},
            {"slug": "gpt-5.6-sol-wm", "visibility": "hide"},
        ],
        "unrelated": {"id": "gpt-5.6-terra-wm"},
    });

    assert_eq!(
        collect_codex_model_ids(&response, "openai-codex"),
        BTreeSet::from([
            "gpt-5.6-luna".to_string(),
            "gpt-5.6-terra".to_string(),
            "gpt-6-sol".to_string(),
        ])
    );
}

#[test]
fn model_filter_admits_new_generation_ids() {
    assert!(model_supported_for_provider("openai", "gpt-6-sol"));
    assert!(model_supported_for_provider("openai-codex", "gpt-6-sol"));
    assert!(model_supported_for_provider(
        "openai-codex",
        "gpt-5.3-codex"
    ));
    assert!(!model_supported_for_provider(
        "openai-codex",
        "gpt-6-sol-realtime"
    ));
    assert!(model_supported_for_provider("zai", "glm-5.1"));
    assert!(model_supported_for_provider("zai-coding", "glm-5.1"));
    assert!(!model_supported_for_provider("zai", "embedding-3"));
    assert!(model_supported_for_provider(
        "moonshotai",
        "kimi-k3-0905-preview"
    ));
    assert!(model_supported_for_provider(
        "kimi-coding-openai",
        "kimi-for-coding"
    ));
    assert!(!model_supported_for_provider("github-copilot", "gpt-6-sol"));
}

#[test]
fn anthropic_models_url_paginates_with_cursor() {
    assert_eq!(
        anthropic_models_url(None),
        "https://api.anthropic.com/v1/models?limit=1000"
    );
    assert_eq!(
        anthropic_models_url(Some("claude-opus-4-8")),
        "https://api.anthropic.com/v1/models?limit=1000&starting_after=claude-opus-4-8"
    );
}

#[test]
fn anthropic_models_response_reads_pagination_fields() {
    let page: AnthropicModelsResponse = serde_json::from_value(serde_json::json!({
        "data": [{"id": "claude-opus-4-8", "display_name": "Claude Opus 4.8"}],
        "has_more": true,
        "first_id": "claude-opus-4-8",
        "last_id": "claude-opus-4-8",
    }))
    .expect("parse page");
    assert!(page.has_more);
    assert_eq!(page.last_id.as_deref(), Some("claude-opus-4-8"));

    let legacy: AnthropicModelsResponse = serde_json::from_value(serde_json::json!({
        "data": [{"id": "claude-sonnet-4-6", "display_name": "Claude Sonnet 4.6"}],
    }))
    .expect("parse legacy page");
    assert!(!legacy.has_more);
    assert_eq!(legacy.last_id, None);
}

#[test]
fn model_cache_version_forces_refresh_for_old_caches() {
    let root = std::env::temp_dir().join(format!(
        "noel-cli-model-cache-version-{}",
        unique_temp_suffix()
    ));
    fs::create_dir_all(&root).expect("create temp dir");
    let path = root.join("model-cache.json");
    let now = unix_seconds().expect("unix seconds");

    write_model_cache(
        &path,
        &ModelCache {
            refreshed_at: now,
            ..ModelCache::default()
        },
    )
    .expect("write legacy cache");
    assert!(model_cache_needs_refresh(&path, 24));

    write_model_cache(
        &path,
        &ModelCache {
            refreshed_at: now,
            version: MODEL_CACHE_VERSION,
            models: Vec::new(),
            diagnostics: Vec::new(),
            claude_code_version: None,
        },
    )
    .expect("write current cache");
    assert!(!model_cache_needs_refresh(&path, 24));
    assert!(model_cache_needs_refresh(&path, 0));

    let _ = fs::remove_dir_all(root);
}

fn refresh_test_model(provider: &str, id: &str) -> ModelDefinition {
    ModelDefinition {
        context_window: None,
        provider: provider.to_string(),
        id: id.to_string(),
        name: Some(id.to_string()),
        api: ConfigProviderApi::OpenAi,
        base_url: None,
        ..ModelDefinition::default()
    }
}

fn refresh_test_cache(models: Vec<ModelDefinition>) -> ModelCache {
    ModelCache {
        refreshed_at: 100,
        version: MODEL_CACHE_VERSION,
        models,
        diagnostics: Vec::new(),
        claude_code_version: None,
    }
}

#[test]
fn model_refresh_replaces_provider_but_keeps_other_providers() {
    let existing = refresh_test_cache(vec![
        refresh_test_model("zai-coding", "glm-5.3"),
        refresh_test_model("openai", "gpt-5"),
    ]);
    let mut report = ModelRefreshReport::default();
    report.record(
        "openai",
        Some(&existing),
        Ok(vec![refresh_test_model("openai", "gpt-6")]),
    );
    let next = next_model_cache(Some(&existing), &report, Some(200)).unwrap();
    assert_eq!(next.refreshed_at, 200);
    assert_eq!(next.version, MODEL_CACHE_VERSION);
    let ids: Vec<&str> = next.models.iter().map(|model| model.id.as_str()).collect();
    assert_eq!(ids, ["glm-5.3", "gpt-6"]);
    assert!(next.diagnostics.is_empty());
}

#[test]
fn model_refresh_empty_fetch_keeps_cached_models_and_reports() {
    let existing = refresh_test_cache(vec![refresh_test_model("zai-coding", "glm-5.3")]);
    let mut report = ModelRefreshReport::default();
    report.record("zai-coding", Some(&existing), Ok(Vec::new()));
    let next = next_model_cache(Some(&existing), &report, Some(200)).unwrap();
    // The cache contents are untouched; only diagnostics record the event.
    assert_eq!(next.models, existing.models);
    assert_eq!(next.refreshed_at, existing.refreshed_at);
    assert_eq!(
        next.diagnostics,
        ["zai-coding model refresh returned an empty list; kept cached models".to_string()]
    );
    assert_eq!(report.diagnostics(), next.diagnostics);
    assert_eq!(report.kept_cached, ["zai-coding".to_string()]);
    assert_eq!(report.refreshed.len(), 0);
    // The formatted report explains the outcome.
    let config = account_test_config(AuthData::default());
    let output = format_model_refresh_report(&report, &config);
    assert!(output.contains("kept cached models after empty response: zai-coding"));
}

#[test]
fn model_refresh_failure_keeps_cache_stale_and_persists_diagnostics() {
    // Cache is old enough that the TTL already expired, then refresh fails.
    let mut existing = refresh_test_cache(vec![refresh_test_model("zai-coding", "glm-5.3")]);
    existing.refreshed_at = 0;
    let mut report = ModelRefreshReport::default();
    report.record("zai-coding", Some(&existing), Err(anyhow!("status 503")));
    let next = next_model_cache(Some(&existing), &report, Some(200)).unwrap();
    // TTL is not reset: next startup (past the original TTL) retries
    // instead of waiting out another full TTL.
    assert_eq!(next.refreshed_at, existing.refreshed_at);
    assert!(model_cache_is_stale(&next, Some(90_000), 24));
    assert_eq!(next.models, existing.models);
    assert_eq!(
        next.diagnostics,
        ["zai-coding model refresh failed: status 503".to_string()]
    );
}

#[test]
fn model_refresh_all_failures_without_cache_still_writes_diagnostics() {
    let mut report = ModelRefreshReport::default();
    report.record("openai", None, Err(anyhow!("network down")));
    let next = next_model_cache(None, &report, Some(200)).unwrap();
    assert_eq!(next.refreshed_at, 0);
    assert!(next.models.is_empty());
    assert_eq!(
        next.diagnostics,
        ["openai model refresh failed: network down".to_string()]
    );
}

#[test]
fn model_refresh_without_auth_writes_nothing() {
    let report = ModelRefreshReport::default();
    assert!(next_model_cache(None, &report, Some(200)).is_none());
    assert!(next_model_cache(None, &report, None).is_none());
}

#[test]
fn tests_never_refresh_models_in_the_background() {
    let settings = Settings::default();
    assert!(background_refresh_allowed(&settings, false, false));
    assert!(!background_refresh_allowed(&settings, false, true));
    assert!(!background_refresh_allowed(&settings, true, false));
}

#[test]
fn claude_code_version_from_refresh_survives_merge() {
    let existing = refresh_test_cache(vec![]);
    let report = ModelRefreshReport {
        claude_code_version: Some("2.1.280".to_string()),
        ..Default::default()
    };
    let next = next_model_cache(Some(&existing), &report, Some(200)).unwrap();
    assert_eq!(next.claude_code_version.as_deref(), Some("2.1.280"));

    // Version-check-only refresh keeps the previously cached version.
    let kept = refresh_test_cache(vec![]);
    let version_only = ModelRefreshReport {
        claude_code_version: Some("9.9.9".to_string()),
        ..Default::default()
    };
    let next = next_model_cache(Some(&kept), &version_only, Some(200)).unwrap();
    assert_eq!(next.claude_code_version.as_deref(), Some("9.9.9"));
}

#[test]
fn model_cache_reset_removes_file_and_tolerates_missing_file() {
    let root = std::env::temp_dir().join(format!(
        "noel-cli-model-cache-reset-{}",
        unique_temp_suffix()
    ));
    fs::create_dir_all(&root).expect("create temp dir");
    let path = root.join("model-cache.json");
    assert_eq!(
        reset_model_cache(&path).unwrap(),
        "model cache already clear"
    );
    write_model_cache(&path, &refresh_test_cache(vec![])).expect("write cache");
    assert_eq!(reset_model_cache(&path).unwrap(), "model cache cleared");
    assert!(!path.exists());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn claude_code_version_validation_rejects_header_unsafe_values() {
    assert!(is_valid_claude_code_version("2.1.280"));
    assert!(is_valid_claude_code_version("2.2.0-beta.1+build"));
    assert!(!is_valid_claude_code_version(""));
    assert!(!is_valid_claude_code_version("2.1.280\r\nx-injected: yes"));
    assert!(!is_valid_claude_code_version(&"1".repeat(33)));
}

#[test]
fn numstat_parses_text_and_binary_entries() {
    let stats = parse_numstat("3\t1\tsrc/a.rs\n-\t-\tassets/logo.png\n");
    assert_eq!(stats["src/a.rs"], (Some(3), Some(1)));
    assert_eq!(stats["assets/logo.png"], (None, None));
    assert!(parse_numstat("").is_empty());
}

#[test]
fn thinking_previews_wrap_and_switch_between_streaming_tail_and_finished_head() {
    let entry = TuiEntry {
        kind: TuiEntryKind::Thinking,
        text: "first thought\nsecond thought\nthird thought\nlast thought".into(),
    };
    let palette = ThemePalette::default();
    let head = preview_entry(&entry, 0, 80, &palette, &PreviewState::default());
    assert_eq!(head.len(), 4); // two content rows, one hint, spacer
    assert!(head
        .iter()
        .take(2)
        .map(line_text)
        .collect::<String>()
        .contains("thinking"));
    assert!(head
        .iter()
        .map(line_text)
        .collect::<String>()
        .contains("ctrl+o"));
    let tail = preview_entry(
        &entry,
        0,
        80,
        &palette,
        &PreviewState {
            live_thinking: Some(0),
            expanded_from: None,
        },
    );
    assert!(tail
        .iter()
        .map(line_text)
        .collect::<String>()
        .contains("last thought"));
    assert!(!tail
        .iter()
        .map(line_text)
        .collect::<String>()
        .contains("ctrl+o"));
    assert_eq!(tail.len(), 3);
    let expanded = preview_entry(
        &entry,
        0,
        8,
        &palette,
        &PreviewState {
            expanded_from: Some(0),
            live_thinking: None,
        },
    );
    assert!(expanded.len() > tail.len());
    assert!(!expanded
        .iter()
        .map(line_text)
        .collect::<String>()
        .contains("more lines"));
    assert_eq!(
        entry.text,
        "first thought\nsecond thought\nthird thought\nlast thought"
    );
}

#[test]
fn collapsed_blocks_never_exceed_three_rows_even_on_narrow_screens() {
    let palette = ThemePalette::default();
    for kind in [TuiEntryKind::Thinking, TuiEntryKind::Tool] {
        let entry = TuiEntry {
            text: if kind == TuiEntryKind::Tool {
                format!(
                    "completed bash\n$ {}\n{}",
                    "long command 👩‍💻界 ".repeat(30),
                    "output 界e\u{301}\t".repeat(100)
                )
            } else {
                "thought 界e\u{301}👩‍💻\t".repeat(100)
            },
            kind,
        };
        for width in [1, 2, 4, 8, 20, 80] {
            let rows = preview_entry(&entry, 0, width, &palette, &PreviewState::default());
            assert!(rows.len() <= 4, "three content rows plus spacer: {width}");
            assert!(rows.iter().all(|row| row.width() <= width), "width {width}");
            let expanded = preview_entry(
                &entry,
                0,
                width,
                &palette,
                &PreviewState {
                    expanded_from: Some(0),
                    live_thinking: None,
                },
            );
            assert!(expanded.len() > rows.len());
        }
    }
}

#[test]
fn compact_command_header_keeps_line_count_and_full_command_is_expandable() {
    let command = format!(
        "git status --short; echo ---; git diff --stat {} end-of-command",
        "README.md crates/noel-tui/src/lib.rs ".repeat(20)
    );
    let entry = TuiEntry {
        kind: TuiEntryKind::Tool,
        text: format!("completed bash\n$ {command}\nfirst\nsecond\nthird\nfinal result"),
    };
    let palette = ThemePalette::default();
    let collapsed = preview_entry(&entry, 0, 80, &palette, &PreviewState::default());
    assert_eq!(collapsed.len(), 3);
    let header = line_text(&collapsed[0]);
    assert!(header.starts_with("● Ran a command · $ git status --short"));
    assert!(header.ends_with("… · 3 more lines"));
    assert_eq!(line_text(&collapsed[1]), "  … final result");
    let expanded = preview_entry(
        &entry,
        0,
        80,
        &palette,
        &PreviewState {
            expanded_from: Some(0),
            live_thinking: None,
        },
    );
    assert!(expanded
        .iter()
        .map(line_text)
        .collect::<String>()
        .contains("end-of-command"));
    assert!(expanded
        .iter()
        .map(line_text)
        .collect::<String>()
        .contains("first"));
    // A watermark inside a folded range must not reconstruct hidden output.
    let pending = pending_entry_rows(
        &entry,
        0,
        80,
        &palette,
        &PreviewState::default(),
        EntryScrollback {
            position: TranscriptPosition { line: 2, byte: 1 },
            complete: false,
        },
    );
    assert_eq!(line_text(&pending[0].line), "  … final result");
}

#[test]
fn short_tool_results_stay_visible_without_an_expansion_count() {
    let entry = TuiEntry {
        kind: TuiEntryKind::Tool,
        text: "completed todo\n\none\ntwo".into(),
    };
    let rows = preview_entry(
        &entry,
        0,
        80,
        &ThemePalette::default(),
        &PreviewState::default(),
    );
    assert_eq!(rows.len(), 4);
    assert_eq!(line_text(&rows[0]), "● Used Todo");
    assert_eq!(line_text(&rows[1]), "  one");
    assert_eq!(line_text(&rows[2]), "  two");
    assert!(!rows
        .iter()
        .map(line_text)
        .collect::<String>()
        .contains("more lines"));
}

#[test]
fn new_prompts_and_clear_restore_collapsed_previews() {
    let mut app = TuiApp::default();
    app.push(TuiEntryKind::User, "first prompt");
    app.previews_expanded = true;
    app.push(TuiEntryKind::Tool, "first result");
    app.push(TuiEntryKind::System, "reloaded");
    assert!(app.previews_expanded);
    app.push(TuiEntryKind::User, "next prompt");
    assert!(!app.previews_expanded);
    app.previews_expanded = true;
    app.clear_visible();
    assert!(!app.previews_expanded);
}

#[test]
fn tool_label_styles_are_segmented_and_previews_keep_final_output_line() {
    let palette = ThemePalette::default();
    let entry = TuiEntry {
        kind: TuiEntryKind::Tool,
        text: "completed read\nsrc/main.rs\nfirst\nsecond\nthird\nfourth\nfifth".into(),
    };
    let lines = render_entry_lines(std::slice::from_ref(&entry), &palette);
    let spans = &lines[0].spans;
    assert_eq!(spans[0].style.fg, Some(palette.success));
    assert_eq!(spans[1].content, "Used ");
    assert_eq!(spans[1].style.fg, None);
    assert_eq!(spans[2].content, "Read");
    assert_eq!(spans[2].style.fg, Some(palette.accent));
    assert!(spans[2].style.add_modifier.contains(Modifier::BOLD));
    assert_eq!(spans[3].style.fg, Some(palette.muted));
    let preview = preview_entry(&entry, 0, 100, &palette, &PreviewState::default());
    let text = preview.iter().map(line_text).collect::<String>();
    assert!(!text.contains("first"));
    assert!(text.contains("4 more lines"));
    assert!(text.contains("… fifth"));
    let mut failed = Vec::new();
    push_tool_lines(
        &mut failed,
        "failed bash\n$ false\n(no output)\n\nCommand exited with code 1",
        &palette,
    );
    assert_eq!(failed[0].spans[0].content, "✗ ");
    assert_eq!(failed[0].spans[0].style.fg, Some(palette.error));
}

#[test]
fn queue_pane_is_bounded_and_keeps_one_row_per_message() {
    use ratatui::backend::TestBackend;
    let app = TuiApp {
        queued_messages: vec!["queued\nmessage".into(); 7],
        turn_running: true,
        ..Default::default()
    };
    assert_eq!(queue_panel_height(&app), 6);
    let mut terminal = Terminal::new(TestBackend::new(80, 6)).unwrap();
    terminal
        .draw(|frame| draw_queue_panel(frame, frame.area(), &app, &ThemePalette::default()))
        .unwrap();
    let buffer = terminal.backend().buffer();
    let text = buffer
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert_eq!(text.matches("queued message").count(), 4);
    assert!(text.contains("+3 more"));
    assert!(text.contains("ctrl+s"));
}

#[test]
fn queue_hint_distinguishes_prose_only_turns_from_tool_rounds() {
    use ratatui::backend::TestBackend;
    let render = |saw_tools: bool| {
        let app = TuiApp {
            queued_messages: vec!["queued".into()],
            turn_running: true,
            turn_saw_tools: saw_tools,
            ..Default::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(80, 6)).unwrap();
        terminal
            .draw(|frame| draw_queue_panel(frame, frame.area(), &app, &ThemePalette::default()))
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
    };
    // With tool rounds in the turn, steering lands at the next boundary.
    assert!(render(true).contains("ctrl+s to steer at next boundary"));
    // A prose-only response has no boundary: promise post-response
    // delivery instead.
    assert!(render(false).contains("ctrl+s queues after this response"));
}

#[test]
fn harness_frame_matches_todo_queue_and_boxed_prompt_example() {
    use ratatui::backend::TestBackend;
    let config = minimal_test_config();
    let mut app = TuiApp {
        todos: (0..6)
            .map(|index| TodoItem {
                content: format!("task {index}"),
                status: if index == 0 {
                    TodoStatus::InProgress
                } else {
                    TodoStatus::Pending
                },
            })
            .collect(),
        queued_messages: vec!["make the harness standard".into()],
        turn_running: true,
        turn_saw_tools: true,
        show_hardware_cursor: true,
        ..Default::default()
    };
    let mut terminal = Terminal::new(TestBackend::new(80, 30)).unwrap();
    terminal
        .draw(|frame| draw_tui(frame, &app, &config))
        .unwrap();
    let rows = buffer_rows(terminal.backend().buffer());
    let expected = [
        "─".repeat(80),
        "  Todo".into(),
        "  ● task 0".into(),
        "  ○ task 1".into(),
        "  ○ task 2".into(),
        "  ○ task 3".into(),
        "  ○ task 4".into(),
        "  … +1 more (1 pending) · ctrl+t to expand".into(),
        "─".repeat(80),
        "  ❯ make the harness standard".into(),
        "  ↑ to edit · ctrl+s to steer at next boundary".into(),
        format!("╭{}╮", "─".repeat(78)),
        format!("│ > {}│", " ".repeat(75)),
        format!("╰{}╯", "─".repeat(78)),
    ];
    assert_eq!(
        rows[15..29]
            .iter()
            .map(|row| row.trim_end())
            .collect::<Vec<_>>(),
        expected.iter().map(String::as_str).collect::<Vec<_>>()
    );
    terminal.backend_mut().assert_cursor_position((4, 27));
    // The queue owns the shared separator, with no duplicate rule or
    // blank row between the hints and editor. Both sections can vanish.
    app.todos.clear();
    app.queued_messages.clear();
    terminal
        .draw(|frame| draw_tui(frame, &app, &config))
        .unwrap();
    let rows = buffer_rows(terminal.backend().buffer());
    assert!(rows[..26].iter().all(|row| row.trim().is_empty()));
    assert!(rows[27].starts_with("│ > "));
}

#[test]
fn height_limited_todos_report_actual_hidden_counts_and_keep_active_task() {
    use ratatui::backend::TestBackend;
    let mut app = TuiApp {
        todos: (0..8)
            .map(|index| TodoItem {
                content: format!("task {index}"),
                status: if index == 6 {
                    TodoStatus::InProgress
                } else {
                    TodoStatus::Pending
                },
            })
            .collect(),
        ..Default::default()
    };
    let mut terminal = Terminal::new(TestBackend::new(70, 7)).unwrap();
    terminal
        .draw(|frame| draw_todo_panel(frame, frame.area(), &app, &ThemePalette::default()))
        .unwrap();
    let text = buffer_rows(terminal.backend().buffer()).join("\n");
    assert!(text.contains("● task 6"));
    assert!(text.contains("+5 more (5 pending) · ctrl+t to expand"));
    assert!(!text.contains("task 2"));
    app.todos_expanded = true;
    terminal
        .draw(|frame| draw_todo_panel(frame, frame.area(), &app, &ThemePalette::default()))
        .unwrap();
    let text = buffer_rows(terminal.backend().buffer()).join("\n");
    assert!(text.contains("+5 more (4 pending, 1 active) · ctrl+t to collapse"));
    assert!(text.contains("task 2"));
    assert!(!text.contains("task 3"));
}

#[test]
fn queue_overflow_shows_newest_editable_message_and_cell_width_truncation() {
    use ratatui::backend::TestBackend;
    let mut app = TuiApp {
        queued_messages: (0..7).map(|index| format!("queued {index}")).collect(),
        ..Default::default()
    };
    let mut terminal = Terminal::new(TestBackend::new(80, 4)).unwrap();
    terminal
        .draw(|frame| draw_queue_panel(frame, frame.area(), &app, &ThemePalette::default()))
        .unwrap();
    let text = buffer_rows(terminal.backend().buffer()).join("\n");
    assert!(text.contains("❯ queued 5"));
    assert!(text.contains("❯ queued 6"));
    assert!(!text.contains("queued 4"));
    assert!(text.contains("+5 more · ↑ to edit · ctrl+enter or Enter sends now"));
    app.queued_messages = vec!["界界界界界界界界界界".into()];
    let mut terminal = Terminal::new(TestBackend::new(16, 3)).unwrap();
    terminal
        .draw(|frame| draw_queue_panel(frame, frame.area(), &app, &ThemePalette::default()))
        .unwrap();
    assert_eq!(
        buffer_rows(terminal.backend().buffer())[1].trim_end(),
        "  ❯ 界界界界界…"
    );
    assert_eq!(truncate_width("e\u{301}xy", 2), "e\u{301}…");
}

#[test]
fn tiny_frames_and_multiline_drafts_never_place_cursor_on_editor_border() {
    use ratatui::backend::TestBackend;
    let config = minimal_test_config();
    let mut app = TuiApp {
        show_hardware_cursor: true,
        queued_messages: vec!["pending".into()],
        todos: vec![TodoItem {
            content: "active".into(),
            status: TodoStatus::InProgress,
        }],
        ..Default::default()
    };
    app.set_input("/help");
    for width in [1, 4, 8, 20, 80] {
        for height in 1..=12 {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| draw_tui(frame, &app, &config))
                .unwrap();
        }
    }
    app.set_input("first\nsecond\nthird");
    let mut terminal = Terminal::new(TestBackend::new(20, 5)).unwrap();
    terminal
        .draw(|frame| {
            draw_input(frame, frame.area(), &app, &ThemePalette::default());
            set_tui_cursor(frame, frame.area(), &app);
        })
        .unwrap();
    let rows = buffer_rows(terminal.backend().buffer());
    assert!(rows[1].starts_with("│ > first"));
    assert!(rows[2].starts_with("│   second"));
    assert!(rows[3].starts_with("│   third"));
    terminal.backend_mut().assert_cursor_position((9, 3));
    app.overlay = Overlay::Multiline(Vec::new());
    app.clear_input();
    assert_eq!(input_desired_height(&app, 40), 4);
    let mut terminal = Terminal::new(TestBackend::new(40, 4)).unwrap();
    terminal
        .draw(|frame| draw_input(frame, frame.area(), &app, &ThemePalette::default()))
        .unwrap();
    let rows = buffer_rows(terminal.backend().buffer());
    assert!(rows[1].starts_with("│ multi> "));
    assert!(rows[2].contains("submit with . on its own line"));
}

#[test]
fn todo_panel_height_collapses_and_expands() {
    let mut app = TuiApp::default();
    assert_eq!(todo_panel_height(&app), 0);
    app.todos = (0..8)
        .map(|index| TodoItem {
            content: format!("task {index}"),
            status: TodoStatus::Pending,
        })
        .collect();
    assert_eq!(todo_panel_height(&app), 9);
    app.todos_expanded = true;
    assert_eq!(todo_panel_height(&app), 12);
    app.todos = (0..3)
        .map(|index| TodoItem {
            content: format!("task {index}"),
            status: TodoStatus::Pending,
        })
        .collect();
    assert_eq!(todo_panel_height(&app), 7);
}

#[test]
fn diff_detail_renders_untracked_file_as_additions() {
    let cwd = std::env::temp_dir().join(format!(
        "noel-cli-diff-detail-test-{}",
        unique_temp_suffix()
    ));
    fs::create_dir_all(&cwd).expect("create temp dir");
    fs::write(cwd.join("new.rs"), "fn main() {}\n").expect("write file");
    let entry = DiffFileEntry {
        path: "new.rs".to_string(),
        added: None,
        removed: None,
        untracked: true,
    };

    let detail = build_diff_detail(&cwd, &entry);

    assert_eq!(detail.lines, ["+fn main() {}".to_string()]);
    let _ = fs::remove_dir_all(cwd);
}

fn test_model(provider: &str, id: &str) -> ModelDefinition {
    ModelDefinition {
        context_window: None,
        provider: provider.to_string(),
        id: id.to_string(),
        name: None,
        api: ConfigProviderApi::default(),
        base_url: None,
        ..ModelDefinition::default()
    }
}

#[test]
fn cli_contract_covers_upstream_ts_options_and_commands() {
    let fixture = serde_json::from_str::<serde_json::Value>(include_str!(
        "../../../tests/fixtures/ts-parity/cli-contract.json"
    ))
    .expect("parse cli contract fixture");
    let rust_options = rust_cli_option_contract();
    let rust_commands = rust_cli_command_contract();

    for option in fixture["options"].as_array().expect("fixture options") {
        let long = option["long"].as_str().expect("option long");
        assert!(
            rust_options.contains(long),
            "missing upstream CLI option --{long}"
        );
        if let Some(short) = option["short"].as_str() {
            assert!(
                rust_options.contains(short),
                "missing upstream CLI option -{short}"
            );
        }
    }

    let fixture_commands = fixture["commands"]
        .as_array()
        .expect("fixture commands")
        .iter()
        .map(|command| command.as_str().expect("command"))
        .collect::<BTreeSet<_>>();
    let register = include_str!("../../../docs/rust-rewrite/non-parity-register.md");
    for (command, heading) in UNSUPPORTED_UPSTREAM_CLI_COMMANDS {
        assert!(
            fixture_commands.contains(command),
            "stale exception: upstream no longer has CLI command {command}"
        );
        assert!(
            !rust_commands.contains(*command),
            "{command} is implemented; drop its exception"
        );
        assert!(
            register.contains(heading),
            "non-parity register must record CLI command {command} under {heading:?}"
        );
    }
    for command in fixture_commands {
        if UNSUPPORTED_UPSTREAM_CLI_COMMANDS
            .iter()
            .any(|(unsupported, _)| *unsupported == command)
        {
            continue;
        }
        assert!(
            rust_commands.contains(command),
            "missing upstream CLI command {command}"
        );
    }
}

/// Upstream CLI commands this port deliberately leaves out, each with
/// its docs/rust-rewrite/non-parity-register.md heading.
const UNSUPPORTED_UPSTREAM_CLI_COMMANDS: &[(&str, &str)] = &[];

#[test]
fn tui_mode_flag_selects_owned_or_native_screen() {
    let parse = |args: &[&str]| Cli::try_parse_from([&["noel"][..], args].concat());
    let owned = |args: &[&str]| parse(args).expect("parse cli").owned_screen();
    assert!(owned(&[]));
    assert!(owned(&["--tui-mode", "fullscreen"]));
    assert!(!owned(&["--tui-mode", "regular"]));
    assert!(!owned(&["--no-alt-screen"]));
    assert!(parse(&["--tui-mode", "fullscreen", "--no-alt-screen"]).is_err());
    assert!(parse(&["--tui-mode", "split"]).is_err());
    assert!(parse(&["--no-mcp"]).expect("parse cli").no_mcp);
}

#[test]
fn use_theme_conflicts_with_other_theme_flags_and_applies_for_one_run() {
    let parse = |args: &[&str]| Cli::try_parse_from([&["noel"][..], args].concat());
    for args in [
        &["--use-theme", "light", "--no-themes"][..],
        &["--use-theme", "light", "--theme", "dark"],
    ] {
        let error = parse(args).expect_err("conflicting theme flags");
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::ArgumentConflict,
            "{args:?}"
        );
    }

    let mut config = minimal_test_config();
    let no_themes = parse(&["--theme", "missing", "--no-themes"]).expect("parse cli");
    apply_cli_overrides(&no_themes, Path::new("."), &mut config).expect("no themes");
    assert_eq!(config.settings.theme, None);
    let use_theme = parse(&["--use-theme", "light"]).expect("parse cli");
    apply_cli_overrides(&use_theme, Path::new("."), &mut config).expect("use theme");
    assert_eq!(config.settings.theme.as_deref(), Some("light"));
    // A light/dark pair is upstream's auto setting: it saves the pair
    // and resolves its half from the terminal's appearance.
    let pair = parse(&["--use-theme", "light/dark"]).expect("parse cli");
    apply_cli_overrides(&pair, Path::new("."), &mut config).expect("pair");
    assert_eq!(config.settings.theme.as_deref(), Some("light/dark"));
    let resolved = terminal_theme(&config).expect("pair resolves");
    // This test terminal reports nothing: the hint is dark.
    assert_eq!(resolved.name, "dark");
    let bad_pair = parse(&["--use-theme", "light/missing"]).expect("parse cli");
    assert!(apply_cli_overrides(&bad_pair, Path::new("."), &mut config).is_err());
}

fn rust_cli_option_contract() -> BTreeSet<String> {
    let mut options = BTreeSet::new();
    for argument in Cli::command().get_arguments() {
        if let Some(long) = argument.get_long() {
            options.insert(long.to_string());
        }
        if let Some(short) = argument.get_short() {
            options.insert(short.to_string());
        }
    }
    for option in [
        "help", "h", "version", "v", "nt", "nbt", "ne", "ns", "np", "nc", "xt", "na",
    ] {
        options.insert(option.to_string());
    }
    options
}

fn rust_cli_command_contract() -> BTreeSet<String> {
    [
        "auth",
        "config",
        "install",
        "list",
        "mcp",
        "remove",
        "uninstall",
        "update",
    ]
    .into_iter()
    .map(ToString::to_string)
    .collect()
}

fn test_resource(name: &str) -> ResourceFile {
    ResourceFile {
        name: name.to_string(),
        path: PathBuf::from(name),
        content: String::new(),
    }
}

fn test_source(path: &Path) -> SourceInfo {
    SourceInfo {
        path: path.to_path_buf(),
        source: "auto".into(),
        scope: SourceScope::User,
        from_package: false,
        base_dir: None,
    }
}

/// A user skill at `/skills/<name>/SKILL.md`.
fn test_skill(name: &str, description: &str) -> Skill {
    let base_dir = PathBuf::from(format!("/skills/{name}"));
    let file_path = base_dir.join("SKILL.md");
    Skill {
        name: name.to_string(),
        description: description.to_string(),
        source_info: test_source(&file_path),
        file_path,
        base_dir,
        disable_model_invocation: false,
    }
}

/// A user prompt template at `/prompts/<name>.md`.
fn test_template(
    name: &str,
    description: &str,
    argument_hint: Option<&str>,
    content: &str,
) -> PromptTemplate {
    let file_path = PathBuf::from(format!("/prompts/{name}.md"));
    PromptTemplate {
        name: name.to_string(),
        description: description.to_string(),
        argument_hint: argument_hint.map(str::to_string),
        content: content.to_string(),
        source_info: test_source(&file_path),
        file_path,
    }
}

fn png_bytes(width: u32, height: u32) -> Vec<u8> {
    let image = image::DynamicImage::ImageRgba8(image::RgbaImage::new(width, height));
    let mut output = Cursor::new(Vec::new());
    image
        .write_to(&mut output, image::ImageFormat::Png)
        .expect("encode png");
    output.into_inner()
}

/// #108: the branch navigator folds a branch and moves to its parent and
/// child, shows named branches only under its filter, and keeps its tree
/// across the worker-to-client JSON.
#[test]
fn the_branch_navigator_folds_and_filters() {
    let items = ["root", "child", "grandchild", "other"]
        .into_iter()
        .map(|value| SelectorItem {
            label: value.to_owned(),
            value: value.to_owned(),
            active: false,
        })
        .collect();
    let mut state = TuiSelectorState::new("branches", Selector::new("branches", items), "", None);
    state.set_tree(BTreeMap::from([
        ("root".to_owned(), (None, true)),
        ("child".to_owned(), (Some("root".to_owned()), false)),
        ("grandchild".to_owned(), (Some("child".to_owned()), true)),
        ("other".to_owned(), (None, false)),
    ]));
    let shown = |state: &TuiSelectorState| -> Vec<String> {
        state
            .filtered_indices
            .iter()
            .map(|index| state.items[*index].label.clone())
            .collect()
    };
    assert_eq!(shown(&state), ["root", "child", "grandchild", "other"]);

    // Fold the root: its branch hides, and the row says it is folded.
    state.fold_or_up();
    assert_eq!(shown(&state), ["root [+]", "other"]);
    // Unfold, then down to the child and up to the root again.
    state.unfold_or_down();
    state.unfold_or_down();
    assert_eq!(state.selected_item().unwrap().value, "child");
    state.fold_or_up();
    assert_eq!(shown(&state), ["root", "child [+]", "other"]);
    // Up from a folded child goes to its parent; the child stays folded.
    state.fold_or_up();
    assert_eq!(state.selected_item().unwrap().value, "root");
    assert_eq!(shown(&state), ["root", "child [+]", "other"]);
    state.unfold_or_down();
    state.unfold_or_down();
    assert_eq!(shown(&state), ["root", "child", "grandchild", "other"]);

    // Named branches only, then all again.
    state.set_tree_filter(TreeFilter::Named);
    assert_eq!(shown(&state), ["root", "grandchild"]);
    assert_eq!(state.title, "branches · named only");
    state.set_tree_filter(TreeFilter::All);
    assert_eq!(state.title, "branches");
    assert_eq!(
        TreeFilter::from_setting(Some("labeled-only")),
        TreeFilter::Named
    );
    assert_eq!(TreeFilter::Named.setting(), "labeled-only");

    // The tree survives the JSON a worker sends.
    let again = TuiSelectorState::from_json(&state.to_json()).unwrap();
    assert_eq!(again.tree.len(), 4);
    assert_eq!(again.tree["grandchild"].parent.as_deref(), Some("child"));
    assert!(again.tree["grandchild"].named);
}

#[test]
fn slash_list_renders_upstream_select_rows() {
    let config = LoadedConfig {
        paths: test_config_paths(),
        settings: Settings::default(),
        auth: AuthData::default(),
        models: Vec::new(),
        model_scope: Vec::new(),
        image_models: Vec::new(),
        keybindings: Vec::new(),
        context_files: Vec::new(),
        extensions: Vec::new(),
        skills: Vec::new(),
        agents: Vec::new(),
        mcp_servers: Vec::new(),
        prompt_templates: Vec::new(),
        themes: Vec::new(),
        diagnostics: Vec::new(),
        system_prompt: None,
        append_system_prompt: Vec::new(),
    };
    let palette = TerminalTheme::builtin("system").unwrap().palette;
    let mut app = TuiApp::default();
    app.set_input("/c");
    app.slash_selection = Some(0);

    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 3)).unwrap();
    terminal
        .draw(|frame| draw_slash_matches(frame, frame.area(), &app, &config, &palette))
        .unwrap();
    let rendered: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol().to_string())
        .collect::<Vec<_>>()
        .chunks(60)
        .map(|row| row.join(""))
        .collect::<Vec<_>>()
        .join("\n");
    // The selected first row: arrow prefix, command, muted description.
    assert!(rendered.contains("→ /changelog"), "{rendered}");
    assert!(rendered.contains("show changelog entries"), "{rendered}");
    // A later row is unselected: no arrow.
    assert!(rendered.contains("  /clear"), "{rendered}");

    // Moving the selection past the window appends the scroll line.
    app.slash_selection = Some(3);
    terminal
        .draw(|frame| draw_slash_matches(frame, frame.area(), &app, &config, &palette))
        .unwrap();
    let scrolled: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol().to_string())
        .collect::<Vec<_>>()
        .chunks(60)
        .map(|row| row.join(""))
        .collect::<Vec<_>>()
        .join("\n");
    if !scrolled.contains("(4/") {
        // If /c matches fewer than 4, the window already holds the row
        // and no scroll line is needed; the arrow proves the selection.
        assert!(scrolled.contains("→ /compact"), "{scrolled}");
    }

    // A prefix nothing matches says so instead of showing nothing.
    app.set_input("/zzz");
    terminal
        .draw(|frame| draw_slash_matches(frame, frame.area(), &app, &config, &palette))
        .unwrap();
    let none: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol().to_string())
        .collect::<Vec<_>>()
        .chunks(60)
        .map(|row| row.join(""))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(none.contains("No matching commands"), "{none}");
}
