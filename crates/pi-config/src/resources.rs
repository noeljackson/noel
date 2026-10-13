//! Where skills and prompt templates come from, as upstream finds them
//! (`core/package-manager.ts` and `core/resource-loader.ts`): the files to
//! load, in the order that decides which of two same-named resources wins.
//!
//! Upstream reads a directory's entries in the order the filesystem lists
//! them; here they are read by name.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path, PathBuf};

use crate::prompt_templates::{load_prompt_templates, PromptTemplate};
use crate::skills::{load_skills, Skill};
use crate::{
    apply_resource_patterns, canonical_path, find_git_root, package_resource_entries,
    read_package_manifest, resolve_resource_path, resource_ignore_pattern_matches, ConfigPaths,
    PackageResourceType, PackageSource, Settings, CONFIG_DIR_NAME, RESOURCE_IGNORE_FILES,
};

/// What the command line and the environment add to resource discovery.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResourceOptions {
    /// The user's home directory: `~/.agents/skills` is read below it, and
    /// `~` in a resource path stands for it. `None` reads no
    /// `~/.agents/skills`.
    pub home_dir: Option<PathBuf>,
    /// `--skill`: skill files or directories, loaded after every discovered
    /// skill.
    pub skill_paths: Vec<PathBuf>,
    /// `--no-skills`: only `skill_paths` load.
    pub no_skills: bool,
    /// `--prompt-template`: template files or directories, loaded after
    /// every discovered template.
    pub prompt_template_paths: Vec<PathBuf>,
    /// `--no-prompt-templates`: only `prompt_template_paths` load.
    pub no_prompt_templates: bool,
}

impl ResourceOptions {
    /// A run without resource flags: `~/.agents/skills` is read below
    /// `$HOME`, except under the test harness.
    pub fn from_env() -> Self {
        Self {
            home_dir: (!crate::test_isolation())
                .then(|| crate::home_dir().ok())
                .flatten(),
            ..Self::default()
        }
    }
}

/// Who a resource belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceScope {
    User,
    Project,
    /// Given for this run only, on the command line.
    Temporary,
}

impl SourceScope {
    pub fn as_str(self) -> &'static str {
        match self {
            SourceScope::User => "user",
            SourceScope::Project => "project",
            SourceScope::Temporary => "temporary",
        }
    }
}

/// Where a skill or prompt template came from (upstream's `SourceInfo`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceInfo {
    pub path: PathBuf,
    /// `auto` for a conventional directory, `local` for a setting or a
    /// command-line path, or the package's source.
    pub source: String,
    pub scope: SourceScope,
    /// Whether the resource belongs to a package.
    pub from_package: bool,
    pub base_dir: Option<PathBuf>,
}

impl SourceInfo {
    /// Upstream's `origin`.
    pub fn origin(&self) -> &'static str {
        if self.from_package {
            "package"
        } else {
            "top-level"
        }
    }

    /// A resource loaded from a command-line path.
    pub(crate) fn local(path: &Path, scope: SourceScope, base_dir: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            source: "local".into(),
            scope,
            from_package: false,
            base_dir: Some(base_dir.to_path_buf()),
        }
    }
}

/// Something to tell the user about a skill or prompt template, worded as
/// upstream words it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResourceDiagnostic {
    pub message: String,
    /// The file or path the diagnostic is about; for a name collision, the
    /// resource that was skipped.
    pub path: PathBuf,
    /// For a name collision: the resource that was kept.
    pub winner: Option<PathBuf>,
}

impl ResourceDiagnostic {
    pub(crate) fn new(message: impl Into<String>, path: &Path) -> Self {
        Self {
            message: message.into(),
            path: path.to_path_buf(),
            winner: None,
        }
    }

    /// `loser` is skipped: `winner` already has the name a command shows as
    /// `name`.
    pub(crate) fn collision(name: &str, winner: &Path, loser: &Path) -> Self {
        Self {
            winner: Some(winner.to_path_buf()),
            ..Self::new(format!("name \"{name}\" collision"), loser)
        }
    }

    /// The diagnostic as one line for `/diagnostics`, about a `kind` such
    /// as `skill`.
    pub(crate) fn describe(&self, kind: &str) -> String {
        match &self.winner {
            Some(winner) => format!(
                "{kind} {}: skipped {}, {} was loaded first",
                self.message,
                self.path.display(),
                winner.display()
            ),
            None => format!("{kind} {}: {}", self.path.display(), self.message),
        }
    }
}

/// A file or directory to load resources from, and what is known of where
/// it comes from. Command-line paths carry no source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResourcePath {
    pub path: PathBuf,
    pub source: Option<SourceInfo>,
}

/// The skills and prompt templates of a configuration, with their
/// diagnostics.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct LoadedResources {
    pub skills: Vec<Skill>,
    pub skill_diagnostics: Vec<ResourceDiagnostic>,
    pub prompt_templates: Vec<PromptTemplate>,
    pub prompt_diagnostics: Vec<ResourceDiagnostic>,
}

/// What resources are discovered from.
pub(crate) struct ResourceSources<'a> {
    pub paths: &'a ConfigPaths,
    pub project_trusted: bool,
    pub user_settings: &'a Settings,
    /// Empty for an untrusted project.
    pub project_settings: &'a Settings,
    /// The packages in effect.
    pub packages: &'a [PackageSource],
    pub options: &'a ResourceOptions,
}

/// Loads every skill and prompt template: discovered ones in upstream's
/// order (project setting, project directories, user setting, user
/// directories, packages), then the command line's. `--no-skills` and
/// `--no-prompt-templates` leave only the command line's. A setting's path
/// that does not exist, which upstream passes over, is added to `notes`.
pub(crate) fn load_resources(
    sources: &ResourceSources<'_>,
    notes: &mut Vec<String>,
) -> LoadedResources {
    let options = sources.options;
    let cwd = &sources.paths.cwd;
    let home = options.home_dir.as_deref();

    let discovered = discover(sources, Kind::Skills, notes);
    let explicit = explicit_paths(&options.skill_paths, cwd, home);
    let (skills, skill_diagnostics) = load_skills(
        &merge(&discovered, explicit, options.no_skills),
        &discovered,
        sources.paths,
    );

    let discovered = discover(sources, Kind::Prompts, notes);
    let explicit = explicit_paths(&options.prompt_template_paths, cwd, home);
    let (prompt_templates, prompt_diagnostics) = load_prompt_templates(
        &merge(&discovered, explicit.clone(), options.no_prompt_templates),
        &discovered,
        &explicit,
        sources.paths,
    );
    LoadedResources {
        skills,
        skill_diagnostics,
        prompt_templates,
        prompt_diagnostics,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Skills,
    Prompts,
}

/// The discovered files of `kind`, in the order they load.
fn discover(
    sources: &ResourceSources<'_>,
    kind: Kind,
    notes: &mut Vec<String>,
) -> Vec<ResourcePath> {
    let paths = sources.paths;
    let home = sources.options.home_dir.as_deref();
    let project_base = paths.cwd.join(CONFIG_DIR_NAME);
    let dir_name = match kind {
        Kind::Skills => "skills",
        Kind::Prompts => "prompts",
    };
    let setting = |settings: &'_ Settings| match kind {
        Kind::Skills => settings.skills.clone(),
        Kind::Prompts => settings.prompts.clone(),
    };
    let mut found = Found {
        kind,
        home,
        files: Vec::new(),
    };

    if sources.project_trusted {
        let entries = setting(sources.project_settings);
        found.configured(&entries, &project_base, SourceScope::Project, notes);
        let project = source_info("auto", SourceScope::Project, Some(&project_base));
        let dir = project_base.join(dir_name);
        match kind {
            Kind::Skills => {
                found.add(skill_entries(&dir, SkillLayout::Pi), &project);
                let user_agents = home.map(|home| normalize(&home.join(".agents/skills")));
                for agents in ancestor_agents_dirs(&paths.cwd) {
                    let dir = agents.join("skills");
                    if Some(&dir) != user_agents.as_ref() {
                        let source = source_info("auto", SourceScope::Project, Some(&agents));
                        found.add(skill_entries(&dir, SkillLayout::Agents), &source);
                    }
                }
            }
            Kind::Prompts => found.add(prompt_dir_entries(&dir), &project),
        }
    }

    let entries = setting(sources.user_settings);
    found.configured(&entries, &paths.agent_dir, SourceScope::User, notes);
    let user = source_info("auto", SourceScope::User, Some(&paths.agent_dir));
    let dir = paths.agent_dir.join(dir_name);
    match kind {
        Kind::Skills => {
            found.add(skill_entries(&dir, SkillLayout::Pi), &user);
            if let Some(agents) = home.map(|home| home.join(".agents")) {
                let source = source_info("auto", SourceScope::User, Some(&agents));
                found.add(
                    skill_entries(&agents.join("skills"), SkillLayout::Agents),
                    &source,
                );
            }
        }
        Kind::Prompts => found.add(prompt_dir_entries(&dir), &user),
    }

    let scope = if sources.project_settings.packages.is_empty() {
        SourceScope::User
    } else {
        SourceScope::Project
    };
    for package in sources.packages {
        let Ok(root) = resolve_resource_path(&paths.cwd, package.source()) else {
            continue;
        };
        if root.is_dir() {
            let source = SourceInfo {
                from_package: true,
                ..source_info(package.source(), scope, Some(&root))
            };
            found.add(package_files(&root, package, kind), &source);
        }
    }
    found.files
}

fn source_info(source: &str, scope: SourceScope, base_dir: Option<&Path>) -> SourceInfo {
    SourceInfo {
        path: PathBuf::new(),
        source: source.into(),
        scope,
        from_package: false,
        base_dir: base_dir.map(Path::to_path_buf),
    }
}

/// The files discovered so far.
struct Found<'a> {
    kind: Kind,
    home: Option<&'a Path>,
    files: Vec<ResourcePath>,
}

impl Found<'_> {
    fn add(&mut self, files: Vec<PathBuf>, source: &SourceInfo) {
        self.files
            .extend(files.into_iter().map(|path| ResourcePath {
                source: Some(SourceInfo {
                    path: path.clone(),
                    ..source.clone()
                }),
                path,
            }));
    }

    /// Adds what the `entries` of a skills or prompts setting name,
    /// relative to `base`.
    fn configured(
        &mut self,
        entries: &[String],
        base: &Path,
        scope: SourceScope,
        notes: &mut Vec<String>,
    ) {
        let source = source_info("local", scope, None);
        for entry in entries {
            let path = resolve_path(Path::new(entry), base, self.home);
            if !path.exists() {
                notes.push(format!("resource path not found: {}", path.display()));
            }
            self.add(entry_files(&path, self.kind), &source);
        }
    }
}

/// The files of `kind` a package gives: those its manifest names, or its
/// conventional directory's, narrowed by the package's filters in settings.
fn package_files(root: &Path, package: &PackageSource, kind: Kind) -> Vec<PathBuf> {
    // The loader of extensions and themes reports a manifest it cannot read.
    let manifest = read_package_manifest(root, &mut Vec::new());
    let (entries, resource_type) = match kind {
        Kind::Skills => (
            package_resource_entries(manifest.as_ref().map(|manifest| &manifest.skills), "skills"),
            PackageResourceType::Skills,
        ),
        Kind::Prompts => (
            package_resource_entries(
                manifest.as_ref().map(|manifest| &manifest.prompts),
                "prompts",
            ),
            PackageResourceType::Prompts,
        ),
    };
    let filters = package.filters(resource_type);
    if matches!(filters, Some(filters) if filters.is_empty()) {
        return Vec::new();
    }
    let files = entries
        .iter()
        .flat_map(|entry| entry_files(&normalize(&root.join(entry)), kind))
        .collect::<Vec<_>>();
    match filters {
        Some(filters) => apply_resource_patterns(&files, filters, root),
        None => files,
    }
}

/// The files a configured path gives: itself, or what its directory holds.
fn entry_files(path: &Path, kind: Kind) -> Vec<PathBuf> {
    if path.is_file() {
        return vec![path.to_path_buf()];
    }
    match kind {
        Kind::Skills => skill_entries(path, SkillLayout::Pi),
        Kind::Prompts => {
            let mut files = Vec::new();
            let mut rules = IgnoreRules::new(path);
            collect_markdown(path, &mut rules, &mut files);
            files
        }
    }
}

/// Which Markdown files beside `SKILL.md` are skills.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SkillLayout {
    /// pi's: those directly in the skills directory.
    Pi,
    /// `.agents/skills`: those below it, in a directory without `SKILL.md`.
    Agents,
}

/// The skill files below `dir`: a directory's `SKILL.md` and nothing else
/// below it, or the described Markdown files `layout` allows.
pub(crate) fn skill_entries(dir: &Path, layout: SkillLayout) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut rules = IgnoreRules::new(dir);
    collect_skills(dir, dir, layout, &mut rules, &mut files);
    files
}

fn collect_skills(
    dir: &Path,
    root: &Path,
    layout: SkillLayout,
    rules: &mut IgnoreRules,
    files: &mut Vec<PathBuf>,
) {
    rules.read(dir);
    let entries = sorted_entries(dir);
    let declared = dir.join("SKILL.md");
    if entries.contains(&declared) && declared.is_file() && !rules.ignores(&declared) {
        files.push(declared);
        return;
    }
    for path in entries {
        if hidden(&path) || rules.ignores(&path) {
            continue;
        }
        if path.is_dir() {
            let mut rules = rules.clone();
            collect_skills(&path, root, layout, &mut rules, files);
        } else if path.is_file()
            && is_markdown(&path)
            && (dir == root) == (layout == SkillLayout::Pi)
        {
            files.push(path);
        }
    }
}

/// The Markdown files directly in a prompts directory.
fn prompt_dir_entries(dir: &Path) -> Vec<PathBuf> {
    let mut rules = IgnoreRules::new(dir);
    rules.read(dir);
    let entries = sorted_entries(dir).into_iter();
    entries
        .filter(|path| !hidden(path) && !rules.ignores(path) && path.is_file() && is_markdown(path))
        .collect()
}

/// Every Markdown file below `dir`.
fn collect_markdown(dir: &Path, rules: &mut IgnoreRules, files: &mut Vec<PathBuf>) {
    rules.read(dir);
    for path in sorted_entries(dir) {
        if hidden(&path) || rules.ignores(&path) {
            continue;
        }
        if path.is_dir() {
            collect_markdown(&path, &mut rules.clone(), files);
        } else if path.is_file() && is_markdown(&path) {
            files.push(path);
        }
    }
}

/// A directory's entries by name; none when it cannot be read.
pub(crate) fn sorted_entries(dir: &Path) -> Vec<PathBuf> {
    let entries = fs::read_dir(dir).into_iter().flatten().flatten();
    let mut paths = entries.map(|entry| entry.path()).collect::<Vec<_>>();
    paths.sort();
    paths
}

/// A dot file or `node_modules`, which discovery passes over.
fn hidden(path: &Path) -> bool {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    name.starts_with('.') || name == "node_modules"
}

pub(crate) fn is_markdown(path: &Path) -> bool {
    path.to_string_lossy().ends_with(".md")
}

/// The `.agents` directories from `cwd` up to the repository root, or to
/// the filesystem root outside a repository, nearest first.
fn ancestor_agents_dirs(cwd: &Path) -> Vec<PathBuf> {
    let cwd = normalize(cwd);
    let git_root = find_git_root(&cwd);
    let mut dirs = Vec::new();
    for dir in cwd.ancestors() {
        dirs.push(dir.join(".agents"));
        if git_root.as_deref() == Some(dir) {
            break;
        }
    }
    dirs
}

/// The `.gitignore`, `.ignore`, and `.fdignore` rules met on the way down a
/// resource directory. A rule read below the top applies from its own
/// directory, as upstream rewrites it.
#[derive(Debug, Clone)]
pub(crate) struct IgnoreRules {
    root: PathBuf,
    patterns: Vec<String>,
}

impl IgnoreRules {
    fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            patterns: Vec::new(),
        }
    }

    /// Adds the rules of the ignore files in `dir`.
    fn read(&mut self, dir: &Path) {
        let relative = dir.strip_prefix(&self.root).unwrap_or(Path::new(""));
        let prefix = match relative.to_string_lossy().as_ref() {
            "" => String::new(),
            relative => format!("{relative}/"),
        };
        for name in RESOURCE_IGNORE_FILES {
            let content = fs::read_to_string(dir.join(name)).unwrap_or_default();
            let rules = content
                .lines()
                .filter_map(|line| ignore_rule(line, &prefix));
            self.patterns.extend(rules);
        }
    }

    /// Whether the last rule that matches `path` hides it.
    fn ignores(&self, path: &Path) -> bool {
        let mut ignored = false;
        for pattern in &self.patterns {
            let (negated, pattern) = match pattern.strip_prefix('!') {
                Some(pattern) => (true, pattern),
                None => (false, pattern.as_str()),
            };
            if resource_ignore_pattern_matches(path, &self.root, pattern) {
                ignored = !negated;
            }
        }
        ignored
    }
}

/// One line of an ignore file as a rule relative to the top directory:
/// upstream's `prefixIgnorePattern`.
fn ignore_rule(line: &str, prefix: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    // Trailing white space does not count, as in a .gitignore.
    let line = line.trim_end();
    let unescaped = line.strip_prefix('\\').filter(|rest| rest.starts_with('!'));
    let (negated, pattern) = match line.strip_prefix('!') {
        Some(pattern) => (true, pattern),
        None => (false, unescaped.unwrap_or(line)),
    };
    let pattern = pattern.strip_prefix('/').unwrap_or(pattern);
    Some(format!(
        "{}{prefix}{pattern}",
        if negated { "!" } else { "" }
    ))
}

/// The command line's paths, resolved.
fn explicit_paths(paths: &[PathBuf], cwd: &Path, home: Option<&Path>) -> Vec<ResourcePath> {
    let resolve = |path: &PathBuf| ResourcePath {
        path: resolve_path(path, cwd, home),
        source: None,
    };
    paths.iter().map(resolve).collect()
}

/// The paths to load: discovered ones unless disabled, then explicit ones,
/// each file once whatever path reaches it.
fn merge(
    discovered: &[ResourcePath],
    explicit: Vec<ResourcePath>,
    only_explicit: bool,
) -> Vec<ResourcePath> {
    let discovered = discovered.iter().filter(|_| !only_explicit).cloned();
    let mut seen = BTreeSet::new();
    discovered
        .chain(explicit)
        .filter(|resource| seen.insert(canonical_path(&resource.path)))
        .collect()
}

/// `path` as upstream resolves a resource path: without surrounding white
/// space, `~` as the home directory, relative to `base`, and without `.` and
/// `..` segments.
pub(crate) fn resolve_path(path: &Path, base: &Path, home: Option<&Path>) -> PathBuf {
    let text = path.to_string_lossy();
    let text = crate::frontmatter::js_trim(&text);
    let home = home
        .map(Path::to_path_buf)
        .or_else(|| crate::home_dir().ok());
    let path = match (home, text.strip_prefix('~')) {
        (Some(home), Some("")) => home,
        (Some(home), Some(rest)) if rest.starts_with('/') => home.join(&rest[1..]),
        _ => PathBuf::from(text),
    };
    normalize(&base.join(path))
}

/// `path` without `.` and `..` segments, as Node's `path.resolve` leaves it.
pub(crate) fn normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            component => normalized.push(component),
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};

    use super::*;
    use crate::read_optional_settings;

    fn source_json(source: &SourceInfo) -> Value {
        let mut value = json!({
            "path": source.path,
            "source": source.source,
            "scope": source.scope.as_str(),
            "origin": source.origin(),
        });
        if let Some(base_dir) = &source.base_dir {
            value["baseDir"] = json!(base_dir);
        }
        value
    }

    /// Diagnostics as the fixture records upstream's. Upstream also types
    /// each one, for its color: a collision, an error (a prompt template
    /// path that does not exist), or a warning. Rust reports them alike, so
    /// the type here follows from the rest. A message that is not one of
    /// upstream's own is the frontmatter parser's.
    fn diagnostics_json(diagnostics: &[ResourceDiagnostic], resource_type: &str) -> Value {
        let own = [
            "description ",
            "name ",
            "skill path ",
            "Prompt template path ",
        ];
        let diagnostics = diagnostics.iter().map(|diagnostic| {
            let message = diagnostic.message.as_str();
            let known = own.iter().any(|start| message.starts_with(start));
            let missing_template = message.starts_with("Prompt template path");
            let mut value = json!({
                "type": if missing_template { "error" } else { "warning" },
                "message": if known { message } else { "<yaml-error>" },
                "path": diagnostic.path,
            });
            if let Some(winner) = &diagnostic.winner {
                let name = message.strip_prefix("name \"").unwrap();
                let name = name.strip_suffix("\" collision").unwrap();
                value["type"] = json!("collision");
                value["collision"] = json!({
                    "resourceType": resource_type,
                    "name": name.trim_start_matches('/'),
                    "winnerPath": winner,
                    "loserPath": diagnostic.path,
                });
            }
            value
        });
        diagnostics.collect()
    }

    /// For every discovery case of `skills.json`, the skills and prompt
    /// templates upstream's resource loader loads from the same tree and
    /// flags: in its order, with its sources and its diagnostics.
    #[test]
    fn discovery_matches_ts_resource_loader() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/ts-parity/skills.json"
        ))
        .expect("parse TS parity fixture");
        let cases = fixture["discovery"].as_array().unwrap();
        let names = cases.iter().map(|case| case["name"].as_str().unwrap());
        assert_eq!(
            names.collect::<Vec<_>>(),
            [
                "default-directories",
                "untrusted-project",
                "agents-directories",
                "agents-directories-without-repository",
                "untrusted-project-below-home",
                "frontmatter-rules",
                "name-collisions",
                "explicit-paths",
                "no-skills-and-no-prompt-templates",
                "no-skills-without-paths",
                "settings-paths",
                "settings-paths-untrusted",
                "local-package",
                "ignore-files",
            ]
        );
        for case in cases {
            let name = case["name"].as_str().unwrap();
            // Outside any repository: one case reads every ancestor's .agents.
            let root = std::env::temp_dir().join(format!(
                "pi-discovery-{name}-{}-{}",
                std::process::id(),
                std::time::SystemTime::UNIX_EPOCH
                    .elapsed()
                    .unwrap()
                    .as_nanos()
            ));
            let tmp = root.to_string_lossy().into_owned();
            let cwd = root.join(case["cwd"].as_str().unwrap());
            for dir in [&root.join("agent"), &root.join("home"), &cwd] {
                fs::create_dir_all(dir).unwrap();
            }
            for (path, content) in case["files"].as_object().unwrap() {
                let content = content.as_str().unwrap();
                let content = content.replace("<tmp>", &tmp).replace("<bom>", "\u{feff}");
                fs::create_dir_all(root.join(path).parent().unwrap()).unwrap();
                fs::write(root.join(path), content).unwrap();
            }
            for (path, target) in case["symlinks"].as_object().unwrap() {
                let target = root.join(target.as_str().unwrap());
                std::os::unix::fs::symlink(target, root.join(path)).unwrap();
            }
            let flag_paths = |flag: &str| {
                let paths = case["flags"][flag].as_array().unwrap().iter();
                paths
                    .map(|path| PathBuf::from(path.as_str().unwrap().replace("<tmp>", &tmp)))
                    .collect::<Vec<_>>()
            };
            let options = ResourceOptions {
                home_dir: Some(root.join("home")),
                skill_paths: flag_paths("skill"),
                no_skills: case["flags"]["noSkills"].as_bool().unwrap(),
                prompt_template_paths: flag_paths("promptTemplate"),
                no_prompt_templates: case["flags"]["noPromptTemplates"].as_bool().unwrap(),
            };
            let agent_dir = root.join("agent");
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
            let trusted = case["trusted"].as_bool().unwrap();
            let settings = |path: &Path| read_optional_settings(path).unwrap().unwrap_or_default();
            let user_settings = settings(&paths.settings_path);
            let project_settings = match trusted {
                true => settings(&paths.project_settings_path),
                false => Settings::default(),
            };
            let merged = user_settings.clone().merge(project_settings.clone());
            let mut notes = Vec::new();
            let loaded = load_resources(
                &ResourceSources {
                    paths: &paths,
                    project_trusted: trusted,
                    user_settings: &user_settings,
                    project_settings: &project_settings,
                    packages: &merged.packages,
                    options: &options,
                },
                &mut notes,
            );
            assert_eq!(notes, Vec::<String>::new(), "{name}");

            let skills = loaded.skills.iter().map(|skill| {
                json!({
                    "name": skill.name,
                    "description": skill.description,
                    "filePath": skill.file_path,
                    "baseDir": skill.base_dir,
                    "disableModelInvocation": skill.disable_model_invocation,
                    "sourceInfo": source_json(&skill.source_info),
                })
            });
            let prompts = loaded.prompt_templates.iter().map(|template| {
                json!({
                    "name": template.name,
                    "description": template.description,
                    "argumentHint": template.argument_hint,
                    "content": template.content,
                    "filePath": template.file_path,
                    "sourceInfo": source_json(&template.source_info),
                })
            });
            let result = json!({
                "skills": skills.collect::<Vec<_>>(),
                "skillDiagnostics": diagnostics_json(&loaded.skill_diagnostics, "skill"),
                "prompts": prompts.collect::<Vec<_>>(),
                "promptDiagnostics": diagnostics_json(&loaded.prompt_diagnostics, "prompt"),
            });
            let result: Value =
                serde_json::from_str(&result.to_string().replace(&tmp, "<tmp>")).unwrap();
            for key in ["skills", "skillDiagnostics", "prompts", "promptDiagnostics"] {
                assert_eq!(
                    result[key], case["result"][key],
                    "{name}: {key}\nours: {:#}\nupstream: {:#}",
                    result[key], case["result"][key]
                );
            }
            let _ = fs::remove_dir_all(root);
        }
    }

    #[test]
    fn paths_resolve_as_upstream_resolves_them() {
        let home = Path::new("/home/user");
        let resolve =
            |path: &str| resolve_path(Path::new(path), Path::new("/work/repo"), Some(home));
        assert_eq!(resolve("skills"), Path::new("/work/repo/skills"));
        assert_eq!(resolve("  ../shared/./x/ "), Path::new("/work/shared/x"));
        assert_eq!(resolve("/abs/../path"), Path::new("/path"));
        assert_eq!(resolve("~"), home);
        assert_eq!(resolve("~/skills"), Path::new("/home/user/skills"));
        assert_eq!(
            resolve("~user/skills"),
            Path::new("/work/repo/~user/skills")
        );
        assert_eq!(resolve("/../.."), Path::new("/"));
    }

    #[test]
    fn ignore_rules_apply_from_their_directory() {
        assert_eq!(ignore_rule("  # comment", ""), None);
        assert_eq!(ignore_rule("", "sub/"), None);
        assert_eq!(ignore_rule("drafts/", ""), Some("drafts/".into()));
        assert_eq!(ignore_rule("/top.md", ""), Some("top.md".into()));
        assert_eq!(ignore_rule("wip-*", "group/"), Some("group/wip-*".into()));
        assert_eq!(
            ignore_rule("!keep.md", "group/"),
            Some("!group/keep.md".into())
        );
        assert_eq!(ignore_rule("\\!literal", ""), Some("!literal".into()));
    }
}
