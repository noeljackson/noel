//! The system prompt every model request starts with: upstream's
//! `buildSystemPrompt` (`packages/coding-agent/src/core/system-prompt.ts`),
//! section by section and word for word. `system-prompt.json` holds
//! upstream's text for fixed inputs, and the tests below compare against it.
//!
//! The prompt is a preamble followed by tagged sections, joined by blank
//! lines: `tools`, `rules`, `addendum` (appended text), `project_context`
//! (context files), `skills`, and `cwd`. A custom prompt replaces the
//! preamble, `tools`, and `rules`; the other sections follow it unchanged.
//! Upstream's `docs` section is left out: see "Default System Prompt" in
//! `docs/rust-rewrite/non-parity-register.md`.

use std::path::Path;

use pi_config::ContextFile;

/// The product's name, wherever the prompt or the environment states it.
pub const PRODUCT_NAME: &str = "pi";

/// One active tool as the prompt presents it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptTool {
    pub name: String,
    /// The tool's line in the `tools` list; a tool without one is not listed.
    pub snippet: Option<String>,
    /// The tool's bullets in `rules`.
    pub guidelines: Vec<String>,
}

impl PromptTool {
    /// A builtin tool. `read` to `ls` use upstream's text
    /// (`core/tools/<name>.ts`); `todo`, `web_search`, and `web_fetch` exist
    /// only here.
    pub fn builtin(name: &str) -> Self {
        let (snippet, guidelines): (&str, &[&str]) = match name {
            "read" => (
                "Read file contents",
                &["Use read to examine files instead of cat or sed."],
            ),
            "bash" => (
                "Execute bash commands (ls, grep, find, etc.)",
                &["You can inspect PI_* environment variables for current model and session details."],
            ),
            "edit" => (
                "Make precise file edits with exact text replacement, including multiple disjoint edits in one call",
                &[
                    "Use edit for precise changes (edits[].oldText must match exactly)",
                    "When changing multiple separate locations in one file, use one edit call with multiple entries in edits[] instead of multiple edit calls",
                    "Each edits[].oldText is matched against the original file, not after earlier edits are applied. Do not emit overlapping or nested edits. Merge nearby changes into one edit.",
                    "Keep edits[].oldText as small as possible while still being unique in the file. Do not pad with large unchanged regions.",
                ],
            ),
            "write" => (
                "Create or overwrite files",
                &["Use write only for new files or complete rewrites."],
            ),
            "grep" => ("Search file contents for patterns (respects .gitignore)", &[]),
            "find" => ("Find files by glob pattern (respects .gitignore)", &[]),
            "ls" => ("List directory contents", &[]),
            "todo" => ("Track multi-step work as a checklist", &[]),
            "web_search" => ("Search the web for current information", &[]),
            "web_fetch" => ("Fetch a web page as readable text", &[]),
            _ => ("", &[]),
        };
        Self {
            name: name.to_string(),
            snippet: normalize_snippet(Some(snippet)),
            guidelines: guidelines.iter().map(|rule| rule.to_string()).collect(),
        }
    }
}

/// A tool's snippet as one line, as upstream lists it; `None` when empty.
pub fn normalize_snippet(snippet: Option<&str>) -> Option<String> {
    let line = snippet?.split_whitespace().collect::<Vec<_>>().join(" ");
    (!line.is_empty()).then_some(line)
}

/// A tool's guidelines as upstream keeps them: trimmed, without empty or
/// repeated ones.
pub fn normalize_guidelines(guidelines: &[String]) -> Vec<String> {
    let mut kept: Vec<String> = Vec::new();
    for guideline in guidelines.iter().map(|guideline| guideline.trim()) {
        if !guideline.is_empty() && !kept.iter().any(|seen| seen == guideline) {
            kept.push(guideline.to_string());
        }
    }
    kept
}

/// What a prompt is built from.
#[derive(Debug, Clone)]
pub struct SystemPromptInput<'a> {
    /// `--system-prompt`, the `systemPrompt` setting, or `SYSTEM.md`. An
    /// empty one counts as none.
    pub custom_prompt: Option<&'a str>,
    /// The tools the request declares, in its order.
    pub tools: &'a [PromptTool],
    /// `--append-system-prompt`, the `appendSystemPrompt` setting, or
    /// `APPEND_SYSTEM.md`.
    pub append_system_prompt: &'a str,
    pub context_files: &'a [ContextFile],
    /// The skills listing; `None` until skills are listed here. Upstream
    /// renders it with `formatSkillsForPrompt` and only while `read` or
    /// `bash` is active, naming that tool as the way to read a skill's file.
    pub skills: Option<&'a str>,
    pub cwd: &'a Path,
}

/// The prompt text: upstream's `buildSystemPrompt`, without its `docs`
/// section.
pub fn build_system_prompt(input: &SystemPromptInput<'_>) -> String {
    let mut sections = Vec::new();
    match input.custom_prompt.filter(|prompt| !prompt.is_empty()) {
        Some(custom) => sections.push(custom.to_string()),
        None => {
            sections.push(format!(
                "You are an expert coding assistant operating inside {PRODUCT_NAME}, a coding agent harness. You help users by reading files, executing commands, editing code, and writing new files."
            ));
            sections.push(section("tools", &tool_list(input.tools)));
            sections.push(section("rules", &rules(input.tools)));
        }
    }
    if !input.append_system_prompt.is_empty() {
        sections.push(section("addendum", input.append_system_prompt));
    }
    if !input.context_files.is_empty() {
        sections.push(section(
            "project_context",
            &project_context(input.context_files),
        ));
    }
    if let Some(skills) = input.skills.map(str::trim).filter(|text| !text.is_empty()) {
        sections.push(section("skills", skills));
    }
    let cwd = input.cwd.display().to_string().replace('\\', "/");
    sections.push(section("cwd", &cwd));
    sections.join("\n\n")
}

fn section(name: &str, content: &str) -> String {
    format!("<{name}>\n{content}\n</{name}>")
}

fn tool_list(tools: &[PromptTool]) -> String {
    let listed = tools
        .iter()
        .filter_map(|tool| Some(format!("- {}: {}", tool.name, tool.snippet.as_deref()?)))
        .collect::<Vec<_>>();
    let listed = if listed.is_empty() {
        "(none)".to_string()
    } else {
        listed.join("\n")
    };
    format!(
        "{listed}\n\nIn addition to the tools above, you may have access to other custom tools depending on the project."
    )
}

fn rules(tools: &[PromptTool]) -> String {
    let has = |name: &str| tools.iter().any(|tool| tool.name == name);
    let mut rules: Vec<&str> = Vec::new();
    if has("bash") && !has("grep") && !has("find") && !has("ls") {
        rules.push("Use bash for file operations like ls, rg, find");
    }
    rules.extend(
        tools
            .iter()
            .flat_map(|tool| &tool.guidelines)
            .map(String::as_str),
    );
    rules.push("Be concise in your responses");
    rules.push("Show file paths clearly when working with files");

    let mut kept: Vec<&str> = Vec::new();
    for rule in rules.into_iter().map(str::trim) {
        if !rule.is_empty() && !kept.contains(&rule) {
            kept.push(rule);
        }
    }
    kept.iter()
        .map(|rule| format!("- {rule}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn project_context(files: &[ContextFile]) -> String {
    std::iter::once("Project-specific instructions and guidelines:".to_string())
        .chain(files.iter().map(|file| {
            format!(
                "<project_instructions path=\"{}\">\n{}\n</project_instructions>",
                file.path.display(),
                file.content
            )
        }))
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::PathBuf;

    use pi_config::ResourceFile;
    use pi_tools::builtin_tool_definitions;
    use serde_json::{json, Value};

    use super::*;
    use crate::{
        active_tool_definitions, extension_tools_from_resources, new_session_id,
        runtime_system_prompt, ReloadableSystems, Runtime, SessionState,
    };

    fn fixture() -> Value {
        serde_json::from_str(include_str!(
            "../../../tests/fixtures/ts-parity/system-prompt.json"
        ))
        .expect("parse TS parity fixture")
    }

    /// Sections of upstream's prompt pi leaves out, each with its heading
    /// in docs/rust-rewrite/non-parity-register.md.
    const OMITTED_UPSTREAM_SECTIONS: &[(&str, &str)] = &[("docs", "### Default System Prompt")];

    fn text(value: &Value, tmp: &Path) -> String {
        let text = value.as_str().expect("fixture string");
        text.replace("<tmp>", &tmp.display().to_string())
            .replace("<product>", PRODUCT_NAME)
    }

    /// A runtime set up as the fixture case's `input` describes: its tools,
    /// prompt sources, and context files. `dir` holds the extension manifest.
    fn runtime_for(input: &Value, tmp: &Path, dir: &Path) -> Runtime {
        let extension = dir.join("tools");
        fs::write(&extension, "").unwrap();
        fs::write(
            dir.join("tools.pi-extension.json"),
            json!({ "protocol": "json", "tools": input["extensionTools"] }).to_string(),
        )
        .unwrap();
        let selected = input["selectedTools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|name| name.as_str().unwrap().to_string())
            .collect::<BTreeSet<_>>();
        let mut session = SessionState::new("prompt", PathBuf::from(text(&input["cwd"], tmp)));
        session.disabled_tools = builtin_tool_definitions()
            .into_iter()
            .map(|tool| tool.name)
            .filter(|name| !selected.contains(name))
            .collect();
        let mut runtime = Runtime::new(session, ReloadableSystems::default());
        runtime.systems.extension_tools = extension_tools_from_resources(&[ResourceFile {
            name: "tools".into(),
            path: extension,
            content: String::new(),
        }]);
        runtime.systems.system_prompt = input["customPrompt"].as_str().map(str::to_string);
        runtime.systems.append_system_prompt = vec![text(&input["appendSystemPrompt"], tmp)];
        runtime.systems.context_files = input["contextFiles"]
            .as_array()
            .unwrap()
            .iter()
            .map(|file| ContextFile {
                path: PathBuf::from(text(&file["path"], tmp)),
                content: text(&file["content"], tmp),
            })
            .collect();
        runtime
    }

    /// For every case of `system-prompt.json`, a runtime with upstream's
    /// inputs sends upstream's prompt, to the byte, apart from the product
    /// name and the sections pi leaves out.
    #[test]
    fn system_prompt_matches_ts_builder() {
        let fixture = fixture();
        let register = include_str!("../../../docs/rust-rewrite/non-parity-register.md");
        for (section, heading) in OMITTED_UPSTREAM_SECTIONS {
            assert!(
                register.contains(heading),
                "non-parity register must record the missing {section} section under {heading:?}"
            );
        }
        let omitted = |section: &Value| {
            OMITTED_UPSTREAM_SECTIONS
                .iter()
                .any(|(name, _)| section["name"] == *name)
        };
        let dir = std::env::temp_dir().join(format!("pi-system-prompt-{}", new_session_id()));
        fs::create_dir_all(&dir).unwrap();
        let tmp = Path::new("/work");
        let cases = fixture["prompts"].as_array().unwrap();
        let names = cases
            .iter()
            .map(|case| case["name"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "default-tools",
                "all-tools",
                "tool-subset",
                "no-tools",
                "context-files",
                "custom-prompt",
                "appended-prompt",
                "custom-and-appended-prompt",
                "extension-tool",
            ]
        );
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let sections = case["sections"].as_array().unwrap();
            let joined = |keep: &dyn Fn(&Value) -> bool| {
                sections
                    .iter()
                    .filter(|section| keep(section))
                    .map(|section| text(&section["text"], tmp))
                    .collect::<Vec<_>>()
                    .join("\n\n")
            };
            assert_eq!(
                joined(&|_| true),
                text(&case["prompt"], tmp),
                "{name}: sections"
            );
            // A custom prompt replaces upstream's omitted sections too.
            assert_eq!(
                sections.iter().any(omitted),
                case["input"]["customPrompt"].is_null(),
                "{name}: stale exception in OMITTED_UPSTREAM_SECTIONS"
            );
            let expected = joined(&|section| !omitted(section));

            let runtime = runtime_for(&case["input"], tmp, &dir);
            let declared = active_tool_definitions(&runtime)
                .into_iter()
                .map(|tool| json!(tool.name))
                .collect::<Vec<_>>();
            assert_eq!(
                json!(declared),
                case["input"]["selectedTools"],
                "{name}: tools"
            );
            assert_eq!(runtime_system_prompt(&runtime), expected, "{name}");
        }
        let _ = fs::remove_dir_all(dir);
    }

    /// The prompt of a session with every builtin tool, which is pi's
    /// default: the tools upstream lacks are listed in upstream's style.
    #[test]
    fn default_prompt_lists_every_builtin_tool() {
        let runtime = Runtime::new(
            SessionState::new("prompt", PathBuf::from("/work/repo")),
            ReloadableSystems::default(),
        );
        assert_eq!(
            runtime_system_prompt(&runtime),
            format!(
                "\
You are an expert coding assistant operating inside {PRODUCT_NAME}, a coding agent harness. You help users by reading files, executing commands, editing code, and writing new files.

<tools>
- read: Read file contents
- bash: Execute bash commands (ls, grep, find, etc.)
- edit: Make precise file edits with exact text replacement, including multiple disjoint edits in one call
- write: Create or overwrite files
- grep: Search file contents for patterns (respects .gitignore)
- find: Find files by glob pattern (respects .gitignore)
- ls: List directory contents
- todo: Track multi-step work as a checklist
- web_search: Search the web for current information
- web_fetch: Fetch a web page as readable text

In addition to the tools above, you may have access to other custom tools depending on the project.
</tools>

<rules>
- Use read to examine files instead of cat or sed.
- You can inspect PI_* environment variables for current model and session details.
- Use edit for precise changes (edits[].oldText must match exactly)
- When changing multiple separate locations in one file, use one edit call with multiple entries in edits[] instead of multiple edit calls
- Each edits[].oldText is matched against the original file, not after earlier edits are applied. Do not emit overlapping or nested edits. Merge nearby changes into one edit.
- Keep edits[].oldText as small as possible while still being unique in the file. Do not pad with large unchanged regions.
- Use write only for new files or complete rewrites.
- Be concise in your responses
- Show file paths clearly when working with files
</rules>

<cwd>
/work/repo
</cwd>"
            )
        );
    }

    #[test]
    fn every_builtin_tool_has_a_snippet() {
        for tool in builtin_tool_definitions() {
            let snippet = PromptTool::builtin(&tool.name).snippet;
            assert!(snippet.is_some(), "{}", tool.name);
        }
    }

    #[test]
    fn skills_go_between_project_context_and_cwd() {
        let files = [ContextFile {
            path: PathBuf::from("/work/AGENTS.md"),
            content: "rules".into(),
        }];
        let mut input = SystemPromptInput {
            custom_prompt: Some("custom"),
            tools: &[],
            append_system_prompt: "",
            context_files: &files,
            skills: Some("\nlisting\n"),
            cwd: Path::new("/work"),
        };
        assert_eq!(
            build_system_prompt(&input),
            "custom\n\n<project_context>\nProject-specific instructions and guidelines:\n\n\
             <project_instructions path=\"/work/AGENTS.md\">\nrules\n</project_instructions>\n\
             </project_context>\n\n<skills>\nlisting\n</skills>\n\n<cwd>\n/work\n</cwd>"
        );
        input.skills = Some(" \n");
        assert!(!build_system_prompt(&input).contains("<skills>"));
    }

    #[test]
    fn an_empty_custom_prompt_keeps_the_default_and_cwd_uses_forward_slashes() {
        let prompt = build_system_prompt(&SystemPromptInput {
            custom_prompt: Some(""),
            tools: &[],
            append_system_prompt: "",
            context_files: &[],
            skills: None,
            cwd: Path::new("C:\\work\\repo"),
        });
        assert!(prompt.starts_with("You are an expert coding assistant"));
        assert!(prompt.ends_with("<cwd>\nC:/work/repo\n</cwd>"));
    }
}
