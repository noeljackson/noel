//! Skills and prompt templates as the commands a user sees: command
//! completion, `/skills`, `/prompts`, and noel's own `/prompt <name>`.
//! Completion describes a command as upstream's does: a template's argument
//! hint, the scope it was loaded from, and its description.

use std::path::Path;

use noel_config::{LoadedConfig, PromptTemplate, ResourceOptions, SourceInfo, SourceScope};

use crate::Cli;

/// The command line's skill and prompt template flags.
pub(crate) fn resource_options(cli: &Cli) -> ResourceOptions {
    ResourceOptions {
        skill_paths: cli.skill.clone(),
        no_skills: cli.no_skills,
        prompt_template_paths: cli.prompt_template.clone(),
        no_prompt_templates: cli.no_prompt_templates,
        ..ResourceOptions::from_env()
    }
}

/// A skill's or a template's command.
struct ResourceCommand<'a> {
    /// `/name` or `/skill:name`.
    command: String,
    /// What typing the command without `skill:` would also mean to find.
    short: Option<String>,
    /// A template's argument hint, then `[scope] description`.
    detail: String,
    path: &'a Path,
}

/// `[u]`, `[p]`, or `[t]` and the description on one line, after the hint.
fn detail(hint: Option<&str>, source: &SourceInfo, description: &str) -> String {
    let scope = match source.scope {
        SourceScope::User => "u",
        SourceScope::Project => "p",
        SourceScope::Temporary => "t",
    };
    let description = description.split_whitespace().collect::<Vec<_>>().join(" ");
    let described = format!("[{scope}] {description}");
    match hint {
        Some(hint) => format!("{hint} \u{2014} {}", described.trim_end()),
        None => described.trim_end().to_string(),
    }
}

/// The prompt templates' commands.
fn template_commands(config: &LoadedConfig) -> impl Iterator<Item = ResourceCommand<'_>> {
    config
        .prompt_templates
        .iter()
        .map(|template| ResourceCommand {
            command: format!("/{}", template.name),
            short: None,
            detail: detail(
                template.argument_hint.as_deref(),
                &template.source_info,
                &template.description,
            ),
            path: &template.file_path,
        })
}

/// The skills' commands.
fn skill_commands(config: &LoadedConfig) -> impl Iterator<Item = ResourceCommand<'_>> {
    config.skills.iter().map(|skill| ResourceCommand {
        command: format!("/skill:{}", skill.name),
        short: Some(format!("/{}", skill.name)),
        detail: detail(None, &skill.source_info, &skill.description),
        path: &skill.file_path,
    })
}

/// Completion lines for the template and skill commands `prefix` starts: the
/// command, then its detail. A skill is also found by its bare name.
pub(crate) fn completions(config: &LoadedConfig, prefix: &str) -> Vec<String> {
    template_commands(config)
        .chain(skill_commands(config))
        .filter(|command| {
            let short = command.short.as_deref();
            command.command.starts_with(prefix)
                || short.is_some_and(|short| short.starts_with(prefix))
        })
        .map(|command| format!("{}  {}", command.command, command.detail))
        .collect()
}

fn listing<'a>(commands: impl Iterator<Item = ResourceCommand<'a>>, none: &str) -> String {
    let lines = commands
        .map(|command| {
            let path = command.path.display();
            format!("{}\t{}\t{path}", command.command, command.detail)
        })
        .collect::<Vec<_>>();
    if lines.is_empty() {
        none.to_string()
    } else {
        lines.join("\n")
    }
}

/// `/skills`: each skill's command, description, and file.
pub(crate) fn format_skills(config: &LoadedConfig) -> String {
    listing(skill_commands(config), "no skills")
}

/// `/prompts`: each template's command, description, and file.
pub(crate) fn format_prompts(config: &LoadedConfig) -> String {
    listing(template_commands(config), "no prompts")
}

pub(crate) fn find_template<'a>(
    config: &'a LoadedConfig,
    name: &str,
) -> Option<&'a PromptTemplate> {
    let mut templates = config.prompt_templates.iter();
    templates.find(|template| template.name == name)
}

/// noel's own `/prompt <name> [input]`: `{{input}}` in the template stands for
/// the input, which otherwise follows the template. Upstream's `/name`
/// commands and `$1` placeholders are `noel_core::expand_input`.
pub(crate) fn fill_prompt_template(template: &str, input: &str) -> String {
    if template.contains("{{input}}") {
        return template.replace("{{input}}", input);
    }
    if input.is_empty() {
        template.to_string()
    } else {
        format!("{template}\n\n{input}")
    }
}
