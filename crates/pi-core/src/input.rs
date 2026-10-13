//! What a user's input becomes before it is a message. Upstream's session
//! expands `/skill:name` and `/template` commands in `prompt`, `steer`, and
//! `followUp` (`core/agent-session.ts`); here every mode applies
//! [`expand_input`] where input enters it, and the engine takes the text it
//! is given. `skills.json` holds upstream's expansions, which the tests
//! below compare.

use std::fs;

use pi_config::{expand_prompt_template, js_trim, strip_frontmatter, LoadedConfig, Skill};

use crate::parse_tool_command;

/// `text` as a prompt, steering, or a follow-up runs it:
///
/// - A slash-tool prompt such as `/read path`, which only pi has, stays as
///   it is, whatever templates are loaded.
/// - `/skill:name arguments` becomes the skill's file, followed by the
///   arguments.
/// - `/name arguments` becomes the prompt template `name`, with the
///   arguments substituted.
/// - Anything else is text for the model, a command that names no skill or
///   template included.
pub fn expand_input(config: &LoadedConfig, text: &str) -> String {
    if !matches!(parse_tool_command(text), Ok(None)) {
        return text.to_string();
    }
    let text = expand_skill_command(text, &config.skills);
    expand_prompt_template(&text, &config.prompt_templates)
}

/// Expands `/skill:name arguments`: the skill's file without its
/// frontmatter, in upstream's `<skill>` block, then the arguments. The text
/// is returned as it is when it names no loaded skill or the skill's file
/// can no longer be read.
fn expand_skill_command(text: &str, skills: &[Skill]) -> String {
    let Some(command) = text.strip_prefix("/skill:") else {
        return text.to_string();
    };
    let (name, arguments) = command.split_once(' ').unwrap_or((command, ""));
    let skill = skills.iter().find(|skill| skill.name == name);
    let loaded = skill.and_then(|skill| {
        let content = fs::read(&skill.file_path).ok()?;
        let body = strip_frontmatter(&String::from_utf8_lossy(&content)).ok()?;
        Some((skill, body))
    });
    let Some((skill, body)) = loaded else {
        return text.to_string();
    };
    let block = format!(
        "<skill name=\"{}\" location=\"{}\">\nReferences are relative to {}.\n\n{}\n</skill>",
        skill.name,
        skill.file_path.display(),
        skill.base_dir.display(),
        js_trim(&body)
    );
    match js_trim(arguments) {
        "" => block,
        arguments => format!("{block}\n\n{arguments}"),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use pi_config::{load_config_with_resources, ConfigPaths, ResourceOptions};
    use serde_json::Value;

    use super::*;
    use crate::new_session_id;

    /// A config loaded from `files`, written below a new directory, whose
    /// agent dir is `agent` and working directory `repo`.
    fn config_with(files: &[(&str, &str)]) -> (PathBuf, LoadedConfig) {
        let root = std::env::temp_dir().join(format!("pi-input-{}", new_session_id()));
        let agent_dir = root.join("agent");
        let cwd = root.join("repo");
        for dir in [&agent_dir, &cwd, &root.join("home")] {
            fs::create_dir_all(dir).unwrap();
        }
        for (path, content) in files {
            fs::create_dir_all(root.join(path).parent().unwrap()).unwrap();
            fs::write(root.join(path), content.replace("<bom>", "\u{feff}")).unwrap();
        }
        let paths = ConfigPaths {
            settings_path: agent_dir.join("settings.json"),
            project_settings_path: cwd.join(".pi/settings.json"),
            session_dir: agent_dir.join("sessions"),
            auth_path: agent_dir.join("auth.json"),
            models_path: agent_dir.join("models.json"),
            model_cache_path: agent_dir.join("model-cache.json"),
            keybindings_path: agent_dir.join("keybindings.json"),
            agent_dir,
            cwd,
        };
        let options = ResourceOptions {
            home_dir: Some(root.join("home")),
            ..ResourceOptions::default()
        };
        let config = load_config_with_resources(paths, true, &options).expect("load config");
        (root, config)
    }

    /// For every expansion case of `skills.json`, the text upstream's session
    /// queues for the same input: skill blocks, template arguments, and the
    /// input itself when it names neither.
    #[test]
    fn input_expansion_matches_ts_session() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/ts-parity/skills.json"
        ))
        .expect("parse TS parity fixture");
        let expansion = &fixture["expansion"];
        let files = expansion["files"].as_object().unwrap().iter();
        let files = files
            .map(|(path, content)| (path.as_str(), content.as_str().unwrap()))
            .collect::<Vec<_>>();
        let (root, config) = config_with(&files);
        assert_eq!(config.diagnostics, Vec::<String>::new());
        // Upstream's session met these changes after it had loaded the files.
        for path in expansion["removed"].as_array().unwrap() {
            fs::remove_file(root.join(path.as_str().unwrap())).unwrap();
        }
        for (path, content) in expansion["rewritten"].as_object().unwrap() {
            fs::write(root.join(path), content.as_str().unwrap()).unwrap();
        }

        let tmp = root.to_string_lossy();
        let cases = expansion["cases"].as_array().unwrap();
        assert!(cases.len() > 40);
        let expands = |input: &str| expand_input(&config, input) != input;
        for case in cases {
            let input = case["input"].as_str().unwrap();
            let output = case["output"].as_str().unwrap().replace("<tmp>", &tmp);
            assert_eq!(expand_input(&config, input), output, "{input:?}");
            assert_eq!(expands(input), input != case["output"], "{input:?}");
        }
        let _ = fs::remove_dir_all(root);
    }

    /// pi's slash-tool prompts keep their meaning: a template named like one
    /// is not reached, and a tool command is never rewritten.
    #[test]
    fn slash_tool_prompts_come_before_templates() {
        let (root, config) = config_with(&[
            ("agent/prompts/read.md", "Read $1 aloud.\n"),
            ("agent/prompts/bash.md", "Never run.\n"),
            ("agent/prompts/help.md", "Explain $@.\n"),
            ("agent/prompts/ls.md", "List.\n"),
        ]);
        for tool_prompt in [
            "/read notes.txt",
            "/bash ls",
            "/read",
            " /read notes.txt",
            "/ls",
        ] {
            assert_eq!(expand_input(&config, tool_prompt), tool_prompt);
        }
        // Not a slash-tool prompt: `/reader` is no tool, and `/help` is a
        // command only in the TUI, which handles it before input expands.
        assert_eq!(expand_input(&config, "/help tools"), "Explain tools.\n");
        assert_eq!(expand_input(&config, "/reader x"), "/reader x");
        let _ = fs::remove_dir_all(root);
    }

    /// `--skill <directory>` with `--no-skills`, as a supervisor starts pi.
    #[test]
    fn a_skill_given_by_path_expands_with_its_location() {
        let (root, base) = config_with(&[(
            "given/review/SKILL.md",
            "---\ndescription: Review code.\n---\nCheck `rules.md`.\n",
        )]);
        assert!(base.skills.is_empty());
        let skill_dir = root.join("given/review");
        let options = ResourceOptions {
            skill_paths: vec![skill_dir.clone()],
            no_skills: true,
            ..ResourceOptions::default()
        };
        let config = load_config_with_resources(base.paths, true, &options).unwrap();
        assert_eq!(
            expand_input(&config, "/skill:review the parser"),
            format!(
                "<skill name=\"review\" location=\"{}\">\nReferences are relative to {}.\n\nCheck `rules.md`.\n</skill>\n\nthe parser",
                skill_dir.join("SKILL.md").display(),
                skill_dir.display()
            )
        );
        let _ = fs::remove_dir_all(root);
    }
}
