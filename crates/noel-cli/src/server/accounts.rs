//! `noel.accounts` and `noel.settings`, served by `Server()` to the owner
//! (docs/client-tui-plan.md, slice 3): the server machine's provider
//! accounts and settings, for a client anywhere.
//!
//! - `noel.accounts`: `accounts` (`{providers, logins}`): each provider's
//!   stored accounts (`{name, kind, limitedUntil?}`) and the account
//!   failover last chose (`current`), and the logins under way. A login
//!   runs here and writes the auth file here, so credentials never leave
//!   the server; the client only relays what the person does:
//!   - `login({provider, account})` starts one and says what it needs:
//!     `device` (open `url`, enter `userCode`), `code` (open `url`, then
//!     paste the code it shows) or `key` (an API key);
//!   - `finishLogin({loginId, code?, key?})` hands that over and answers
//!     once the login is done;
//!   - `logout({provider, account})` removes an account.
//! - `noel.settings`: `settings` (`{path, values}`), the user settings file;
//!   `set({key, value})` writes one value (`key` a dotted path; `null`
//!   removes it). A session takes a change on `/reload`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use noel_chord::v2::host::Host;
use noel_chord::v2::object::{Cell, Implementation};
use noel_chord::v2::WireError;
use noel_config::{AuthCredential, LoadedConfig};
use serde_json::{json, Value};
use tokio::sync::{oneshot, watch};

use super::errors::{forbidden, refused};
use crate::{oauth_login, oauth_refresh};

/// Where the OAuth flows go: the providers' own endpoints, or a test's.
#[derive(Clone)]
pub(crate) struct Endpoints {
    pub codex_issuer: String,
    pub codex_token_url: String,
    pub claude_authorize_url: String,
    pub claude_token_url: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            codex_issuer: oauth_login::CODEX_ISSUER.to_owned(),
            codex_token_url: oauth_refresh::CODEX_TOKEN_URL.to_owned(),
            claude_authorize_url: oauth_login::CLAUDE_AUTHORIZE_URL.to_owned(),
            claude_token_url: oauth_refresh::CLAUDE_TOKEN_URL.to_owned(),
        }
    }
}

/// How a login ends: the account was stored (`true` when it replaced one),
/// or why not.
type Outcome = Option<Result<bool, String>>;

struct Login {
    provider: String,
    account: String,
    /// What the person hands over: a pasted code or an API key.
    input: Option<std::sync::mpsc::Sender<String>>,
    key: bool,
    done: watch::Receiver<Outcome>,
}

pub(crate) struct Accounts {
    auth_path: PathBuf,
    failover_path: PathBuf,
    endpoints: Endpoints,
    cell: Cell,
    logins: Mutex<HashMap<String, Login>>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Accounts {
    pub(crate) fn new(config: &LoadedConfig, endpoints: Endpoints) -> anyhow::Result<Arc<Self>> {
        let schema = noel_client::contracts::accounts().declaration.variants["owner"].state
            ["accounts"]
            .clone();
        let accounts = Arc::new(Self {
            auth_path: config.paths.auth_path.clone(),
            failover_path: config
                .paths
                .agent_dir
                .join(crate::account_failover::STATE_FILE),
            endpoints,
            cell: Cell::new(schema, json!({"providers": [], "logins": []}))?,
            logins: Mutex::default(),
        });
        accounts.publish();
        Ok(accounts)
    }

    /// The auth file and the logins, as the state shows them.
    fn publish(&self) {
        let auth = noel_config::read_auth_file(&self.auth_path).unwrap_or_default();
        let providers = auth
            .providers
            .iter()
            .map(|(provider, accounts)| {
                let saved = crate::account_failover::saved_state(&self.failover_path, provider);
                let accounts = accounts
                    .iter()
                    .map(|(name, credential)| {
                        let kind = match credential {
                            AuthCredential::ApiKey { .. } => "api_key",
                            AuthCredential::OAuth { .. } => "oauth",
                        };
                        let mut entry = json!({"name": name, "kind": kind});
                        if let Some(until) = saved.limited_until.get(name) {
                            entry["limitedUntil"] = json!(until);
                        }
                        entry
                    })
                    .collect::<Vec<_>>();
                let mut entry = json!({"provider": provider, "accounts": accounts});
                if let Some(current) = &saved.current {
                    entry["current"] = json!(current);
                }
                entry
            })
            .collect::<Vec<_>>();
        let logins = lock(&self.logins)
            .iter()
            .map(|(id, login)| {
                let mut entry = json!({
                    "loginId": id,
                    "provider": login.provider,
                    "account": login.account,
                    "status": "waiting",
                });
                match &*login.done.borrow() {
                    Some(Ok(_)) => entry["status"] = json!("done"),
                    Some(Err(error)) => {
                        entry["status"] = json!("failed");
                        entry["error"] = json!(error);
                    }
                    None => {}
                }
                entry
            })
            .collect::<Vec<_>>();
        let _ = self
            .cell
            .replace(json!({"providers": providers, "logins": logins}));
    }

    /// `login({provider, account})`.
    async fn login(self: &Arc<Self>, args: &Value) -> Result<Value, WireError> {
        let text = |key: &str| args.get(key).and_then(Value::as_str).unwrap_or_default();
        let (provider, account) = (text("provider").to_owned(), text("account").to_owned());
        if provider.is_empty() || account.trim().is_empty() {
            return Err(refused("login", "a provider and an account are needed"));
        }
        let id = super::new_request_id();
        let (done_tx, done) = watch::channel(None);
        let (shown_tx, shown) = oneshot::channel::<Result<Value, String>>();
        let mut login = Login {
            provider: provider.clone(),
            account: account.clone(),
            input: None,
            key: false,
            done,
        };
        let endpoints = self.endpoints.clone();
        let auth_path = self.auth_path.clone();
        let this = self.clone();
        let finished = move |outcome: Result<bool, String>| {
            let _ = done_tx.send(Some(outcome));
            this.publish();
        };
        match provider.as_str() {
            "openai-codex" => {
                tokio::spawn(async move {
                    let client = crate::login_http_client();
                    let mut shown_tx = Some(shown_tx);
                    let tokens = oauth_login::run_codex_device_login(
                        &client,
                        &endpoints.codex_issuer,
                        &endpoints.codex_token_url,
                        |device| {
                            if let Some(shown) = shown_tx.take() {
                                let _ = shown.send(Ok(json!({
                                    "kind": "device",
                                    "url": device.verification_url,
                                    "userCode": device.user_code,
                                })));
                            }
                        },
                    )
                    .await;
                    let outcome = tokens
                        .map_err(|error| error.to_string())
                        .and_then(|tokens| {
                            store_oauth(&auth_path, "openai-codex", &account, tokens)
                        });
                    if let (Some(shown), Err(error)) = (shown_tx.take(), &outcome) {
                        let _ = shown.send(Err(error.clone()));
                    }
                    finished(outcome);
                });
            }
            "anthropic" => {
                let (input, pasted) = std::sync::mpsc::channel::<String>();
                login.input = Some(input);
                // The flow reads the pasted code synchronously: it runs on a
                // thread of its own, which waits for `finishLogin`.
                std::thread::spawn(move || {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build();
                    let outcome = match runtime {
                        Err(error) => Err(error.to_string()),
                        Ok(runtime) => runtime.block_on(async {
                            let client = crate::login_http_client();
                            let mut shown_tx = Some(shown_tx);
                            let tokens = tokio::time::timeout(
                                oauth_login::LOGIN_TIMEOUT,
                                oauth_login::run_claude_pkce_login(
                                    &client,
                                    &endpoints.claude_authorize_url,
                                    &endpoints.claude_token_url,
                                    |url| {
                                        if let Some(shown) = shown_tx.take() {
                                            let _ =
                                                shown.send(Ok(json!({"kind": "code", "url": url})));
                                        }
                                        pasted
                                            .recv_timeout(oauth_login::LOGIN_TIMEOUT)
                                            .map_err(|_| anyhow::anyhow!("no code was pasted"))
                                    },
                                ),
                            )
                            .await
                            .map_err(|_| "login timed out after 15 minutes".to_owned())
                            .and_then(|tokens| tokens.map_err(|error| error.to_string()));
                            let outcome = tokens.and_then(|tokens| {
                                store_oauth(&auth_path, "anthropic", &account, tokens)
                            });
                            if let (Some(shown), Err(error)) = (shown_tx.take(), &outcome) {
                                let _ = shown.send(Err(error.clone()));
                            }
                            outcome
                        }),
                    };
                    finished(outcome);
                });
            }
            _ => {
                let (input, key) = std::sync::mpsc::channel::<String>();
                login.input = Some(input);
                login.key = true;
                let _ = shown_tx.send(Ok(json!({"kind": "key"})));
                tokio::task::spawn_blocking(move || {
                    let outcome = key
                        .recv_timeout(oauth_login::LOGIN_TIMEOUT)
                        .map_err(|_| "no key was given".to_owned())
                        .and_then(|key| store_key(&auth_path, &provider, &account, &key));
                    finished(outcome);
                });
            }
        }
        lock(&self.logins).insert(id.clone(), login);
        self.publish();
        let mut shown = shown
            .await
            .unwrap_or_else(|_| Err("the login ended".to_owned()))
            .map_err(|error| refused("login", error))?;
        shown["loginId"] = json!(id);
        Ok(shown)
    }

    /// `finishLogin({loginId, code?, key?})`: answers once the login ends.
    async fn finish_login(&self, args: &Value) -> Result<Value, WireError> {
        let id = args
            .get("loginId")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let (input, key, mut done, provider, account) = {
            let logins = lock(&self.logins);
            let login = logins
                .get(id)
                .ok_or_else(|| refused("login", format!("no login {id}")))?;
            (
                login.input.clone(),
                login.key,
                login.done.clone(),
                login.provider.clone(),
                login.account.clone(),
            )
        };
        let given = if key {
            args.get("key")
        } else {
            args.get("code")
        };
        if let (Some(input), Some(given)) = (input, given.and_then(Value::as_str)) {
            let _ = input.send(given.to_owned());
        }
        let outcome = loop {
            if let Some(outcome) = done.borrow().clone() {
                break outcome;
            }
            if done.changed().await.is_err() {
                break Err("the login ended".to_owned());
            }
        };
        lock(&self.logins).remove(id);
        self.publish();
        let replaced = outcome.map_err(|error| refused("login", error))?;
        Ok(json!({"provider": provider, "account": account, "replaced": replaced}))
    }

    /// `logout({provider, account})`.
    fn logout(&self, args: &Value) -> Result<Value, WireError> {
        let text = |key: &str| args.get(key).and_then(Value::as_str).unwrap_or_default();
        let removed = noel_config::update_auth_file(&self.auth_path, |auth| {
            auth.remove(text("provider"), text("account")).is_some()
        })
        .map_err(|error| refused("logout", error.to_string()))?;
        self.publish();
        Ok(json!({ "removed": removed }))
    }
}

fn store_oauth(
    auth_path: &std::path::Path,
    provider: &str,
    account: &str,
    tokens: oauth_login::LoginTokens,
) -> Result<bool, String> {
    noel_config::update_auth_file(auth_path, |auth| {
        let replaced = auth.credential(provider, account).is_some();
        auth.insert(
            provider,
            account,
            AuthCredential::OAuth {
                access_token: tokens.access_token,
                refresh_token: tokens.refresh_token,
                expires: tokens.expires,
                account_id: tokens.account_id,
            },
        );
        replaced
    })
    .map_err(|error| error.to_string())
}

fn store_key(
    auth_path: &std::path::Path,
    provider: &str,
    account: &str,
    key: &str,
) -> Result<bool, String> {
    let key = key.trim();
    if key.is_empty() {
        return Err("api key is empty".to_owned());
    }
    noel_config::update_auth_file(auth_path, |auth| {
        let replaced = auth.credential(provider, account).is_some();
        auth.insert(
            provider,
            account,
            AuthCredential::ApiKey {
                key: key.to_owned(),
            },
        );
        replaced
    })
    .map_err(|error| error.to_string())
}

fn owner(principal: &Value) -> bool {
    principal.get("kind").and_then(Value::as_str) == Some("owner")
}

pub(crate) fn serve_accounts(host: &Host, accounts: Arc<Accounts>) {
    host.serve(
        noel_client::contracts::accounts(),
        move |_input, principal| {
            if !owner(principal) {
                return Err(forbidden("noel.accounts is the owner's"));
            }
            // The auth file may have changed since (`noel auth login`).
            accounts.publish();
            let (login, finish, logout) = (accounts.clone(), accounts.clone(), accounts.clone());
            Ok(Implementation::new("owner")
                .state("accounts", &accounts.cell)
                .method("login", move |args, _ctx| {
                    let accounts = login.clone();
                    async move { accounts.login(&args).await }
                })
                .method("finishLogin", move |args, _ctx| {
                    let accounts = finish.clone();
                    async move { accounts.finish_login(&args).await }
                })
                .method("logout", move |args, _ctx| {
                    let accounts = logout.clone();
                    async move { accounts.logout(&args) }
                }))
        },
    );
}

// ─── noel.settings ──────────────────────────────────────────────────────────

pub(crate) struct Settings {
    path: PathBuf,
    cell: Cell,
}

impl Settings {
    pub(crate) fn new(config: &LoadedConfig) -> anyhow::Result<Arc<Self>> {
        let schema = noel_client::contracts::settings().declaration.variants["owner"].state
            ["settings"]
            .clone();
        let path = config.paths.settings_path.clone();
        let settings = Arc::new(Self {
            cell: Cell::new(
                schema,
                json!({"path": path.display().to_string(), "values": {}}),
            )?,
            path,
        });
        settings.publish();
        Ok(settings)
    }

    fn values(&self) -> Value {
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({}))
    }

    fn publish(&self) {
        let _ = self.cell.replace(json!({
            "path": self.path.display().to_string(),
            "values": self.values(),
        }));
    }

    /// `set({key, value})`.
    fn set(&self, args: &Value) -> Result<Value, WireError> {
        let key = args.get("key").and_then(Value::as_str).unwrap_or_default();
        let path = key.split('.').collect::<Vec<_>>();
        if path.iter().any(|part| part.is_empty()) {
            return Err(refused("key", format!("{key:?} is not a settings key")));
        }
        let value = args.get("value").cloned().unwrap_or(Value::Null);
        let mut values = self.values();
        set_path(&mut values, &path, value);
        let text = serde_json::to_string_pretty(&values).unwrap_or_default();
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(&self.path, format!("{text}\n"))
            .map_err(|error| refused("settings", format!("{}: {error}", self.path.display())))?;
        self.publish();
        Ok(json!({ "values": self.values() }))
    }
}

/// Sets the value at `path` in `values`, making objects on the way; `null`
/// removes it.
fn set_path(values: &mut Value, path: &[&str], value: Value) {
    let Some((last, parents)) = path.split_last() else {
        return;
    };
    let mut current = values;
    for key in parents {
        if !current.get(*key).is_some_and(Value::is_object) {
            if value.is_null() {
                return;
            }
            current[*key] = json!({});
        }
        current = current.get_mut(*key).expect("an object, just made");
    }
    match (current.as_object_mut(), value) {
        (Some(object), Value::Null) => {
            object.remove(*last);
        }
        (Some(object), value) => {
            object.insert((*last).to_owned(), value);
        }
        (None, _) => {}
    }
}

pub(crate) fn serve_settings(host: &Host, settings: Arc<Settings>) {
    host.serve(
        noel_client::contracts::settings(),
        move |_input, principal| {
            if !owner(principal) {
                return Err(forbidden("noel.settings is the owner's"));
            }
            settings.publish();
            let set = settings.clone();
            Ok(Implementation::new("owner")
                .state("settings", &settings.cell)
                .method("set", move |args, _ctx| {
                    let settings = set.clone();
                    async move { settings.set(&args) }
                }))
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oauth_login::tests::{fake_jwt, spawn_stub};

    fn accounts(name: &str, endpoints: Endpoints) -> (Arc<Accounts>, PathBuf) {
        let dir = std::env::temp_dir().join(format!("noel-accounts-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut config = crate::tests::minimal_test_config();
        config.paths.agent_dir = dir.clone();
        config.paths.auth_path = dir.join("auth.json");
        (Accounts::new(&config, endpoints).unwrap(), dir)
    }

    fn oauth_account(dir: &std::path::Path, provider: &str, account: &str) -> Option<String> {
        let auth = noel_config::read_auth_file(&dir.join("auth.json")).unwrap();
        match auth.credential(provider, account)? {
            AuthCredential::OAuth { access_token, .. } => Some(access_token.clone()),
            AuthCredential::ApiKey { .. } => None,
        }
    }

    /// A browser sign-in that ends in a pasted code: the server builds the
    /// URL, the client shows it and hands back the code, and the server
    /// exchanges it and stores the account. The token never leaves it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_pasted_code_login_is_relayed_and_stored_on_the_server() {
        let stub = spawn_stub(vec![(
            200,
            r#"{"access_token":"claude-access","refresh_token":"claude-refresh","expires_in":3600}"#,
        )]);
        let endpoints = Endpoints {
            claude_token_url: stub.url.clone(),
            ..Endpoints::default()
        };
        let (accounts, dir) = accounts("code", endpoints);
        let started = accounts
            .login(&json!({"provider": "anthropic", "account": "work"}))
            .await
            .unwrap();
        assert_eq!(started["kind"], "code");
        let url = started["url"].as_str().unwrap();
        let state = url
            .split(['?', '&'])
            .find_map(|part| part.strip_prefix("state="))
            .unwrap()
            .to_owned();
        assert_eq!(
            accounts.cell.value()["logins"][0]["status"],
            json!("waiting")
        );
        let done = accounts
            .finish_login(&json!({
                "loginId": started["loginId"],
                "code": format!("the-code#{state}"),
            }))
            .await
            .unwrap();
        assert_eq!(
            done,
            json!({"provider": "anthropic", "account": "work", "replaced": false})
        );
        assert_eq!(
            oauth_account(&dir, "anthropic", "work").as_deref(),
            Some("claude-access")
        );
        let request = stub.requests.lock().unwrap()[0].clone();
        assert!(request.contains("\"code\":\"the-code\""), "{request}");
        // Listed, with no secret, and the login is gone.
        let listed = accounts.cell.value();
        assert_eq!(
            listed["providers"][0]["accounts"][0],
            json!({"name": "work", "kind": "oauth"})
        );
        assert!(!listed.to_string().contains("claude-access"));
        assert_eq!(listed["logins"], json!([]));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A device-code sign-in: the client shows the code, and `finishLogin`
    /// answers once the server's polling has stored the account.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_device_code_login_shows_the_code_and_finishes_on_the_server() {
        let access_token = fake_jwt(
            r#"{"exp":4102444800,"https://api.openai.com/auth":{"chatgpt_account_id":"acct-xyz"}}"#,
        );
        let success = format!(
            r#"{{"id_token":"id","access_token":"{access_token}","refresh_token":"refresh-1"}}"#
        );
        let stub = spawn_stub(vec![
            (
                200,
                r#"{"device_auth_id":"dev-1","user_code":"ABCD","interval":1}"#,
            ),
            (
                200,
                r#"{"authorization_code":"code-1","code_challenge":"ch","code_verifier":"ver-1"}"#,
            ),
            (200, Box::leak(success.into_boxed_str())),
        ]);
        let endpoints = Endpoints {
            codex_issuer: stub.url.clone(),
            codex_token_url: stub.url.clone(),
            ..Endpoints::default()
        };
        let (accounts, dir) = accounts("device", endpoints);
        let started = accounts
            .login(&json!({"provider": "openai-codex", "account": "default"}))
            .await
            .unwrap();
        assert_eq!(started["kind"], "device");
        assert_eq!(started["userCode"], "ABCD");
        let done = accounts
            .finish_login(&json!({"loginId": started["loginId"]}))
            .await
            .unwrap();
        assert_eq!(done["replaced"], false);
        assert_eq!(
            oauth_account(&dir, "openai-codex", "default"),
            Some(access_token)
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_setting_is_set_at_its_path_and_null_removes_it() {
        let mut values = json!({"retry": {"enabled": false}});
        set_path(&mut values, &["retry", "maxRetries"], json!(3));
        set_path(&mut values, &["theme"], json!("dark"));
        assert_eq!(
            values,
            json!({"retry": {"enabled": false, "maxRetries": 3}, "theme": "dark"})
        );
        set_path(&mut values, &["retry", "enabled"], Value::Null);
        set_path(&mut values, &["missing", "key"], Value::Null);
        assert_eq!(values, json!({"retry": {"maxRetries": 3}, "theme": "dark"}));
    }
}
