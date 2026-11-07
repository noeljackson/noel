//! Credentials: auth.json and its lock, the accounts per provider, environment and login keys, and the Claude Code and Codex login files.

use super::*;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum AuthCredential {
    ApiKey {
        key: String,
    },
    #[serde(rename = "oauth", alias = "o_auth")]
    OAuth {
        #[serde(alias = "access")]
        access_token: String,
        #[serde(default, alias = "refresh")]
        refresh_token: Option<String>,
        expires: u64,
        #[serde(default, alias = "accountId")]
        account_id: Option<String>,
    },
}

pub const DEFAULT_ACCOUNT_NAME: &str = "default";

/// Whether `provider`'s requests can carry `credential`. Upstream keeps
/// credentials per provider, and each provider names what it takes:
/// `openai-codex` a ChatGPT login and no API key, `openai` an API key. A
/// stored credential of the other kind is no credential, so nothing offers
/// the provider's models on the strength of it.
///
/// Upstream's `openai` also has a ChatGPT sign-in of its own, whose tokens
/// its Responses API takes. noel has no such flow: a login stored under
/// `openai` is the Codex login `noel login` used to save there as well, which
/// that API does not take.
pub(crate) fn provider_takes(provider: &str, credential: &AuthCredential) -> bool {
    !matches!(
        (provider, credential),
        ("openai", AuthCredential::OAuth { .. }) | ("openai-codex", AuthCredential::ApiKey { .. })
    )
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuthData {
    pub providers: BTreeMap<String, BTreeMap<String, AuthCredential>>,
}

impl AuthData {
    /// The key `provider`'s accounts are stored under: its id, or for Azure
    /// the id this port used to use, when the file still holds that one
    /// (see [`canonical_provider`]). Either id names the same accounts.
    pub(crate) fn stored_key<'a>(&self, provider: &'a str) -> &'a str {
        let provider = canonical_provider(provider);
        if provider == noel_ai::AZURE_PROVIDER
            && !self.providers.contains_key(provider)
            && self.providers.contains_key(LEGACY_AZURE_PROVIDER)
        {
            return LEGACY_AZURE_PROVIDER;
        }
        provider
    }

    /// `provider`'s stored accounts, less those whose credential it does
    /// not take (see [`provider_takes`]).
    pub(crate) fn accounts<'a, 'p>(
        &'a self,
        provider: &'p str,
    ) -> impl Iterator<Item = (&'a str, &'a AuthCredential)> + 'p
    where
        'a: 'p,
    {
        self.providers
            .get(self.stored_key(provider))
            .into_iter()
            .flatten()
            .filter(move |(_, credential)| provider_takes(provider, credential))
            .map(|(account, credential)| (account.as_str(), credential))
    }

    pub fn accounts_for_provider(&self, provider: &str) -> Vec<String> {
        self.accounts(provider)
            .map(|(account, _)| account.to_string())
            .collect()
    }

    pub fn credential(&self, provider: &str, account: &str) -> Option<&AuthCredential> {
        self.accounts(provider)
            .find(|(name, _)| *name == account)
            .map(|(_, credential)| credential)
    }

    pub fn insert(&mut self, provider: &str, account: &str, credential: AuthCredential) {
        self.providers
            .entry(self.stored_key(provider).to_string())
            .or_default()
            .insert(account.to_string(), credential);
    }

    pub fn remove(&mut self, provider: &str, account: &str) -> Option<AuthCredential> {
        let key = self.stored_key(provider);
        let accounts = self.providers.get_mut(key)?;
        let removed = accounts.remove(account);
        if accounts.is_empty() {
            self.providers.remove(key);
        }
        removed
    }

    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }

    pub fn account_for_resolution(&self, provider: &str) -> Option<&str> {
        let accounts = self
            .accounts(provider)
            .map(|(account, _)| account)
            .collect::<Vec<_>>();
        if accounts.contains(&DEFAULT_ACCOUNT_NAME) {
            return Some(DEFAULT_ACCOUNT_NAME);
        }
        match accounts[..] {
            [only] => Some(only),
            _ => None,
        }
    }

    pub(crate) fn default_credential(&self, provider: &str) -> Option<&AuthCredential> {
        self.account_for_resolution(provider)
            .and_then(|account| self.credential(provider, account))
    }
}

impl From<BTreeMap<String, AuthCredential>> for AuthData {
    fn from(flat: BTreeMap<String, AuthCredential>) -> Self {
        flat.into_iter()
            .map(|(provider, credential)| {
                (
                    provider,
                    BTreeMap::from([(DEFAULT_ACCOUNT_NAME.to_string(), credential)]),
                )
            })
            .collect::<BTreeMap<_, _>>()
            .into()
    }
}

impl From<BTreeMap<String, BTreeMap<String, AuthCredential>>> for AuthData {
    fn from(providers: BTreeMap<String, BTreeMap<String, AuthCredential>>) -> Self {
        Self { providers }
    }
}

impl Serialize for AuthData {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.providers.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for AuthData {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum AuthDataFile {
            V2(BTreeMap<String, BTreeMap<String, AuthCredential>>),
            V1(BTreeMap<String, AuthCredential>),
        }
        match AuthDataFile::deserialize(deserializer)? {
            AuthDataFile::V2(providers) => Ok(providers.into()),
            AuthDataFile::V1(flat) => Ok(flat.into()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedAuth {
    ApiKey(String),
    ClaudeCodeOAuth {
        access_token: String,
        refresh_token: Option<String>,
        expires: u64,
    },
    ChatGptOAuth {
        access_token: String,
        refresh_token: Option<String>,
        expires: u64,
        account_id: Option<String>,
    },
}

impl ResolvedAuth {
    pub fn refresh_token(&self) -> Option<&str> {
        match self {
            ResolvedAuth::ClaudeCodeOAuth { refresh_token, .. }
            | ResolvedAuth::ChatGptOAuth { refresh_token, .. } => refresh_token.as_deref(),
            ResolvedAuth::ApiKey(_) => None,
        }
    }

    pub fn expires(&self) -> u64 {
        match self {
            ResolvedAuth::ClaudeCodeOAuth { expires, .. }
            | ResolvedAuth::ChatGptOAuth { expires, .. } => *expires,
            ResolvedAuth::ApiKey(_) => 0,
        }
    }
}

pub fn is_expired(expires: u64, now: u64) -> bool {
    expires != 0 && now >= expires
}

pub fn expires_within(expires: u64, now: u64, within_seconds: u64) -> bool {
    expires != 0 && now.saturating_add(within_seconds) >= expires
}

pub fn api_key_for_provider(
    auth: &AuthData,
    provider: &str,
    account: Option<&str>,
) -> Option<String> {
    match auth_for_provider(auth, provider, account) {
        Some(ResolvedAuth::ApiKey(api_key)) => Some(api_key),
        _ => None,
    }
}

pub fn auth_for_provider(
    auth: &AuthData,
    provider: &str,
    account: Option<&str>,
) -> Option<ResolvedAuth> {
    let credential = match account {
        Some(account) => auth.credential(provider, account),
        None => auth.default_credential(provider),
    };
    if let Some(credential) = credential {
        return match credential {
            AuthCredential::ApiKey { key } => Some(ResolvedAuth::ApiKey(resolve_config_value(key))),
            AuthCredential::OAuth {
                access_token,
                refresh_token,
                expires,
                account_id,
            } => Some(oauth_for_provider(
                provider,
                resolve_config_value(access_token),
                refresh_token.as_deref().map(resolve_config_value),
                *expires,
                account_id.clone(),
            )),
        };
    }
    env_auth(provider).or_else(|| login_auth(provider))
}

/// Whether `provider`'s models can be offered: it has a credential, and, for
/// the Cloudflare providers, the ids their URLs are built from. Upstream
/// counts neither on the key alone (`cloudflare-auth.ts`), and one key
/// serves both.
pub fn has_auth_for_provider(auth: &AuthData, provider: &str, account: Option<&str>) -> bool {
    let ids: &[&str] = match provider {
        "cloudflare-workers-ai" => &["CLOUDFLARE_ACCOUNT_ID"],
        "cloudflare-ai-gateway" => &["CLOUDFLARE_ACCOUNT_ID", "CLOUDFLARE_GATEWAY_ID"],
        _ => &[],
    };
    ids.iter().all(|name| read_non_empty_env(name).is_some())
        && auth_for_provider(auth, provider, account).is_some()
}

pub fn import_account_name(provider: &str) -> &'static str {
    match provider {
        "anthropic" => "claude-import",
        _ => "codex-import",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountSource {
    Stored,
    Environment,
    ImportedFile,
}

pub fn listed_accounts_for_provider(
    auth: &AuthData,
    provider: &str,
) -> Vec<(String, AccountSource)> {
    listed_accounts(
        auth,
        provider,
        env_auth(provider).is_some(),
        login_auth(provider).is_some(),
    )
}

pub(crate) fn listed_accounts(
    auth: &AuthData,
    provider: &str,
    env_present: bool,
    import_present: bool,
) -> Vec<(String, AccountSource)> {
    let mut accounts: Vec<(String, AccountSource)> = auth
        .accounts_for_provider(provider)
        .into_iter()
        .map(|account| (account, AccountSource::Stored))
        .collect();
    if env_present && !accounts.iter().any(|(account, _)| account == "env") {
        accounts.push(("env".to_string(), AccountSource::Environment));
    }
    if import_present {
        let import = import_account_name(provider);
        if !accounts.iter().any(|(account, _)| account == import) {
            accounts.push((import.to_string(), AccountSource::ImportedFile));
        }
    }
    accounts
}

/// How long an `auth.json` update waits for another noel process to finish its
/// own (a login, logout, or OAuth refresh).
pub const AUTH_LOCK_TIMEOUT: Duration = Duration::from_secs(30);

/// Cross-process advisory lock serializing every read-modify-write of
/// `auth.json`. The file itself is replaced by rename, so the lock lives in a
/// sibling `auth.json.flock` that is never replaced. Dropping the lock
/// releases it, and so does the process exiting.
#[derive(Debug)]
pub struct AuthFileLock {
    _file: fs::File,
}

/// `auth_path` with its links resolved: the file's own path when it
/// exists, else its directory's real path joined with its name.
pub(crate) fn real_auth_path(auth_path: &Path) -> PathBuf {
    if let Ok(real) = fs::canonicalize(auth_path) {
        return real;
    }
    match (auth_path.parent(), auth_path.file_name()) {
        (Some(parent), Some(name)) => fs::canonicalize(parent)
            .map(|parent| parent.join(name))
            .unwrap_or_else(|_| auth_path.to_path_buf()),
        _ => auth_path.to_path_buf(),
    }
}

impl AuthFileLock {
    /// Takes the lock unless another holder has it.
    pub fn try_acquire(auth_path: &Path) -> Result<Option<Self>, ConfigError> {
        // Every path to the same file shares one lock: a symlinked
        // `auth.json` locks its target (upstream 081a6e87).
        let real = real_auth_path(auth_path);
        let auth_path = real.as_path();
        let file_name = auth_path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "auth.json".to_string());
        let lock_path = auth_path.with_file_name(format!("{file_name}.flock"));
        if let Some(parent) = lock_path.parent() {
            create_private_dir_all(parent)?;
        }
        let mut options = fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let lock_error = |source| ConfigError::Write {
            path: lock_path.clone(),
            source,
        };
        let file = options.open(&lock_path).map_err(lock_error)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(fs::TryLockError::WouldBlock) => Ok(None),
            Err(fs::TryLockError::Error(source)) => Err(lock_error(source)),
        }
    }

    /// Waits up to [`AUTH_LOCK_TIMEOUT`] for the lock.
    pub fn acquire(auth_path: &Path) -> Result<Self, ConfigError> {
        let deadline = Instant::now() + AUTH_LOCK_TIMEOUT;
        loop {
            if let Some(lock) = Self::try_acquire(auth_path)? {
                return Ok(lock);
            }
            if Instant::now() >= deadline {
                return Err(ConfigError::Write {
                    path: auth_path.to_path_buf(),
                    source: std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "another noel process holds the auth.json lock",
                    ),
                });
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// Reads `auth.json` as it is on disk now; a missing file holds no
/// credentials.
pub fn read_auth_file(auth_path: &Path) -> Result<AuthData, ConfigError> {
    Ok(read_optional_json::<AuthData>(auth_path)?.unwrap_or_default())
}

/// Writes `auth.json` atomically (temporary file, then rename) and owner-only
/// (0600), creating its directory owner-only (0700) when missing. Callers
/// hold [`AuthFileLock`].
pub fn write_auth_file(auth_path: &Path, auth: &AuthData) -> Result<(), ConfigError> {
    if let Some(parent) = auth_path.parent() {
        create_private_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(auth).map_err(|source| ConfigError::Parse {
        path: auth_path.to_path_buf(),
        source,
    })?;
    write_file_atomic_with_mode(auth_path, format!("{content}\n").as_bytes(), Some(0o600))
}

/// Applies `mutate` to `auth.json` as it is now, under [`AuthFileLock`], and
/// writes the result back only when it changed. Logins and logouts go through
/// here and OAuth refreshes hold the same lock, so a change only touches its
/// own entry and never restores another process's stale copy.
pub fn update_auth_file<T>(
    auth_path: &Path,
    mutate: impl FnOnce(&mut AuthData) -> T,
) -> Result<T, ConfigError> {
    let _lock = AuthFileLock::acquire(auth_path)?;
    let mut auth = read_auth_file(auth_path)?;
    let before = auth.clone();
    let result = mutate(&mut auth);
    if auth != before {
        write_auth_file(auth_path, &auth)?;
    }
    Ok(result)
}

pub(crate) fn env_api_key(provider: &str) -> Option<String> {
    let names = match provider {
        "anthropic" => &["ANTHROPIC_API_KEY"][..],
        "amazon-bedrock" => &["AWS_BEARER_TOKEN_BEDROCK"][..],
        "ant-ling" => &["ANT_LING_API_KEY"][..],
        "azure" | LEGACY_AZURE_PROVIDER => &["AZURE_OPENAI_API_KEY"][..],
        "baseten" => &["BASETEN_API_KEY"][..],
        "cloudflare-ai-gateway" | "cloudflare-workers-ai" => &["CLOUDFLARE_API_KEY"][..],
        "cerebras" => &["CEREBRAS_API_KEY"][..],
        "deepseek" => &["DEEPSEEK_API_KEY"][..],
        "fireworks" => &["FIREWORKS_API_KEY"][..],
        "github-copilot" => &["COPILOT_GITHUB_TOKEN"][..],
        "google" => &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
        "google-vertex" => &["GOOGLE_CLOUD_API_KEY"][..],
        "groq" => &["GROQ_API_KEY"][..],
        "huggingface" => &["HF_TOKEN"][..],
        "kimi-coding" | "kimi-coding-openai" => &["KIMI_API_KEY"][..],
        "meta" => &["META_API_KEY"][..],
        "minimax" => &["MINIMAX_API_KEY"][..],
        "minimax-cn" => &["MINIMAX_CN_API_KEY"][..],
        "moonshotai" | "moonshotai-cn" => &["MOONSHOT_API_KEY"][..],
        "nvidia" => &["NVIDIA_API_KEY"][..],
        "openai" => &["OPENAI_API_KEY"],
        "opencode" | "opencode-go" => &["OPENCODE_API_KEY"][..],
        "openrouter" => &["OPENROUTER_API_KEY"],
        "mistral" => &["MISTRAL_API_KEY"],
        "qwen-token-plan" | "qwen-token-plan-individual" => &["QWEN_TOKEN_PLAN_API_KEY"][..],
        "qwen-token-plan-cn" => &["QWEN_TOKEN_PLAN_CN_API_KEY"][..],
        "together" => &["TOGETHER_API_KEY"][..],
        "vercel-ai-gateway" => &["AI_GATEWAY_API_KEY"][..],
        "xai" => &["XAI_API_KEY"][..],
        "xiaomi" => &["XIAOMI_API_KEY"][..],
        "xiaomi-token-plan-ams" => &["XIAOMI_TOKEN_PLAN_AMS_API_KEY"][..],
        "xiaomi-token-plan-cn" => &["XIAOMI_TOKEN_PLAN_CN_API_KEY"][..],
        "xiaomi-token-plan-sgp" => &["XIAOMI_TOKEN_PLAN_SGP_API_KEY"][..],
        "zai" | "zai-coding" | "zai-anthropic" => &["ZAI_API_KEY"][..],
        "zai-coding-cn" => &["ZAI_CODING_CN_API_KEY"][..],
        _ => &[],
    };
    names.iter().find_map(|name| env::var(name).ok())
}

pub(crate) fn env_auth(provider: &str) -> Option<ResolvedAuth> {
    match provider {
        "anthropic" => read_non_empty_env("ANTHROPIC_OAUTH_TOKEN")
            .or_else(|| read_non_empty_env("ANTHROPIC_AUTH_TOKEN"))
            .or_else(|| read_non_empty_env("CLAUDE_CODE_OAUTH_TOKEN"))
            .map(|access_token| ResolvedAuth::ClaudeCodeOAuth {
                access_token,
                refresh_token: None,
                expires: 0,
            })
            .or_else(|| env_api_key(provider).map(ResolvedAuth::ApiKey)),
        // An API key, under OpenAI's name for it or the Codex CLI's. A
        // ChatGPT login is `openai-codex`'s credential (see `provider_takes`).
        "openai" => env_api_key(provider)
            .or_else(|| read_non_empty_env("CODEX_API_KEY"))
            .map(ResolvedAuth::ApiKey),
        "openai-codex" => read_non_empty_env("CODEX_ACCESS_TOKEN").map(|access_token| {
            ResolvedAuth::ChatGptOAuth {
                access_token,
                refresh_token: None,
                expires: 0,
                account_id: read_non_empty_env("CHATGPT_ACCOUNT_ID"),
            }
        }),
        _ => env_api_key(provider).map(ResolvedAuth::ApiKey),
    }
}

pub(crate) fn login_auth(provider: &str) -> Option<ResolvedAuth> {
    // Tests never read the developer's Claude Code or Codex login files.
    if test_isolation() {
        return None;
    }
    match provider {
        "anthropic" => read_claude_code_oauth(),
        "openai-codex" => read_codex_chatgpt_oauth(),
        _ => None,
    }
}

pub(crate) fn oauth_for_provider(
    provider: &str,
    access_token: String,
    refresh_token: Option<String>,
    expires: u64,
    account_id: Option<String>,
) -> ResolvedAuth {
    match provider {
        "anthropic" => ResolvedAuth::ClaudeCodeOAuth {
            access_token,
            refresh_token,
            expires,
        },
        "openai-codex" => ResolvedAuth::ChatGptOAuth {
            access_token,
            refresh_token,
            expires,
            account_id,
        },
        _ => ResolvedAuth::ApiKey(access_token),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ClaudeCredentialsFile {
    claude_ai_oauth: Option<ClaudeAiOAuth>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ClaudeAiOAuth {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_at: Option<u64>,
}

pub(crate) fn read_claude_code_oauth() -> Option<ResolvedAuth> {
    let path = home_dir().ok()?.join(".claude").join(".credentials.json");
    read_claude_code_oauth_from_path(&path)
}

pub(crate) fn read_claude_code_oauth_from_path(path: &Path) -> Option<ResolvedAuth> {
    let credentials = read_optional_json::<ClaudeCredentialsFile>(path)
        .ok()
        .flatten()?;
    let oauth = credentials.claude_ai_oauth?;
    if oauth.access_token.trim().is_empty() {
        return None;
    }
    Some(ResolvedAuth::ClaudeCodeOAuth {
        access_token: oauth.access_token,
        refresh_token: non_empty(oauth.refresh_token),
        expires: oauth.expires_at.map(|millis| millis / 1000).unwrap_or(0),
    })
}

#[derive(Debug, Deserialize)]
pub(crate) struct CodexAuthFile {
    tokens: Option<CodexAuthTokens>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct CodexAuthTokens {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    account_id: Option<String>,
}

pub(crate) fn read_codex_chatgpt_oauth() -> Option<ResolvedAuth> {
    let path = home_dir().ok()?.join(".codex").join("auth.json");
    read_codex_chatgpt_oauth_from_path(&path)
}

pub(crate) fn read_codex_chatgpt_oauth_from_path(path: &Path) -> Option<ResolvedAuth> {
    let auth = read_optional_json::<CodexAuthFile>(path).ok().flatten()?;
    let tokens = auth.tokens?;
    if tokens.access_token.trim().is_empty() {
        return None;
    }
    let expires = jwt_exp_claim(&tokens.access_token).unwrap_or(0);
    Some(ResolvedAuth::ChatGptOAuth {
        access_token: tokens.access_token,
        refresh_token: non_empty(tokens.refresh_token),
        expires,
        account_id: tokens.account_id,
    })
}

pub fn jwt_exp_claim(token: &str) -> Option<u64> {
    let parts = token.split('.').collect::<Vec<_>>();
    if parts.len() != 3 {
        return None;
    }
    let payload = URL_SAFE_NO_PAD.decode(parts[1]).ok()?;
    let payload = serde_json::from_slice::<serde_json::Value>(&payload).ok()?;
    payload.get("exp").and_then(serde_json::Value::as_u64)
}
