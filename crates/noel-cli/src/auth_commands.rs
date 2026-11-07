//! `noel auth` and `noel accounts`: logins, stored keys, and account status.

use super::*;

pub(crate) async fn run_auth_login(args: &[String]) -> Result<()> {
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        print_auth_help("login");
        return Ok(());
    }
    let mut provider = None;
    let mut account = None;
    let mut api_key = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--api-key" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return Err(anyhow!("--api-key requires a value"));
                };
                api_key = Some(value.clone());
            }
            "--account" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return Err(anyhow!("--account requires a value"));
                };
                account = Some(value.clone());
            }
            value if value.starts_with('-') => {
                return Err(anyhow!("unknown login option: {value}"))
            }
            value => {
                if provider.replace(value.to_string()).is_some() {
                    return Err(anyhow!("unexpected login argument: {value}"));
                }
            }
        }
        index += 1;
    }
    let provider = provider.ok_or_else(|| {
        anyhow!("usage: noel login <provider> [--account <name>] [--api-key <key|env:VAR|->]")
    })?;
    let account = account.unwrap_or_else(|| DEFAULT_ACCOUNT_NAME.to_string());
    let cwd = std::env::current_dir()?;
    // A ChatGPT login is `openai-codex`'s credential, and its only one; an
    // API key is `openai`'s. The paths are asked for only once the arguments
    // are known to be usable (see `config_paths`).
    match (provider.as_str(), &api_key) {
        ("openai-codex", None) => {
            return run_codex_login(&config_paths(cwd, None)?, &account).await
        }
        ("openai-codex", Some(_)) => {
            return Err(anyhow!(
                "openai-codex signs in with ChatGPT and takes no API key: run `noel login openai-codex`, or store the key with `noel login openai --api-key <key|env:VAR|->`"
            ))
        }
        ("anthropic", None) => {
            return run_claude_login(&config_paths(cwd, None)?, &account).await
        }
        _ => {}
    }
    let key = match api_key {
        Some(value) if value == "-" => {
            let mut input = String::new();
            io::stdin().read_to_string(&mut input)?;
            input.trim().to_string()
        }
        Some(value) => value,
        None => default_api_key_env(&provider)
            .and_then(|name| {
                std::env::var(name)
                    .ok()
                    .filter(|value| !value.trim().is_empty())
                    .map(|_| format!("env:{name}"))
            })
            .ok_or_else(|| {
                anyhow!(
                    "usage: noel login {provider} --api-key <key|env:VAR|-> (OAuth login is available for: openai-codex, anthropic)"
                )
            })?,
    };
    if key.trim().is_empty() {
        return Err(anyhow!("api key is empty"));
    }
    let paths = config_paths(cwd, None)?;
    noel_config::update_auth_file(&paths.auth_path, |auth| {
        auth.insert(&provider, &account, AuthCredential::ApiKey { key })
    })?;
    println!(
        "stored API-key auth for {provider}{} in {}",
        account_suffix(&account),
        paths.auth_path.display()
    );
    Ok(())
}

/// Stores an API key for `provider`/`account` (the tail of the CLI login).
pub(crate) fn run_store_api_key(
    auth_path: &Path,
    provider: &str,
    account: &str,
    key: &str,
) -> Result<()> {
    if key.trim().is_empty() {
        return Err(anyhow!("api key is empty"));
    }
    noel_config::update_auth_file(auth_path, |auth| {
        auth.insert(
            provider,
            account,
            AuthCredential::ApiKey {
                key: key.to_owned(),
            },
        )
    })?;
    println!(
        "stored API-key auth for {provider}{} in {}",
        account_suffix(account),
        auth_path.display()
    );
    Ok(())
}

pub(crate) fn account_suffix(account: &str) -> String {
    if account == DEFAULT_ACCOUNT_NAME {
        String::new()
    } else {
        format!(" (account {account})")
    }
}

/// Opens a URL in the platform browser, upstream's `openBrowser`: spawn
/// the platform launcher directly, never a shell (a URL with shell
/// metacharacters must not be able to inject). Best effort: a missing
/// launcher is not an error, the URL is printed for copy regardless.
pub(crate) fn open_browser(url: &str) {
    #[cfg(unix)]
    let (program, args): (&str, Vec<&str>) = if cfg!(target_os = "macos") {
        ("open", vec![url])
    } else {
        ("xdg-open", vec![url])
    };
    #[cfg(windows)]
    let (program, args): (&str, Vec<&str>) = ("rundll32", vec!["url.dll,FileProtocolHandler", url]);
    let _ = std::process::Command::new(program)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|child| {
            // The launcher must not hold noel's process group hostage: it
            // never becomes a zombie we wait on.
            let _ = child.id();
        });
}

pub(crate) fn login_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// The TUI's `/login`: suspends the TUI, runs the provider's real login
/// flow on the plain terminal (the CLI command's own flow, opened browser
/// included), then reloads the config so the new account is selectable at
/// once. Returns the line to show in the transcript.
pub(crate) async fn run_tui_login(
    surface: &mut TuiSurface<'_>,
    app: &mut TuiApp,
    config: &mut LoadedConfig,
    provider: &str,
    account: &str,
) -> Result<String> {
    let cwd = config.paths.cwd.clone();
    let auth_path = config.paths.auth_path.clone();
    let existing = noel_config::read_auth_file(&auth_path)
        .map(|auth| auth.credential(provider, account).is_some())
        .unwrap_or(false);
    let flow: std::pin::Pin<Box<dyn Future<Output = Result<String>> + '_>> = Box::pin(async {
        if existing {
            println!("account {account} of {provider} already has stored auth; logging in replaces it with whichever account the browser signs in as");
            let mut answer = String::new();
            print!("continue? [y/N] ");
            let _ = io::Write::flush(&mut io::stdout());
            io::stdin().read_line(&mut answer)?;
            if !answer.trim().eq_ignore_ascii_case("y") {
                return Ok("login cancelled".to_string());
            }
        }
        match provider {
            "openai-codex" => {
                run_codex_login_opened(&config_paths(cwd, None)?, account).await?;
            }
            "anthropic" => {
                run_claude_login_opened(&config_paths(cwd, None)?, account).await?;
            }
            _ => run_api_key_login_interactive(&auth_path, provider, account).await?,
        }
        Ok(format!("logged in {provider} as account {account}"))
    });
    let outcome = with_suspended_tui(surface, app, config, flow).await??;
    // The config reloads so the account appears in /account and the model
    // pickers without a restart, the same way /reload does.
    *config = reload_config(config)?;
    Ok(outcome)
}

/// [`run_codex_login`] with the browser opened on the device URL.
pub(crate) async fn run_codex_login_opened(paths: &ConfigPaths, account: &str) -> Result<()> {
    let client = login_http_client();
    let tokens = oauth_login::run_codex_device_login(
        &client,
        oauth_login::CODEX_ISSUER,
        oauth_refresh::CODEX_TOKEN_URL,
        |device| {
            open_browser(&device.verification_url);
            println!(
                "opening {} (copy it if no browser opened) and enter code {}",
                device.verification_url, device.user_code
            );
            println!("waiting for authorization (the code expires in 15 minutes)...");
        },
    )
    .await?;
    let replaced = noel_config::update_auth_file(&paths.auth_path, |auth| {
        let replaced = auth.credential("openai-codex", account).is_some();
        auth.insert(
            "openai-codex",
            account,
            AuthCredential::OAuth {
                access_token: tokens.access_token,
                refresh_token: tokens.refresh_token,
                expires: tokens.expires,
                account_id: tokens.account_id,
            },
        );
        replaced
    })?;
    println!(
        "logged in as account {account}{}",
        if replaced {
            " (replaced existing login)"
        } else {
            ""
        }
    );
    Ok(())
}

/// [`run_claude_login`] with the browser opened on the authorize URL.
pub(crate) async fn run_claude_login_opened(paths: &ConfigPaths, account: &str) -> Result<()> {
    let client = login_http_client();
    let tokens = tokio::time::timeout(
        oauth_login::LOGIN_TIMEOUT,
        oauth_login::run_claude_pkce_login(
            &client,
            oauth_login::CLAUDE_AUTHORIZE_URL,
            oauth_refresh::CLAUDE_TOKEN_URL,
            |url| {
                open_browser(url);
                println!("opening the sign-in page (copy it if no browser opened):");
                println!("{url}");
                println!("after signing in, paste the full code shown (code#state): ");
                let mut input = String::new();
                io::stdin().read_line(&mut input)?;
                Ok(input)
            },
        ),
    )
    .await
    .map_err(|_| anyhow!("login timed out after 15 minutes"))??;
    let replaced = noel_config::update_auth_file(&paths.auth_path, |auth| {
        let replaced = auth.credential("anthropic", account).is_some();
        auth.insert(
            "anthropic",
            account,
            AuthCredential::OAuth {
                access_token: tokens.access_token.clone(),
                refresh_token: tokens.refresh_token.clone(),
                expires: tokens.expires,
                account_id: tokens.account_id.clone(),
            },
        );
        replaced
    })?;
    println!(
        "logged in as account {account}{}",
        if replaced {
            " (replaced existing login)"
        } else {
            ""
        }
    );
    Ok(())
}

/// An API-key login read from the plain terminal (the `--api-key -` path).
pub(crate) async fn run_api_key_login_interactive(
    auth_path: &Path,
    provider: &str,
    account: &str,
) -> Result<()> {
    println!("paste the {provider} API key (or an empty line to cancel):");
    let mut key = String::new();
    io::stdin().read_line(&mut key)?;
    let key = key.trim();
    if key.is_empty() {
        return Err(anyhow!("login cancelled"));
    }
    run_store_api_key(auth_path, provider, account, key)
}

pub(crate) async fn run_codex_login(paths: &ConfigPaths, account: &str) -> Result<()> {
    let client = login_http_client();
    let tokens = oauth_login::run_codex_device_login(
        &client,
        oauth_login::CODEX_ISSUER,
        oauth_refresh::CODEX_TOKEN_URL,
        |device| {
            println!(
                "open {} in your browser and enter code {}",
                device.verification_url, device.user_code
            );
            println!("waiting for authorization (the code expires in 15 minutes)...");
        },
    )
    .await?;
    let replaced = noel_config::update_auth_file(&paths.auth_path, |auth| {
        let replaced = auth.credential("openai-codex", account).is_some();
        auth.insert(
            "openai-codex",
            account,
            AuthCredential::OAuth {
                access_token: tokens.access_token,
                refresh_token: tokens.refresh_token,
                expires: tokens.expires,
                account_id: tokens.account_id,
            },
        );
        replaced
    })?;
    println!(
        "logged in as account {account}{}",
        if replaced {
            " (replaced existing login)"
        } else {
            ""
        }
    );
    Ok(())
}

pub(crate) async fn run_claude_login(paths: &ConfigPaths, account: &str) -> Result<()> {
    let client = login_http_client();
    let tokens = tokio::time::timeout(
        oauth_login::LOGIN_TIMEOUT,
        oauth_login::run_claude_pkce_login(
            &client,
            oauth_login::CLAUDE_AUTHORIZE_URL,
            oauth_refresh::CLAUDE_TOKEN_URL,
            |url| {
                println!("open this URL and sign in:\n{url}");
                println!("after signing in, paste the full code shown (code#state): ");
                let mut input = String::new();
                io::stdin().read_line(&mut input)?;
                Ok(input)
            },
        ),
    )
    .await
    .map_err(|_| anyhow!("login timed out after 15 minutes"))??;
    let replaced = noel_config::update_auth_file(&paths.auth_path, |auth| {
        let replaced = auth.credential("anthropic", account).is_some();
        auth.insert(
            "anthropic",
            account,
            AuthCredential::OAuth {
                access_token: tokens.access_token.clone(),
                refresh_token: tokens.refresh_token.clone(),
                expires: tokens.expires,
                account_id: tokens.account_id.clone(),
            },
        );
        replaced
    })?;
    println!(
        "logged in as account {account}{}",
        if replaced {
            " (replaced existing login)"
        } else {
            ""
        }
    );
    Ok(())
}

pub(crate) fn run_auth_logout(args: &[String]) -> Result<()> {
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        print_auth_help("logout");
        return Ok(());
    }
    let mut provider = None;
    let mut account = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--account" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return Err(anyhow!("--account requires a value"));
                };
                account = Some(value.clone());
            }
            value if value.starts_with('-') => {
                return Err(anyhow!("unknown logout option: {value}"))
            }
            value => {
                if provider.replace(value.to_string()).is_some() {
                    return Err(anyhow!("unexpected logout argument: {value}"));
                }
            }
        }
        index += 1;
    }
    let provider =
        provider.ok_or_else(|| anyhow!("usage: noel logout <provider> [--account <name>]"))?;
    let account = account.unwrap_or_else(|| DEFAULT_ACCOUNT_NAME.to_string());
    let cwd = std::env::current_dir()?;
    let paths = config_paths(cwd, None)?;
    let removed = noel_config::update_auth_file(&paths.auth_path, |auth| {
        auth.remove(&provider, &account).is_some()
    })?;
    if removed {
        println!(
            "removed stored auth for {provider}{}",
            account_suffix(&account)
        );
    } else {
        println!("no stored auth for {provider}{}", account_suffix(&account));
    }
    Ok(())
}

pub(crate) async fn try_run_accounts_command() -> Result<bool> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.first().map(String::as_str) != Some("accounts") {
        return Ok(false);
    }
    run_accounts_status(&args[1..]).await?;
    Ok(true)
}

pub(crate) async fn run_accounts_status(args: &[String]) -> Result<()> {
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        print_auth_help("accounts");
        return Ok(());
    }
    let mut action = None;
    let mut refresh = false;
    for arg in args {
        match arg.as_str() {
            "status" => {
                if action.replace(()).is_some() {
                    return Err(anyhow!("unexpected accounts argument: status"));
                }
            }
            "--refresh" => refresh = true,
            value => return Err(anyhow!("unknown accounts argument: {value}")),
        }
    }
    if action.is_none() {
        return Err(anyhow!("usage: noel accounts status [--refresh]"));
    }
    let cwd = std::env::current_dir()?;
    let paths = config_paths(cwd, None)?;
    let config = load_config(paths)?;
    let now = unix_seconds().unwrap_or(0) as i64;
    let rows = usage::collect_account_status(&config, refresh, None, None, now).await;
    println!("{}", usage::format_account_status(&rows, now));
    Ok(())
}

pub(crate) fn run_package_install(args: &[String]) -> Result<()> {
    let (local, rest) = parse_package_scope_args(args)?;
    if rest.iter().any(|arg| arg == "-h" || arg == "--help") {
        print_package_help("install");
        return Ok(());
    }
    let source = single_package_source("install", &rest)?;
    let path = package_settings_path(local)?;
    mutate_settings_packages(&path, |packages| {
        if !packages.iter().any(|package| package.source() == source) {
            packages.push(PackageSource::Simple(source.clone()));
        }
    })?;
    println!(
        "recorded package source in {} settings: {source}",
        if local { "project" } else { "user" }
    );
    Ok(())
}

pub(crate) fn run_package_remove(args: &[String]) -> Result<()> {
    let (local, rest) = parse_package_scope_args(args)?;
    if rest.iter().any(|arg| arg == "-h" || arg == "--help") {
        print_package_help("remove");
        return Ok(());
    }
    let source = single_package_source("remove", &rest)?;
    let path = package_settings_path(local)?;
    let mut removed = false;
    mutate_settings_packages(&path, |packages| {
        let before = packages.len();
        packages.retain(|package| package.source() != source);
        removed = packages.len() != before;
    })?;
    if removed {
        println!(
            "removed package source from {} settings: {source}",
            if local { "project" } else { "user" }
        );
    } else {
        println!(
            "package source not present in {} settings: {source}",
            if local { "project" } else { "user" }
        );
    }
    Ok(())
}

pub(crate) fn run_package_update(args: &[String]) -> Result<()> {
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        print_package_help("update");
        return Ok(());
    }
    if let Some(invalid) = args.iter().find(|arg| arg.starts_with('-')) {
        return Err(anyhow!("unknown update option: {invalid}"));
    }
    if args.len() > 1 {
        return Err(anyhow!("usage: noel update [source|self|noel]"));
    }
    let cwd = std::env::current_dir()?;
    let paths = config_paths(cwd.clone(), None)?;
    let target = args.first().map(String::as_str).unwrap_or("all");
    let sources = if target == "all" {
        configured_package_sources(&paths)?
    } else if matches!(target, "self" | "noel") {
        println!("self update is not managed by the Rust no-npm package updater");
        return Ok(());
    } else {
        vec![target.to_string()]
    };
    if sources.is_empty() {
        println!("no package sources configured");
        return Ok(());
    }
    for source in sources {
        update_package_source(&cwd, &source)?;
    }
    Ok(())
}
