//! `noel install`, `remove`, `update`, `list` and `config`: packages and their resource settings.

use super::*;

pub(crate) fn configured_package_sources(paths: &ConfigPaths) -> Result<Vec<String>> {
    let mut sources = BTreeSet::new();
    for source in read_settings_packages(&paths.settings_path)? {
        sources.insert(source.source().to_string());
    }
    for source in read_settings_packages(&paths.project_settings_path)? {
        sources.insert(source.source().to_string());
    }
    Ok(sources.into_iter().collect())
}

pub(crate) fn update_package_source(cwd: &Path, source: &str) -> Result<()> {
    if source.contains("://") || source.starts_with("git@") {
        println!("git source is recorded but not installed locally: {source}");
        return Ok(());
    }
    let path = resolve_package_source_path(cwd, source)?;
    if !path.exists() {
        return Err(anyhow!("package path not found: {}", path.display()));
    }
    if path.join(".git").is_dir() {
        let output = Command::new("git")
            .arg("-C")
            .arg(&path)
            .args(["pull", "--ff-only"])
            .output()?;
        if !output.status.success() {
            return Err(anyhow!(
                "failed to update package {}:\n{}{}",
                path.display(),
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        let details = String::from_utf8_lossy(&output.stdout);
        let details = details.trim();
        if details.is_empty() {
            println!("updated package {} from git", path.display());
        } else {
            println!("updated package {} from git: {details}", path.display());
        }
    } else {
        println!(
            "local package {} is not a git repository; resources reload at startup",
            path.display()
        );
    }
    Ok(())
}

pub(crate) fn resolve_package_source_path(cwd: &Path, source: &str) -> Result<PathBuf> {
    let path = if let Some(rest) = source.strip_prefix("~/") {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| anyhow!("home directory is not available"))?
            .join(rest)
    } else {
        PathBuf::from(source)
    };
    Ok(if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    })
}

pub(crate) fn run_package_list(args: &[String]) -> Result<()> {
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        print_package_help("list");
        return Ok(());
    }
    if let Some(invalid) = args.first() {
        return Err(anyhow!("unknown list argument: {invalid}"));
    }
    let cwd = std::env::current_dir()?;
    let paths = config_paths(cwd, None)?;
    print_package_sources("user", &paths.settings_path)?;
    print_package_sources("project", &paths.project_settings_path)?;
    Ok(())
}

pub(crate) fn run_package_config(args: &[String]) -> Result<()> {
    let (local, rest) = parse_package_scope_args(args)?;
    if rest.iter().any(|arg| arg == "-h" || arg == "--help") {
        print_package_help("config");
        return Ok(());
    }
    match rest.as_slice() {
        [] => print_package_config_summary(),
        [summary] if summary == "show" || summary == "list" => print_package_config_summary(),
        [action, kind, name] if action == "disable" || action == "enable" => {
            let kind = normalize_resource_kind(kind)?;
            let path = package_settings_path(local)?;
            let disabled = action == "disable";
            mutate_settings_resource_state(&path, kind, name, disabled)?;
            println!(
                "{} {kind} resource in {} settings: {name}",
                if disabled { "disabled" } else { "enabled" },
                if local { "project" } else { "user" }
            );
            Ok(())
        }
        [action, ..] if action == "disable" || action == "enable" => Err(anyhow!(
            "usage: noel config {action} <extension|skill|prompt|theme> <name> [-l]"
        )),
        [invalid, ..] => Err(anyhow!("unknown config argument: {invalid}")),
    }
}

pub(crate) fn print_package_config_summary() -> Result<()> {
    let cwd = std::env::current_dir()?;
    let paths = config_paths(cwd, None)?;
    let config = load_config(paths)?;
    println!("agent dir: {}", config.paths.agent_dir.display());
    println!(
        "project settings: {}",
        config.paths.project_settings_path.display()
    );
    println!("user settings: {}", config.paths.settings_path.display());
    println!(
        "packages: {}",
        if config.settings.packages.is_empty() {
            "-".to_string()
        } else {
            config
                .settings
                .packages
                .iter()
                .map(|package| package.source())
                .collect::<Vec<_>>()
                .join(", ")
        }
    );
    println!("skills: {}", config.skills.len());
    println!("prompts: {}", config.prompt_templates.len());
    println!("themes: {}", config.themes.len());
    println!("extensions: {}", config.extensions.len());
    println!(
        "disabled extensions: {}",
        format_disabled_resource_patterns(
            config
                .settings
                .disabled_resources
                .as_ref()
                .map(|resources| resources.extensions.as_slice())
        )
    );
    println!(
        "disabled skills: {}",
        format_disabled_resource_patterns(
            config
                .settings
                .disabled_resources
                .as_ref()
                .map(|resources| resources.skills.as_slice())
        )
    );
    println!(
        "disabled prompts: {}",
        format_disabled_resource_patterns(
            config
                .settings
                .disabled_resources
                .as_ref()
                .map(|resources| resources.prompts.as_slice())
        )
    );
    println!(
        "disabled themes: {}",
        format_disabled_resource_patterns(
            config
                .settings
                .disabled_resources
                .as_ref()
                .map(|resources| resources.themes.as_slice())
        )
    );
    Ok(())
}

pub(crate) fn format_disabled_resource_patterns(patterns: Option<&[String]>) -> String {
    patterns
        .filter(|patterns| !patterns.is_empty())
        .map(|patterns| patterns.join(", "))
        .unwrap_or_else(|| "-".to_string())
}

pub(crate) fn parse_package_scope_args(args: &[String]) -> Result<(bool, Vec<String>)> {
    let mut local = false;
    let mut rest = Vec::new();
    for arg in args {
        match arg.as_str() {
            "-l" | "--local" => local = true,
            _ if arg.starts_with('-') && arg != "-h" && arg != "--help" => {
                return Err(anyhow!("unknown package option: {arg}"));
            }
            _ => rest.push(arg.clone()),
        }
    }
    Ok((local, rest))
}

pub(crate) fn single_package_source(command: &str, args: &[String]) -> Result<String> {
    match args {
        [source] => Ok(source.clone()),
        [] => Err(anyhow!("usage: noel {command} <source> [-l]")),
        [_, extra, ..] => Err(anyhow!("unexpected package argument: {extra}")),
    }
}

pub(crate) fn package_settings_path(local: bool) -> Result<PathBuf> {
    let cwd = std::env::current_dir()?;
    let paths = config_paths(cwd, None)?;
    Ok(if local {
        paths.project_settings_path
    } else {
        paths.settings_path
    })
}

pub(crate) fn mutate_settings_packages(
    path: &Path,
    mutate: impl FnOnce(&mut Vec<PackageSource>),
) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut settings = if path.exists() {
        serde_json::from_str::<serde_json::Value>(&fs::read_to_string(path)?)?
    } else {
        serde_json::json!({})
    };
    if !settings.is_object() {
        return Err(anyhow!(
            "settings file must contain a JSON object: {}",
            path.display()
        ));
    }
    let current = settings
        .get("packages")
        .and_then(serde_json::Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(|value| serde_json::from_value::<PackageSource>(value.clone()).ok())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let mut packages = current;
    mutate(&mut packages);
    packages.sort_by(|left, right| left.source().cmp(right.source()));
    settings["packages"] = serde_json::to_value(packages)?;
    write_file_atomic(
        path,
        format!("{}\n", serde_json::to_string_pretty(&settings)?).as_bytes(),
    )?;
    Ok(())
}

pub(crate) fn mutate_settings_resource_state(
    path: &Path,
    kind: &str,
    name: &str,
    disabled: bool,
) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut settings = if path.exists() {
        serde_json::from_str::<serde_json::Value>(&fs::read_to_string(path)?)?
    } else {
        serde_json::json!({})
    };
    if !settings.is_object() {
        return Err(anyhow!(
            "settings file must contain a JSON object: {}",
            path.display()
        ));
    }
    if !settings
        .get("disabledResources")
        .map(|value| value.is_object())
        .unwrap_or(false)
    {
        settings["disabledResources"] = serde_json::json!({});
    }
    let mut values = settings["disabledResources"]
        .get(kind)
        .and_then(serde_json::Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if disabled {
        if !values.iter().any(|value| value == name) {
            values.push(name.to_string());
        }
    } else {
        values.retain(|value| value != name);
    }
    values.sort();
    values.dedup();
    settings["disabledResources"][kind] = serde_json::to_value(values)?;
    fs::write(
        path,
        format!("{}\n", serde_json::to_string_pretty(&settings)?),
    )?;
    Ok(())
}

pub(crate) fn normalize_resource_kind(kind: &str) -> Result<&'static str> {
    match kind {
        "extension" | "extensions" => Ok("extensions"),
        "skill" | "skills" => Ok("skills"),
        "prompt" | "prompts" => Ok("prompts"),
        "theme" | "themes" => Ok("themes"),
        _ => Err(anyhow!(
            "unknown resource kind: {kind}; expected extension, skill, prompt, or theme"
        )),
    }
}

pub(crate) fn print_package_sources(scope: &str, path: &Path) -> Result<()> {
    let packages = read_settings_packages(path)?;
    if packages.is_empty() {
        println!("{scope}: no packages");
    } else {
        for package in packages {
            println!("{scope}: {}", package.source());
        }
    }
    Ok(())
}

pub(crate) fn read_settings_packages(path: &Path) -> Result<Vec<PackageSource>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let settings = serde_json::from_str::<Settings>(&fs::read_to_string(path)?)?;
    Ok(settings.packages)
}

pub(crate) fn default_api_key_env(provider: &str) -> Option<&'static str> {
    match provider {
        "anthropic" => Some("ANTHROPIC_API_KEY"),
        "openai" => Some("OPENAI_API_KEY"),
        "google" => Some("GEMINI_API_KEY"),
        "openrouter" => Some("OPENROUTER_API_KEY"),
        "mistral" => Some("MISTRAL_API_KEY"),
        "github-copilot" => Some("COPILOT_GITHUB_TOKEN"),
        "azure" | noel_config::LEGACY_AZURE_PROVIDER => Some("AZURE_OPENAI_API_KEY"),
        "cloudflare-ai-gateway" | "cloudflare-workers-ai" => Some("CLOUDFLARE_API_KEY"),
        "amazon-bedrock" => Some("AWS_BEARER_TOKEN_BEDROCK"),
        _ => None,
    }
}

pub(crate) fn print_package_help(command: &str) {
    match command {
        "install" => {
            println!("usage: noel install <source> [-l]\n\nRecord a package source in settings.")
        }
        "remove" => {
            println!("usage: noel remove <source> [-l]\n\nRemove a package source from settings.")
        }
        "update" => println!(
            "usage: noel update [source|self|noel]\n\nUpdate local git package sources without npm."
        ),
        "list" => {
            println!("usage: noel list\n\nList package sources from user and project settings.")
        }
        "config" => println!(
            "usage: noel config [show|list|disable <kind> <name>|enable <kind> <name>] [-l]\n\nShow active resource configuration or enable/disable resources."
        ),
        _ => {}
    }
}

pub(crate) fn print_auth_help(command: &str) {
    match command {
        "login" => println!(
            "usage: noel login <provider> [--account <name>] [--api-key <key|env:VAR|->]\n\nStore API-key auth in ~/.noel/auth.json. Without --api-key, noel starts the OAuth login flow for providers that support it (openai-codex, anthropic); other providers store env:<provider default> when that environment variable is present. Without --account, the account is \"default\"."
        ),
        "logout" => println!(
            "usage: noel logout <provider> [--account <name>]\n\nRemove stored provider auth. Without --account, removes the \"default\" account."
        ),
        "accounts" => println!(
            "usage: noel accounts status [--refresh]\n\nShow auth and quota status for every configured account. --refresh bypasses the 15-minute usage cache.\n\nAccount failover, in settings.json: {{ \"failover\": {{ \"enabled\": true }} }}\nWith two or more stored accounts for a provider, one sticky account carries its requests. Another takes over when the provider answers 429 before any output, until the time the provider gives for the limit to reset (else 5 minutes), or when the sticky account's 5h usage window is at failover.providers.<provider>.exhaustionThresholdPct (default 90) by the usage cache. A switch away from an account near its usage limit waits minSwitchIntervalMs (default 10000) after the last switch. What failover learns is kept in failover-state.json. Pinning an account with /account, or --api-key, turns failover off."
        ),
        _ => {}
    }
}
