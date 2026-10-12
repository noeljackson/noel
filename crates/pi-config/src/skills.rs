//! Skills, as upstream loads and lists them (`core/skills.ts`): a skill is a
//! Markdown file whose frontmatter has a `description`, most often a
//! directory's `SKILL.md`. `skills.json` holds what upstream loads from
//! fixed trees and the listing it writes; the tests in this crate compare
//! against it.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::frontmatter::{js_trim, parse_frontmatter};
use crate::resources::{
    is_markdown, skill_entries, ResourceDiagnostic, ResourcePath, SkillLayout, SourceInfo,
    SourceScope,
};
use crate::{canonical_path, ConfigPaths, CONFIG_DIR_NAME};

const MAX_NAME_LENGTH: usize = 64;
const MAX_DESCRIPTION_LENGTH: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    /// The frontmatter's `name`, or the name of the file's directory.
    pub name: String,
    pub description: String,
    pub file_path: PathBuf,
    /// The file's directory, which the skill's relative paths start from.
    pub base_dir: PathBuf,
    pub source_info: SourceInfo,
    /// `disable-model-invocation: true`: not listed in the system prompt,
    /// and loaded only by its `/skill:name` command.
    pub disable_model_invocation: bool,
}

/// Loads the skills of `paths`, files or directories, in order. The first
/// skill of a name wins and each later one is reported, after the other
/// diagnostics. A skill loaded from a path without a source takes the
/// source `discovered` has for its file, if any.
pub(crate) fn load_skills(
    paths: &[ResourcePath],
    discovered: &[ResourcePath],
    config: &ConfigPaths,
) -> (Vec<Skill>, Vec<ResourceDiagnostic>) {
    let mut skills: Vec<Skill> = Vec::new();
    let mut loaded_files = BTreeSet::new();
    let mut diagnostics = Vec::new();
    let mut collisions = Vec::new();
    for resource in paths {
        let path = &resource.path;
        let files = if !path.exists() {
            diagnostics.push(ResourceDiagnostic::new("skill path does not exist", path));
            continue;
        } else if path.is_dir() {
            skill_entries(path, SkillLayout::Pi)
        } else if is_markdown(path) {
            vec![path.clone()]
        } else {
            let message = "skill path is not a markdown file";
            diagnostics.push(ResourceDiagnostic::new(message, path));
            continue;
        };
        for file in files {
            let source = resource.source.as_ref().or_else(|| {
                let found = discovered.iter().find(|resource| resource.path == file);
                found.and_then(|resource| resource.source.as_ref())
            });
            let source = match source {
                Some(source) => SourceInfo {
                    path: file.clone(),
                    ..source.clone()
                },
                None => explicit_source(&file, path, config),
            };
            let Some(skill) = load_skill_file(&file, source, &mut diagnostics) else {
                continue;
            };
            // A file reached by a second path, such as a link, loads once.
            let real_path = canonical_path(&skill.file_path);
            if loaded_files.contains(&real_path) {
                continue;
            }
            match skills.iter().find(|loaded| loaded.name == skill.name) {
                Some(winner) => collisions.push(ResourceDiagnostic::collision(
                    &skill.name,
                    &winner.file_path,
                    &skill.file_path,
                )),
                None => {
                    loaded_files.insert(real_path);
                    skills.push(skill);
                }
            }
        }
    }
    diagnostics.append(&mut collisions);
    (skills, diagnostics)
}

/// The source of a skill `file` loaded from the command-line path `given`.
fn explicit_source(file: &Path, given: &Path, config: &ConfigPaths) -> SourceInfo {
    let scope = if given.starts_with(config.agent_dir.join("skills")) {
        SourceScope::User
    } else if given.starts_with(config.cwd.join(CONFIG_DIR_NAME).join("skills")) {
        SourceScope::Project
    } else {
        SourceScope::Temporary
    };
    SourceInfo::local(file, scope, file.parent().unwrap_or(file))
}

/// Loads one skill file. A file without a description is no skill; that is
/// reported for a `SKILL.md`, which declares one, and passed over silently
/// for another Markdown file. A name or description outside the Agent
/// Skills rules is reported and loads all the same.
fn load_skill_file(
    path: &Path,
    source_info: SourceInfo,
    diagnostics: &mut Vec<ResourceDiagnostic>,
) -> Option<Skill> {
    let declared = path.file_name().is_some_and(|name| name == "SKILL.md");
    let content = match fs::read(path) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(error) => {
            diagnostics.push(ResourceDiagnostic::new(error.to_string(), path));
            return None;
        }
    };
    let frontmatter = match parse_frontmatter(&content) {
        Ok((frontmatter, _)) => frontmatter,
        Err(message) => {
            if declared {
                diagnostics.push(ResourceDiagnostic::new(message, path));
            }
            return None;
        }
    };
    let description = frontmatter.string("description");
    let description = description.filter(|description| !js_trim(description).is_empty());
    if !declared && description.is_none() {
        return None;
    }
    let mut warn = |message: String| diagnostics.push(ResourceDiagnostic::new(message, path));
    match description.map(|description| description.encode_utf16().count()) {
        None => warn("description is required".into()),
        Some(length) if length > MAX_DESCRIPTION_LENGTH => warn(format!(
            "description exceeds {MAX_DESCRIPTION_LENGTH} characters ({length})"
        )),
        Some(_) => {}
    }

    let base_dir = path.parent().unwrap_or(path);
    let directory_name = base_dir.file_name().unwrap_or_default().to_string_lossy();
    let name = frontmatter.string("name").filter(|name| !name.is_empty());
    let name = name.unwrap_or(&directory_name).to_string();
    for problem in name_problems(&name) {
        warn(problem);
    }
    Some(Skill {
        name,
        description: description?.to_string(),
        file_path: path.to_path_buf(),
        base_dir: base_dir.to_path_buf(),
        source_info,
        disable_model_invocation: frontmatter.is_true("disable-model-invocation"),
    })
}

/// How `name` breaks the Agent Skills naming rules.
fn name_problems(name: &str) -> Vec<String> {
    let mut problems = Vec::new();
    let length = name.encode_utf16().count();
    if length > MAX_NAME_LENGTH {
        problems.push(format!(
            "name exceeds {MAX_NAME_LENGTH} characters ({length})"
        ));
    }
    let allowed = |ch: char| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-';
    if name.is_empty() || !name.chars().all(allowed) {
        problems.push(
            "name contains invalid characters (must be lowercase a-z, 0-9, hyphens only)".into(),
        );
    }
    if name.starts_with('-') || name.ends_with('-') {
        problems.push("name must not start or end with a hyphen".into());
    }
    if name.contains("--") {
        problems.push("name must not contain consecutive hyphens".into());
    }
    problems
}

/// The tool the system prompt tells the model to load a skill's file with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillReader {
    Read,
    Bash,
}

/// The skills listing of the system prompt: upstream's
/// `formatSkillsForPrompt`. Empty when no skill may be listed.
pub fn format_skills_for_prompt(skills: &[Skill], reader: SkillReader) -> String {
    let listed = skills
        .iter()
        .filter(|skill| !skill.disable_model_invocation);
    let listed = listed.collect::<Vec<_>>();
    if listed.is_empty() {
        return String::new();
    }
    let load = match reader {
        SkillReader::Read => {
            "Use the read tool to load a skill's file when the task matches its description."
        }
        SkillReader::Bash => {
            "Use bash to load a skill's file when the task matches its description."
        }
    };
    let mut lines = vec![
        "\n\nThe following skills provide specialized instructions for specific tasks.".to_string(),
        load.to_string(),
        "When a skill file references a relative path, resolve it against the skill directory (parent of SKILL.md / dirname of the path) and use that absolute path in tool commands.".to_string(),
        String::new(),
        "<available_skills>".to_string(),
    ];
    for skill in listed {
        lines.push("  <skill>".to_string());
        lines.push(format!("    <name>{}</name>", escape_xml(&skill.name)));
        lines.push(format!(
            "    <description>{}</description>",
            escape_xml(&skill.description)
        ));
        lines.push(format!(
            "    <location>{}</location>",
            escape_xml(&skill.file_path.to_string_lossy())
        ));
        lines.push("  </skill>".to_string());
    }
    lines.push("</available_skills>".to_string());
    lines.join("\n")
}

fn escape_xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;

    /// For every listing case of `skills.json`, upstream's text to the byte.
    #[test]
    fn skills_listing_matches_ts_formatter() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/ts-parity/skills.json"
        ))
        .expect("parse TS parity fixture");
        let cases = fixture["listing"].as_array().unwrap();
        let names = cases.iter().map(|case| case["name"].as_str().unwrap());
        assert_eq!(
            names.collect::<Vec<_>>(),
            ["read", "bash", "only-hidden-skills", "no-skills"]
        );
        for case in cases {
            let skills = case["skills"].as_array().unwrap().iter().map(|skill| {
                let path = |key: &str| PathBuf::from(skill[key].as_str().unwrap());
                Skill {
                    name: skill["name"].as_str().unwrap().into(),
                    description: skill["description"].as_str().unwrap().into(),
                    file_path: path("filePath"),
                    base_dir: path("baseDir"),
                    source_info: SourceInfo::local(
                        &path("filePath"),
                        SourceScope::User,
                        &path("baseDir"),
                    ),
                    disable_model_invocation: skill["disableModelInvocation"].as_bool().unwrap(),
                }
            });
            let reader = match case["tool"].as_str().unwrap() {
                "read" => SkillReader::Read,
                "bash" => SkillReader::Bash,
                tool => panic!("unknown tool {tool}"),
            };
            assert_eq!(
                format_skills_for_prompt(&skills.collect::<Vec<_>>(), reader),
                case["text"].as_str().unwrap(),
                "{}",
                case["name"]
            );
        }
    }

    #[test]
    fn names_follow_the_agent_skills_rules() {
        assert!(name_problems("pdf-tools-2").is_empty());
        assert_eq!(name_problems("").len(), 1);
        assert_eq!(name_problems("PDF").len(), 1);
        assert_eq!(name_problems("-a--b-").len(), 2);
        assert_eq!(name_problems(&"a".repeat(65)).len(), 1);
    }
}
