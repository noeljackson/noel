//! Resource files: extensions, skills, prompts and themes found in the agent and project directories and in packages, with their ignore files and patterns.

use super::*;

pub(crate) fn load_resource_files(
    paths: &ConfigPaths,
    resource_dir_name: &str,
    project_trusted: bool,
    diagnostics: &mut Vec<String>,
) -> Vec<ResourceFile> {
    let mut dirs = vec![paths.agent_dir.join(resource_dir_name)];
    if project_trusted {
        dirs.push(project_config_dir(&paths.cwd).join(resource_dir_name));
    }
    let mut resources = BTreeMap::<String, ResourceFile>::new();
    for dir in dirs {
        if !dir.exists() {
            continue;
        }
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) => {
                diagnostics.push(format!("failed to read {}: {error}", dir.display()));
                continue;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    diagnostics.push(format!(
                        "failed to read entry in {}: {error}",
                        dir.display()
                    ));
                    continue;
                }
            };
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            if is_resource_metadata_file(&path) {
                continue;
            }
            push_resource_file(&mut resources, &path, diagnostics);
        }
    }
    resources.into_values().collect()
}

pub(crate) fn find_git_root(cwd: &Path) -> Option<PathBuf> {
    let mut current = Some(cwd);
    while let Some(dir) = current {
        if dir.join(".git").exists() {
            return Some(dir.to_path_buf());
        }
        current = dir.parent();
    }
    None
}

pub(crate) fn extend_resource_files(
    resources: &mut Vec<ResourceFile>,
    base_dir: &Path,
    entries: &[String],
    diagnostics: &mut Vec<String>,
) {
    let mut map = resources
        .drain(..)
        .map(|resource| (resource.name.clone(), resource))
        .collect::<BTreeMap<_, _>>();
    for entry in entries {
        let path = match resolve_resource_path(base_dir, entry) {
            Ok(path) => path,
            Err(error) => {
                diagnostics.push(format!("failed to resolve resource path {entry}: {error}"));
                continue;
            }
        };
        collect_resource_path(&mut map, &path, diagnostics);
    }
    resources.extend(map.into_values());
}

/// The extensions and themes of the packages. Their skills and prompt
/// templates load with the others, in `resources`.
pub(crate) fn load_package_resources(
    cwd: &Path,
    packages: &[PackageSource],
    extensions: &mut Vec<ResourceFile>,
    themes: &mut Vec<ResourceFile>,
    diagnostics: &mut Vec<String>,
) {
    for package in packages {
        let package_root = match resolve_resource_path(cwd, package.source()) {
            Ok(path) => path,
            Err(error) => {
                diagnostics.push(format!(
                    "failed to resolve package {}: {error}",
                    package.source()
                ));
                continue;
            }
        };
        if package_root.is_file() {
            extend_resource_files(
                extensions,
                cwd,
                &[package.source().to_string()],
                diagnostics,
            );
            continue;
        }
        if !package_root.is_dir() {
            diagnostics.push(format!(
                "package path not found: {}",
                package_root.display()
            ));
            continue;
        }
        load_package_root_resources(&package_root, package, extensions, themes, diagnostics);
    }
}

pub(crate) fn load_package_root_resources(
    package_root: &Path,
    package: &PackageSource,
    extensions: &mut Vec<ResourceFile>,
    themes: &mut Vec<ResourceFile>,
    diagnostics: &mut Vec<String>,
) {
    let manifest = read_package_manifest(package_root, diagnostics);
    let extension_entries = package_resource_entries(
        manifest.as_ref().map(|manifest| &manifest.extensions),
        "extensions",
    );
    let theme_entries =
        package_resource_entries(manifest.as_ref().map(|manifest| &manifest.themes), "themes");
    extend_package_resource_files(
        extensions,
        package_root,
        &extension_entries,
        package.filters(PackageResourceType::Extensions),
        diagnostics,
    );
    extend_package_resource_files(
        themes,
        package_root,
        &theme_entries,
        package.filters(PackageResourceType::Themes),
        diagnostics,
    );
}

pub(crate) fn read_package_manifest(
    package_root: &Path,
    diagnostics: &mut Vec<String>,
) -> Option<PackageManifest> {
    let path = package_root.join("package.json");
    if !path.exists() {
        return None;
    }
    match read_optional_json::<PackageJson>(&path) {
        Ok(Some(package_json)) => package_json.noel.or(package_json.pi),
        Ok(None) => None,
        Err(error) => {
            diagnostics.push(format!(
                "failed to read package manifest {}: {error}",
                path.display()
            ));
            None
        }
    }
}

pub(crate) fn package_resource_entries(
    manifest_entries: Option<&Vec<String>>,
    default_dir: &str,
) -> Vec<String> {
    manifest_entries
        .cloned()
        .unwrap_or_else(|| vec![default_dir.to_string()])
}

pub(crate) fn extend_package_resource_files(
    resources: &mut Vec<ResourceFile>,
    base_dir: &Path,
    entries: &[String],
    filters: Option<&[String]>,
    diagnostics: &mut Vec<String>,
) {
    let Some(filtered_paths) =
        collect_filtered_package_paths(base_dir, entries, filters, diagnostics)
    else {
        return;
    };
    let mut map = resources
        .drain(..)
        .map(|resource| (resource.name.clone(), resource))
        .collect::<BTreeMap<_, _>>();
    for path in filtered_paths {
        push_resource_file(&mut map, &path, diagnostics);
    }
    resources.extend(map.into_values());
}

pub(crate) fn collect_filtered_package_paths(
    base_dir: &Path,
    entries: &[String],
    filters: Option<&[String]>,
    diagnostics: &mut Vec<String>,
) -> Option<Vec<PathBuf>> {
    if matches!(filters, Some(filters) if filters.is_empty()) {
        return None;
    }
    let mut paths = Vec::new();
    for entry in entries {
        let path = match resolve_resource_path(base_dir, entry) {
            Ok(path) => path,
            Err(error) => {
                diagnostics.push(format!("failed to resolve resource path {entry}: {error}"));
                continue;
            }
        };
        collect_resource_paths(&mut paths, &path, diagnostics);
    }
    paths.sort();
    paths.dedup();
    Some(match filters {
        Some(filters) => apply_resource_patterns(&paths, filters, base_dir),
        None => paths,
    })
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct PackageJson {
    noel: Option<PackageManifest>,
    /// The key of packages written for pi, read when `noel` is absent.
    pi: Option<PackageManifest>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct PackageManifest {
    #[serde(default)]
    pub(crate) extensions: Vec<String>,
    #[serde(default)]
    pub(crate) skills: Vec<String>,
    #[serde(default)]
    pub(crate) prompts: Vec<String>,
    #[serde(default)]
    pub(crate) themes: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum PackageResourceType {
    Extensions,
    Skills,
    Prompts,
    Themes,
}

impl PackageResourceType {
    pub(crate) fn key(self) -> &'static str {
        match self {
            PackageResourceType::Extensions => "extensions",
            PackageResourceType::Skills => "skills",
            PackageResourceType::Prompts => "prompts",
            PackageResourceType::Themes => "themes",
        }
    }

    pub(crate) fn singular(self) -> &'static str {
        match self {
            PackageResourceType::Extensions => "extension",
            PackageResourceType::Skills => "skill",
            PackageResourceType::Prompts => "prompt",
            PackageResourceType::Themes => "theme",
        }
    }
}

/// Whether `disabledResources` in settings names this resource: by name,
/// as `skills/<name>` or `skill:<name>`, or by path. Wildcards apply.
pub(crate) fn resource_disabled(
    disabled_resources: Option<&ResourceStateSettings>,
    resource_type: PackageResourceType,
    name: &str,
    path: &Path,
) -> bool {
    let Some(disabled_resources) = disabled_resources else {
        return false;
    };
    let patterns = match resource_type {
        PackageResourceType::Extensions => &disabled_resources.extensions,
        PackageResourceType::Skills => &disabled_resources.skills,
        PackageResourceType::Prompts => &disabled_resources.prompts,
        PackageResourceType::Themes => &disabled_resources.themes,
    };
    let path = normalize_resource_path(path);
    let key = resource_type.key();
    let singular = resource_type.singular();
    patterns.iter().any(|pattern| {
        let pattern = normalize_pattern(pattern);
        wildcard_matches(&pattern, name)
            || wildcard_matches(&pattern, &format!("{key}/{name}"))
            || wildcard_matches(&pattern, &format!("{singular}:{name}"))
            || wildcard_matches(&pattern, &path)
    })
}

pub(crate) fn resolve_resource_path(base_dir: &Path, entry: &str) -> Result<PathBuf, ConfigError> {
    let path = expand_tilde(PathBuf::from(entry))?;
    Ok(if path.is_absolute() {
        path
    } else {
        base_dir.join(path)
    })
}

pub(crate) fn collect_resource_path(
    resources: &mut BTreeMap<String, ResourceFile>,
    path: &Path,
    diagnostics: &mut Vec<String>,
) {
    let mut paths = Vec::new();
    collect_resource_paths(&mut paths, path, diagnostics);
    for path in paths {
        push_resource_file(resources, &path, diagnostics);
    }
}

pub(crate) fn collect_resource_paths(
    paths: &mut Vec<PathBuf>,
    path: &Path,
    diagnostics: &mut Vec<String>,
) {
    if is_resource_metadata_file(path) {
        return;
    }
    if path.is_file() {
        paths.push(path.to_path_buf());
        return;
    }
    if !path.is_dir() {
        diagnostics.push(format!("resource path not found: {}", path.display()));
        return;
    }
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) => {
            diagnostics.push(format!("failed to read {}: {error}", path.display()));
            return;
        }
    };
    let ignore_patterns = read_resource_ignore_patterns(path, diagnostics);
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                diagnostics.push(format!(
                    "failed to read entry in {}: {error}",
                    path.display()
                ));
                continue;
            }
        };
        let child = entry.path();
        if resource_path_ignored(&child, path, &ignore_patterns) {
            continue;
        }
        if is_resource_metadata_file(&child) {
            continue;
        }
        if child.is_dir() {
            collect_resource_paths(paths, &child, diagnostics);
        } else if child.is_file() {
            paths.push(child);
        }
    }
}

pub(crate) fn apply_resource_patterns(
    paths: &[PathBuf],
    patterns: &[String],
    base_dir: &Path,
) -> Vec<PathBuf> {
    let mut includes = Vec::new();
    let mut excludes = Vec::new();
    let mut force_includes = Vec::new();
    let mut force_excludes = Vec::new();
    for pattern in patterns {
        if let Some(pattern) = pattern.strip_prefix('+') {
            force_includes.push(pattern);
        } else if let Some(pattern) = pattern.strip_prefix('-') {
            force_excludes.push(pattern);
        } else if let Some(pattern) = pattern.strip_prefix('!') {
            excludes.push(pattern);
        } else {
            includes.push(pattern.as_str());
        }
    }

    let mut selected = paths
        .iter()
        .filter(|path| {
            includes.is_empty()
                || includes
                    .iter()
                    .any(|pattern| resource_pattern_matches(path, pattern, base_dir))
        })
        .filter(|path| {
            !excludes
                .iter()
                .any(|pattern| resource_pattern_matches(path, pattern, base_dir))
        })
        .cloned()
        .collect::<Vec<_>>();

    for path in paths {
        if !selected.contains(path)
            && force_includes
                .iter()
                .any(|pattern| exact_resource_pattern_matches(path, pattern, base_dir))
        {
            selected.push(path.clone());
        }
    }
    selected.retain(|path| {
        !force_excludes
            .iter()
            .any(|pattern| exact_resource_pattern_matches(path, pattern, base_dir))
    });
    selected
}

pub(crate) fn resource_pattern_matches(path: &Path, pattern: &str, base_dir: &Path) -> bool {
    let pattern = normalize_pattern(pattern);
    let rel = normalize_resource_path(path.strip_prefix(base_dir).unwrap_or(path));
    let name = path
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or_default()
        .to_string();
    let full = normalize_resource_path(path);
    wildcard_matches(&pattern, &rel)
        || wildcard_matches(&pattern, &name)
        || wildcard_matches(&pattern, &full)
}

pub(crate) fn exact_resource_pattern_matches(path: &Path, pattern: &str, base_dir: &Path) -> bool {
    let pattern = normalize_pattern(pattern);
    let rel = normalize_resource_path(path.strip_prefix(base_dir).unwrap_or(path));
    let full = normalize_resource_path(path);
    pattern == rel || pattern == full
}

pub(crate) fn normalize_pattern(pattern: &str) -> String {
    pattern
        .strip_prefix("./")
        .unwrap_or(pattern)
        .replace('\\', "/")
}

pub(crate) fn normalize_resource_path(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

pub(crate) fn wildcard_matches(pattern: &str, value: &str) -> bool {
    let pattern = pattern.as_bytes();
    let value = value.as_bytes();
    let mut matched = vec![vec![false; value.len() + 1]; pattern.len() + 1];
    matched[0][0] = true;
    for index in 1..=pattern.len() {
        if pattern[index - 1] == b'*' {
            matched[index][0] = matched[index - 1][0];
        }
    }
    for pattern_index in 1..=pattern.len() {
        for value_index in 1..=value.len() {
            matched[pattern_index][value_index] = match pattern[pattern_index - 1] {
                b'*' => {
                    matched[pattern_index - 1][value_index]
                        || matched[pattern_index][value_index - 1]
                }
                b'?' => matched[pattern_index - 1][value_index - 1],
                byte => {
                    matched[pattern_index - 1][value_index - 1] && byte == value[value_index - 1]
                }
            };
        }
    }
    matched[pattern.len()][value.len()]
}

pub(crate) fn is_resource_metadata_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|value| value.to_str())
        .map(|name| {
            name.ends_with(".noel-extension.json")
                || name.ends_with(".pi-extension.json")
                || RESOURCE_IGNORE_FILES.contains(&name)
        })
        .unwrap_or(false)
}

pub(crate) fn read_resource_ignore_patterns(
    dir: &Path,
    diagnostics: &mut Vec<String>,
) -> Vec<String> {
    let mut patterns = Vec::new();
    for filename in RESOURCE_IGNORE_FILES {
        let path = dir.join(filename);
        if !path.exists() {
            continue;
        }
        match fs::read_to_string(&path) {
            Ok(content) => patterns.extend(content.lines().filter_map(parse_ignore_line)),
            Err(error) => diagnostics.push(format!("failed to read {}: {error}", path.display())),
        }
    }
    patterns
}

pub(crate) fn parse_ignore_line(line: &str) -> Option<String> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    Some(line.to_string())
}

pub(crate) fn resource_path_ignored(path: &Path, base_dir: &Path, patterns: &[String]) -> bool {
    let mut ignored = false;
    for pattern in patterns {
        let (negated, pattern) = pattern
            .strip_prefix('!')
            .map(|pattern| (true, pattern))
            .unwrap_or((false, pattern.as_str()));
        if resource_ignore_pattern_matches(path, base_dir, pattern) {
            ignored = !negated;
        }
    }
    ignored
}

pub(crate) fn resource_ignore_pattern_matches(path: &Path, base_dir: &Path, pattern: &str) -> bool {
    let is_dir = path.is_dir();
    let pattern = normalize_pattern(pattern);
    let directory_only = pattern.ends_with('/');
    if directory_only && !is_dir {
        return false;
    }
    let pattern = pattern.trim_end_matches('/');
    let rel = normalize_resource_path(path.strip_prefix(base_dir).unwrap_or(path));
    let name = path
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or_default()
        .to_string();
    if pattern.contains('/') {
        wildcard_matches(pattern, &rel)
    } else {
        wildcard_matches(pattern, &name)
    }
}

pub(crate) fn push_resource_file(
    resources: &mut BTreeMap<String, ResourceFile>,
    path: &Path,
    diagnostics: &mut Vec<String>,
) {
    let Some(name) = resource_name(path) else {
        return;
    };
    match fs::read_to_string(path) {
        Ok(content) => {
            let previous = resources.insert(
                name.clone(),
                ResourceFile {
                    name: name.clone(),
                    path: path.to_path_buf(),
                    content,
                },
            );
            if let Some(previous) = previous {
                diagnostics.push(format!(
                    "resource name collision for {name}: {} replaced by {}",
                    previous.path.display(),
                    path.display()
                ));
            }
        }
        Err(error) => diagnostics.push(format!("failed to read {}: {error}", path.display())),
    }
}

pub(crate) fn resource_name(path: &Path) -> Option<String> {
    path.file_stem()
        .and_then(|value| value.to_str())
        .map(ToString::to_string)
}
