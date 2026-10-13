//! Rubiks IDE account: GitHub sign-in through the Rubiks gateway, replacing
//! the zed.dev account. Uses the same GitHub OAuth app as the Rubiks portal
//! and Coder; its client secret stays on the gateway, never in this binary.
//!
//! Flow (same shape as Zed's native-app sign-in): generate an RSA keypair,
//! listen on a random 127.0.0.1 port, open the gateway's native-app sign-in
//! page, receive the session token encrypted to our public key, decrypt it,
//! and keep it in the system keychain.
//!
//! Once signed in, the gateway's company setup (`/api/ide/config`) is applied as
//! the organization settings layer. Its contents follow the user's portal role:
//! developers and admins get Prime Agent with its LLM keys, guests do not.
//! For them, Prime Agent is installed if missing, and git's HTTPS prompts for
//! github.com are answered with the GitHub access granted at sign-in.

use std::{sync::Arc, time::Duration};

use anyhow::{Context as _, Result, anyhow};
use credentials_provider::CredentialsProvider;
use fs::GitCredentials;
use futures::AsyncReadExt as _;
use gpui::{
    App, AppContext as _, AsyncApp, Context, Entity, Global, Task, UpdateGlobal as _, WeakEntity,
};
use http_client::{AsyncBody, HttpClient, HttpClientWithUrl, Request, StatusCode};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use settings::SettingsStore;
use url::Url;
use util::ResultExt as _;
use util::command::{Stdio, new_command};

pub const DEFAULT_RUBIKS_SERVER_URL: &str = "https://api.rubiksworkspace.com";

/// Where "your account" links go: the Rubiks Workspace site, not the API host.
pub const RUBIKS_ACCOUNT_URL: &str = "https://rubiksworkspace.com";

/// Prime Agent's official installer. Fixed here rather than taken from the
/// gateway, so the company setup cannot make the IDE run other commands.
const PRIME_AGENT_INSTALLER_URL: &str = "https://app.primeintellect.ai/prime-agent/install.sh";

/// How long to wait for the browser to come back after GitHub sign-in.
const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(300);

/// The Rubiks gateway URL. `RUBIKS_SERVER_URL` overrides it, for pointing a
/// development build at a local gateway.
pub fn rubiks_server_url() -> String {
    std::env::var("RUBIKS_SERVER_URL")
        .ok()
        .filter(|url| !url.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_RUBIKS_SERVER_URL.to_string())
        .trim_end_matches('/')
        .to_string()
}

/// The portal user, as returned by the gateway's `/api/auth/me`.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RubiksUser {
    pub id: String,
    pub username: String,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub avatar_url: Option<String>,
}

/// A GitHub repository the user can clone, from the gateway's `/api/ide/repos`.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RubiksRepository {
    pub full_name: String,
    pub clone_url: String,
    #[serde(default)]
    pub private: bool,
}

#[derive(Deserialize)]
struct CompanySetup {
    version: u64,
    settings: serde_json::Value,
    #[serde(default)]
    features: CompanyFeatures,
    #[serde(default)]
    warnings: Vec<String>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CompanyFeatures {
    #[serde(default)]
    prime_agent: bool,
}

#[derive(Deserialize)]
struct GitHubAccess {
    username: String,
    token: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RubiksAccountStatus {
    SignedOut,
    Authenticating,
    SignedIn(RubiksUser),
}

pub struct RubiksAccount {
    status: RubiksAccountStatus,
    token: Option<String>,
    http: Arc<HttpClientWithUrl>,
    credentials_provider: Arc<dyn CredentialsProvider>,
    company_setup: Option<Task<()>>,
}

struct GlobalRubiksAccount(Entity<RubiksAccount>);

impl Global for GlobalRubiksAccount {}

pub fn init(client: &Arc<crate::Client>, cx: &mut App) {
    let http = client.http_client();
    let credentials_provider = client.credentials_provider();
    let account = cx.new(|cx| {
        let mut account = RubiksAccount {
            status: RubiksAccountStatus::SignedOut,
            token: None,
            http,
            credentials_provider,
            company_setup: None,
        };
        account.restore(cx);
        account
    });
    cx.set_global(GlobalRubiksAccount(account));
}

#[derive(Debug)]
enum GatewayError {
    /// The gateway rejected the token (expired or signed out elsewhere).
    Unauthorized,
    Other(anyhow::Error),
}

impl From<GatewayError> for anyhow::Error {
    fn from(error: GatewayError) -> Self {
        match error {
            GatewayError::Unauthorized => {
                anyhow!("your Rubiks session has expired, sign in again")
            }
            GatewayError::Other(error) => error,
        }
    }
}

impl RubiksAccount {
    pub fn global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalRubiksAccount>()
            .map(|global| global.0.clone())
    }

    pub fn status(&self) -> &RubiksAccountStatus {
        &self.status
    }

    pub fn user(&self) -> Option<&RubiksUser> {
        match &self.status {
            RubiksAccountStatus::SignedIn(user) => Some(user),
            _ => None,
        }
    }

    /// Session token for calling Rubiks gateway APIs as `Authorization: Bearer`.
    pub fn token(&self) -> Option<&str> {
        self.token.as_deref()
    }

    fn set_status(&mut self, status: RubiksAccountStatus, cx: &mut Context<Self>) {
        if self.status != status {
            self.status = status;
            cx.notify();
        }
    }

    fn signed_in(&mut self, user: RubiksUser, token: String, cx: &mut Context<Self>) {
        self.token = Some(token);
        self.set_status(RubiksAccountStatus::SignedIn(user), cx);
        self.apply_company_setup(cx);
    }

    /// Fetch the company setup for this user's role and apply it. Runs at every
    /// launch, so a role changed in the portal takes effect on the next restart.
    fn apply_company_setup(&mut self, cx: &mut Context<Self>) {
        let Some(token) = self.token.clone() else {
            return;
        };
        let http = self.http.clone();
        self.company_setup = Some(cx.spawn(async move |_, cx| {
            let setup = match get_json::<CompanySetup>(&http, &token, "/api/ide/config").await {
                Ok(setup) => setup,
                Err(error) => {
                    log::error!(
                        "could not load the Rubiks company setup: {:#}",
                        anyhow::Error::from(error)
                    );
                    return;
                }
            };
            for warning in &setup.warnings {
                log::warn!("Rubiks company setup: {warning}");
            }
            let settings = setup.settings.to_string();
            cx.update(|cx| {
                SettingsStore::update_global(cx, |store, cx| {
                    store.set_organization_settings(Some(&settings), cx)
                })
            })
            .log_err();
            log::info!("applied Rubiks company setup v{}", setup.version);

            if setup.features.prime_agent {
                cx.background_spawn(install_prime_agent_if_missing())
                    .await
                    .log_err();
            }
        }));
    }

    /// The user's GitHub access from their Rubiks sign-in, for git over HTTPS.
    pub fn github_credentials(&self, cx: &App) -> Task<Result<GitCredentials>> {
        let Some(token) = self.token.clone() else {
            return Task::ready(Err(anyhow!("sign in to Rubiks to use your GitHub access")));
        };
        let http = self.http.clone();
        cx.background_spawn(async move {
            let access: GitHubAccess = get_json(&http, &token, "/api/ide/github-token").await?;
            Ok(GitCredentials {
                username: access.username,
                password: access.token,
            })
        })
    }

    /// Answers git's HTTPS credential prompt for github.com, or returns `None`
    /// for any other prompt (other hosts, SSH passphrases) or when signed out.
    pub fn answer_git_prompt(&self, prompt: &str, cx: &App) -> Option<Task<Result<String>>> {
        self.token.as_ref()?;
        let field = github_prompt_field(prompt)?;
        let credentials = self.github_credentials(cx);
        Some(cx.background_spawn(async move {
            let credentials = credentials.await?;
            Ok(match field {
                GitPromptField::Username => credentials.username,
                GitPromptField::Password => credentials.password,
            })
        }))
    }

    fn clear_company_setup(&mut self, cx: &mut Context<Self>) {
        self.company_setup = None;
        SettingsStore::update_global(cx, |store, cx| store.set_organization_settings(None, cx))
            .log_err();
    }

    /// GitHub repositories the signed-in user can clone. The gateway refuses
    /// guests, with a message saying so.
    pub fn repositories(&self, cx: &App) -> Task<Result<Vec<RubiksRepository>>> {
        let Some(token) = self.token.clone() else {
            return Task::ready(Err(anyhow!("sign in to Rubiks to see your repositories")));
        };
        let http = self.http.clone();
        cx.background_spawn(async move {
            Ok(get_json(&http, &token, "/api/ide/repos").await?)
        })
    }

    /// Restore a session saved in the keychain on a previous launch.
    fn restore(&mut self, cx: &mut Context<Self>) {
        let http = self.http.clone();
        let credentials_provider = self.credentials_provider.clone();
        cx.spawn(async move |this, cx| {
            let server_url = rubiks_server_url();
            let Some((_user_id, token)) = credentials_provider
                .read_credentials(&server_url, cx)
                .await
                .log_err()
                .flatten()
            else {
                return;
            };
            let Ok(token) = String::from_utf8(token) else {
                return;
            };
            match fetch_user(&http, &token).await {
                Ok(user) => {
                    this.update(cx, |this, cx| this.signed_in(user, token, cx))
                        .log_err();
                }
                Err(GatewayError::Unauthorized) => {
                    credentials_provider
                        .delete_credentials(&server_url, cx)
                        .await
                        .log_err();
                }
                // Offline or gateway unavailable: keep the saved session for next time.
                Err(GatewayError::Other(error)) => {
                    log::warn!("could not restore Rubiks session: {error:#}");
                }
            }
        })
        .detach();
    }

    pub fn sign_in(&mut self, cx: &mut Context<Self>) -> Task<Result<()>> {
        if matches!(
            self.status,
            RubiksAccountStatus::Authenticating | RubiksAccountStatus::SignedIn(_)
        ) {
            return Task::ready(Ok(()));
        }
        self.set_status(RubiksAccountStatus::Authenticating, cx);

        let http = self.http.clone();
        let credentials_provider = self.credentials_provider.clone();
        cx.spawn(async move |this, cx| {
            let result = authenticate(&http, &credentials_provider, cx).await;
            finish_sign_in(this, result, cx)
        })
    }

    pub fn sign_out(&mut self, cx: &mut Context<Self>) -> Task<()> {
        let token = self.token.take();
        self.set_status(RubiksAccountStatus::SignedOut, cx);
        self.clear_company_setup(cx);

        let http = self.http.clone();
        let credentials_provider = self.credentials_provider.clone();
        cx.spawn(async move |_, cx| {
            if let Some(token) = token {
                end_gateway_session(&http, &token).await.log_err();
            }
            credentials_provider
                .delete_credentials(&rubiks_server_url(), cx)
                .await
                .log_err();
        })
    }
}

#[derive(Debug, PartialEq)]
enum GitPromptField {
    Username,
    Password,
}

/// Which credential git is asking for, if the prompt is for exactly
/// https://github.com. Git prompts look like `Username for 'https://github.com': `.
fn github_prompt_field(prompt: &str) -> Option<GitPromptField> {
    let (kind, rest) = prompt.trim().split_once(" for '")?;
    let url = Url::parse(rest.strip_suffix("':")?).ok()?;
    if url.scheme() != "https" || url.host_str() != Some("github.com") || url.port().is_some() {
        return None;
    }
    match kind {
        "Username" => Some(GitPromptField::Username),
        "Password" => Some(GitPromptField::Password),
        _ => None,
    }
}

async fn install_prime_agent_if_missing() -> Result<()> {
    if cfg!(windows) {
        return Ok(());
    }
    let binary = util::paths::home_dir().join(".local/bin/prime-agent");
    if smol::fs::metadata(&binary).await.is_ok() {
        return Ok(());
    }
    log::info!("Prime Agent is not installed, installing it to {}", binary.display());
    let output = new_command("sh")
        .args([
            "-c",
            &format!("curl -fsSL {PRIME_AGENT_INSTALLER_URL} | sh"),
        ])
        .env("PRIME_AGENT_INSTALLER_NONINTERACTIVE", "1")
        .stdin(Stdio::null())
        .output()
        .await
        .context("failed to run the Prime Agent installer")?;
    anyhow::ensure!(
        output.status.success(),
        "the Prime Agent installer failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    log::info!("installed Prime Agent");
    Ok(())
}

fn finish_sign_in(
    this: WeakEntity<RubiksAccount>,
    result: Result<(RubiksUser, String)>,
    cx: &mut AsyncApp,
) -> Result<()> {
    match result {
        Ok((user, token)) => {
            this.update(cx, |this, cx| this.signed_in(user, token, cx))?;
            cx.update(|cx| cx.activate(true));
            Ok(())
        }
        Err(error) => {
            this.update(cx, |this, cx| {
                this.set_status(RubiksAccountStatus::SignedOut, cx)
            })
            .log_err();
            Err(error)
        }
    }
}

async fn authenticate(
    http: &Arc<HttpClientWithUrl>,
    credentials_provider: &Arc<dyn CredentialsProvider>,
    cx: &mut AsyncApp,
) -> Result<(RubiksUser, String)> {
    let server_url = rubiks_server_url();
    let token = sign_in_with_browser(&server_url, cx).await?;
    let user = fetch_user(http, &token).await.map_err(|error| match error {
        GatewayError::Unauthorized => anyhow!("the Rubiks gateway rejected the new session"),
        GatewayError::Other(error) => error,
    })?;
    credentials_provider
        .write_credentials(&server_url, &user.id, token.as_bytes(), cx)
        .await
        .context("failed to save the Rubiks session to the keychain")?;
    Ok((user, token))
}

async fn sign_in_with_browser(server_url: &str, cx: &mut AsyncApp) -> Result<String> {
    let (public_key, private_key) = rpc::auth::keypair().context("failed to generate keypair")?;
    let public_key = String::try_from(public_key).context("failed to serialize public key")?;

    let server = tiny_http::Server::http("127.0.0.1:0")
        .map_err(|error| anyhow!(error).context("failed to bind callback port"))?;
    let port = server
        .server_addr()
        .to_ip()
        .context("server not bound to a TCP address")?
        .port();

    let query = serde_urlencoded::to_string([
        ("native_app_port", port.to_string()),
        ("native_app_public_key", public_key),
    ])?;
    let sign_in_url = format!("{server_url}/api/public/native-app/signin?{query}");
    cx.update(|cx| cx.open_url(&sign_in_url));

    let signed_in_url = format!("{server_url}/api/public/native-app/signed-in");
    let encrypted_token = cx
        .background_spawn(async move {
            let attempts = SIGN_IN_TIMEOUT.as_secs();
            for _ in 0..attempts {
                let Some(request) = server.recv_timeout(Duration::from_secs(1))? else {
                    continue;
                };
                let url = Url::parse(&format!("http://127.0.0.1{}", request.url()))
                    .context("failed to parse sign-in callback url")?;
                let token = url
                    .query_pairs()
                    .find(|(key, _)| key == "access_token")
                    .map(|(_, value)| value.into_owned());
                let Some(token) = token else {
                    // Not the sign-in callback (for example a favicon request).
                    request.respond(tiny_http::Response::empty(404)).log_err();
                    continue;
                };
                request
                    .respond(
                        tiny_http::Response::empty(302).with_header(
                            tiny_http::Header::from_bytes(&b"Location"[..], signed_in_url.as_bytes())
                                .map_err(|()| anyhow!("invalid redirect header"))?,
                        ),
                    )
                    .context("failed to respond to sign-in callback")?;
                return Ok(token);
            }
            anyhow::bail!("sign-in timed out waiting for the browser")
        })
        .await?;

    private_key
        .decrypt_string(&encrypted_token)
        .context("failed to decrypt the Rubiks session token")
}

async fn fetch_user(
    http: &Arc<HttpClientWithUrl>,
    token: &str,
) -> std::result::Result<RubiksUser, GatewayError> {
    get_json(http, token, "/api/auth/me").await
}

/// GET a gateway API as the signed-in user. Error responses carry FastAPI's
/// `{"detail": ...}`, which is shown to the user as is.
async fn get_json<T: DeserializeOwned>(
    http: &Arc<HttpClientWithUrl>,
    token: &str,
    path: &str,
) -> std::result::Result<T, GatewayError> {
    let request = Request::get(format!("{}{path}", rubiks_server_url()))
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/json")
        .body(AsyncBody::default())
        .map_err(|error| GatewayError::Other(error.into()))?;
    let mut response = http.send(request).await.map_err(GatewayError::Other)?;
    if response.status() == StatusCode::UNAUTHORIZED {
        return Err(GatewayError::Unauthorized);
    }
    let mut body = String::new();
    response
        .body_mut()
        .read_to_string(&mut body)
        .await
        .map_err(|error| GatewayError::Other(error.into()))?;
    if !response.status().is_success() {
        #[derive(Deserialize)]
        struct ErrorBody {
            detail: String,
        }
        let message = serde_json::from_str::<ErrorBody>(&body)
            .map(|error| error.detail)
            .unwrap_or_else(|_| format!("{} {body}", response.status()));
        return Err(GatewayError::Other(anyhow!(message)));
    }
    serde_json::from_str(&body)
        .with_context(|| format!("unexpected {path} response"))
        .map_err(GatewayError::Other)
}

async fn end_gateway_session(http: &Arc<HttpClientWithUrl>, token: &str) -> Result<()> {
    let request = Request::post(format!("{}/api/auth/logout", rubiks_server_url()))
        .header("Authorization", format!("Bearer {token}"))
        .body(AsyncBody::default())?;
    http.send(request).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_github_prompt_field() {
        assert_eq!(
            github_prompt_field("Username for 'https://github.com': "),
            Some(GitPromptField::Username)
        );
        assert_eq!(
            github_prompt_field("Password for 'https://x-access-token@github.com': "),
            Some(GitPromptField::Password)
        );
        for prompt in [
            "Password for 'https://x@notgithub.com': ",
            "Password for 'https://x@github.com.evil.com': ",
            "Password for 'http://x@github.com': ",
            "Password for 'https://x@github.com:8443': ",
            "Username for 'https://gitlab.com': ",
            "Enter passphrase for key '/Users/me/.ssh/id_ed25519': ",
        ] {
            assert_eq!(github_prompt_field(prompt), None, "{prompt}");
        }
    }
}
