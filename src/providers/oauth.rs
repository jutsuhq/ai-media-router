//! OAuth account sign-in for providers whose `[auth] type = "oauth"`: dynamic client
//! registration (RFC 7591), the PKCE authorization-code flow over a loopback callback, and a
//! session kept alongside the provider keys. Tokens live in memory (or the session slot) and are
//! never logged.

use std::{
    path::{Path, PathBuf},
    sync::Mutex,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail, ensure};
use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::Url;
use rmcp::transport::auth::{
    AuthError, AuthorizationManager, AuthorizationMetadata, CredentialRefreshGuard, CredentialStore,
    OAuthClientConfig, OAuthTokenResponse, StoredCredentials,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::spec::{Auth, AuthKind, Spec};
use crate::config::{self, Store};

const PORTS: [u16; 3] = [41687, 53419, 60871];
const LOGIN_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const DEFAULT_SCOPES: &str = "openid email offline_access";

/// The compact form persisted in the session slot: base64url(no pad) of this JSON.
#[derive(Clone, Serialize, Deserialize)]
struct Session {
    client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    issuer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    access_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expires_at: Option<u64>,
    #[serde(default)]
    scopes: Vec<String>,
}

impl Session {
    fn has_token(&self) -> bool {
        self.refresh_token.is_some() || self.access_token.is_some()
    }

    fn to_stored_credentials(&self) -> StoredCredentials {
        StoredCredentials::new(self.client_id.clone(), token_response(self), self.scopes.clone(), token_received_at(self))
            .with_issuer(self.issuer.clone())
    }
}

/// `Debug` must not reveal a token.
impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("client_id", &self.client_id)
            .field("issuer", &self.issuer)
            .field("email", &self.email)
            .field("refresh_token", &self.refresh_token.as_ref().map(|_| "[redacted]"))
            .field("access_token", &self.access_token.as_ref().map(|_| "[redacted]"))
            .field("expires_at", &self.expires_at)
            .field("scopes", &self.scopes)
            .finish()
    }
}

fn encode(session: &Session) -> Result<String> {
    Ok(URL_SAFE_NO_PAD.encode(serde_json::to_vec(session)?))
}

fn decode(value: &str) -> Option<Session> {
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(value).ok()?).ok()
}

/// A token response with only the refresh token; `expires_in = 0` and `token_received_at = 0`
/// make rmcp refresh it before the first use.
fn token_response(session: &Session) -> Option<OAuthTokenResponse> {
    let mut obj = serde_json::Map::new();
    if let Some(refresh) = &session.refresh_token {
        obj.insert("access_token".into(), Value::String(String::new()));
        obj.insert("token_type".into(), Value::String("bearer".into()));
        obj.insert("expires_in".into(), Value::from(0u64));
        obj.insert("refresh_token".into(), Value::String(refresh.clone()));
        if !session.scopes.is_empty() {
            obj.insert("scope".into(), Value::String(session.scopes.join(" ")));
        }
    } else if let Some(access) = &session.access_token {
        obj.insert("access_token".into(), Value::String(access.clone()));
        obj.insert("token_type".into(), Value::String("bearer".into()));
        if let Some(expires_at) = session.expires_at {
            obj.insert("expires_in".into(), Value::from(expires_at.saturating_sub(crate::db::now() as u64)));
        }
    } else {
        return None;
    }
    serde_json::from_value(Value::Object(obj)).ok()
}

fn token_received_at(session: &Session) -> Option<u64> {
    if session.refresh_token.is_some() {
        Some(0)
    } else if session.access_token.is_some() {
        Some(crate::db::now() as u64)
    } else {
        None
    }
}

/// The session for a token exchange result, keeping the email already stored when the new token
/// response carries no `id_token` (a refresh).
fn from_credentials(credentials: &StoredCredentials, existing_email: Option<String>) -> Session {
    let token = credentials.token_response.as_ref();
    let email = token.and_then(id_token_email).or(existing_email);
    let fields = token.and_then(|t| serde_json::to_value(t).ok()).unwrap_or_default();
    let text = |k: &str| fields.get(k).and_then(Value::as_str).filter(|v| !v.is_empty()).map(str::to_owned);
    let refresh = text("refresh_token");
    let (access, expires_at) = match refresh {
        Some(_) => (None, None),
        None => (text("access_token"), fields.get("expires_in").and_then(Value::as_u64).map(|s| crate::db::now() as u64 + s)),
    };
    Session {
        client_id: credentials.client_id.clone(),
        issuer: credentials.issuer.clone(),
        email,
        refresh_token: refresh,
        access_token: access,
        expires_at,
        scopes: credentials.granted_scopes.clone(),
    }
}

/// The email claim of an `id_token` (base64url JSON payload); the signature is not verified.
fn id_token_email(token: &OAuthTokenResponse) -> Option<String> {
    id_token_email_from_jwt(token.extra_fields().0.get("id_token")?.as_str()?)
}

fn id_token_email_from_jwt(id_token: &str) -> Option<String> {
    let payload = id_token.split('.').nth(1)?;
    let claims: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).ok()?).ok()?;
    claims.get("email")?.as_str().map(str::to_owned)
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Account {
    pub email: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Status {
    pub signed_in: bool,
    pub email: Option<String>,
}

pub fn status(value: Option<&str>) -> Status {
    let session = value.and_then(decode);
    Status { signed_in: session.as_ref().is_some_and(Session::has_token), email: session.and_then(|s| s.email) }
}

/// Whether a stored value decodes to a session that still holds a token.
pub(crate) fn holds_token(value: &str) -> bool {
    decode(value).is_some_and(|s| s.has_token())
}

/// The signed-in email, for auth placeholders.
pub(crate) fn email(value: &str) -> Option<String> {
    decode(value).and_then(|s| s.email)
}

/// The decoded refresh/access tokens of a session, for redaction.
pub(crate) fn secrets(home: &Path, name: &str) -> Vec<String> {
    let Some(value) = config::get_one(home, name) else { return Vec::new() };
    let Some(session) = decode(&value) else { return Vec::new() };
    [session.refresh_token, session.access_token].into_iter().flatten().filter(|t| t.len() >= 6).collect()
}

fn oauth_name(spec: &Spec) -> Result<&str> {
    ensure!(
        spec.auth.kind == AuthKind::Oauth,
        "`{}` has no [auth] type = \"oauth\": its credentials are set with `keys set`",
        spec.key
    );
    spec.auth.env.first().map(String::as_str).ok_or_else(|| anyhow!("`{}` oauth auth needs an env slot", spec.key))
}

#[allow(dead_code)] // used by MCP tool steps, which land next
fn auth_name(auth: &Auth) -> Result<&str> {
    auth.env.first().map(String::as_str).ok_or_else(|| anyhow!("oauth auth needs an env slot"))
}

/// The session slot for a provider: `[env]` name the spec declares in `[auth] env`.
fn resource_url(spec: &Spec) -> String {
    spec.auth.resource.as_deref().unwrap_or(&spec.base_url).to_owned()
}

/// A credential store backed by the session slot, caching the credentials in memory.
pub struct SessionStore {
    home: PathBuf,
    name: String,
    store: Store,
    memory: Mutex<Option<StoredCredentials>>,
}

impl SessionStore {
    pub fn new(home: &Path, name: &str, store: Store) -> Self {
        Self { home: home.to_owned(), name: name.to_owned(), store, memory: Mutex::new(None) }
    }

    fn current_email(&self) -> Option<String> {
        config::get_one(&self.home, &self.name).and_then(|v| decode(&v)).and_then(|s| s.email)
    }

    fn persist(&self, session: &Session) -> Result<(), AuthError> {
        let value = encode(session).map_err(|e| AuthError::CredentialStoreError(e.to_string()))?;
        config::set_key(&self.home, &self.name, &value, self.store)
            .map_err(|e| AuthError::CredentialStoreError(format!("{e:#}")))?;
        Ok(())
    }

    fn cached(&self) -> Option<StoredCredentials> {
        self.memory.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

#[async_trait]
impl CredentialStore for SessionStore {
    async fn load(&self) -> Result<Option<StoredCredentials>, AuthError> {
        if let Some(creds) = self.cached() {
            return Ok(Some(creds));
        }
        let Some(value) = config::get_one(&self.home, &self.name) else { return Ok(None) };
        let Some(session) = decode(&value) else { return Ok(None) };
        let creds = session.to_stored_credentials();
        *self.memory.lock().unwrap_or_else(|e| e.into_inner()) = Some(creds.clone());
        Ok(Some(creds))
    }

    async fn save(&self, credentials: StoredCredentials) -> Result<(), AuthError> {
        let session = from_credentials(&credentials, self.current_email());
        self.persist(&session)?;
        *self.memory.lock().unwrap_or_else(|e| e.into_inner()) = Some(credentials);
        Ok(())
    }

    async fn clear(&self) -> Result<(), AuthError> {
        let Some(session) = config::get_one(&self.home, &self.name).and_then(|v| decode(&v)) else {
            return Ok(());
        };
        let cleared = Session {
            client_id: session.client_id,
            issuer: session.issuer,
            email: None,
            refresh_token: None,
            access_token: None,
            expires_at: None,
            scopes: Vec::new(),
        };
        self.persist(&cleared)?;
        *self.memory.lock().unwrap_or_else(|e| e.into_inner()) = None;
        Ok(())
    }

    async fn acquire_refresh_guard(&self) -> Result<Option<CredentialRefreshGuard>, AuthError> {
        // Re-read storage first: another process may have rotated the refresh token.
        if let Some(value) = config::get_one(&self.home, &self.name)
            && let Some(session) = decode(&value)
        {
            *self.memory.lock().unwrap_or_else(|e| e.into_inner()) = Some(session.to_stored_credentials());
        }
        Ok(None)
    }
}

/// Builds a manager with the store, resolves and https-checks metadata, and sets it.
async fn resolve(home: &Path, name: &str, resource: &str, store: Store) -> Result<(AuthorizationManager, AuthorizationMetadata)> {
    let mut manager =
        AuthorizationManager::new(resource).await.map_err(|e| anyhow!("{resource}: cannot start OAuth: {e}"))?;
    manager.set_credential_store(SessionStore::new(home, name, store));
    let resolution =
        manager.resolve_metadata().await.map_err(|e| anyhow!("{resource}: OAuth metadata discovery failed: {e}"))?;
    check_endpoints(&resolution.metadata, resource)?;
    let metadata = resolution.metadata.clone();
    manager.set_metadata(resolution.metadata);
    Ok((manager, metadata))
}

/// A manager for part 2 (MCP transport), already signed in.
#[allow(dead_code)] // used by MCP tool steps, which land next
pub(crate) async fn manager(home: &Path, key: &str, auth: &Auth, base_url: &str) -> Result<AuthorizationManager> {
    let name = auth_name(auth)?;
    let resource = auth.resource.as_deref().unwrap_or(base_url);
    let (mut manager, _) = resolve(home, name, resource, config::current_store(home, name)).await?;
    if !manager.initialize_from_store().await.map_err(|e| anyhow!("OAuth session: {e}"))? {
        bail!("not signed in: run `ai-media-router login {key}`");
    }
    Ok(manager)
}

/// `authorization_endpoint`, `token_endpoint` and `registration_endpoint` must be https; http is
/// allowed only when the resource itself is a loopback http URL and the endpoint host is loopback.
fn check_endpoints(metadata: &AuthorizationMetadata, resource: &str) -> Result<()> {
    let resource = Url::parse(resource).context("invalid resource URL")?;
    let endpoints = [
        ("authorization_endpoint", Some(metadata.authorization_endpoint.as_str())),
        ("token_endpoint", Some(metadata.token_endpoint.as_str())),
        ("registration_endpoint", metadata.registration_endpoint.as_deref()),
    ];
    for (field, endpoint) in endpoints {
        let Some(endpoint) = endpoint else { continue };
        let url = Url::parse(endpoint).with_context(|| format!("{field} `{endpoint}` is not a valid URL"))?;
        if !endpoint_allowed(&url, &resource) {
            bail!("{field} `{endpoint}` must be https (http is only allowed on loopback for a loopback resource)");
        }
    }
    Ok(())
}

fn endpoint_allowed(endpoint: &Url, resource: &Url) -> bool {
    endpoint.scheme() == "https"
        || (endpoint.scheme() == "http" && is_loopback_http(resource) && is_loopback_host(endpoint))
}

fn is_loopback_http(url: &Url) -> bool {
    url.scheme() == "http" && is_loopback_host(url)
}

fn is_loopback_host(url: &Url) -> bool {
    url.host_str()
        .is_some_and(|h| matches!(h, "127.0.0.1" | "localhost" | "::1") || h.ends_with(".localhost"))
}

/// Reuses the stored client_id when its issuer matches; otherwise registers one (RFC 7591).
async fn client_id(home: &Path, name: &str, metadata: &AuthorizationMetadata, http: &reqwest::Client) -> Result<String> {
    if let Some(value) = config::get_one(home, name)
        && let Some(session) = decode(&value)
        && session.issuer.as_deref() == metadata.issuer.as_deref()
    {
        return Ok(session.client_id);
    }
    let endpoint = metadata.registration_endpoint.as_deref().ok_or_else(|| {
        anyhow!("{name}: the authorization server has no registration endpoint; provide a pre-registered client_id in the session")
    })?;
    let redirect_uris: Vec<String> = PORTS.iter().map(|p| format!("http://127.0.0.1:{p}/callback")).collect();
    let body = json!({
        "client_name": "AI Media Router",
        "redirect_uris": redirect_uris,
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
        "application_type": "native",
    });
    let resp = http.post(endpoint).json(&body).send().await.with_context(|| format!("{name}: cannot reach {endpoint}"))?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        bail!("{name}: client registration at `{endpoint}` returned HTTP {status}: {text}");
    }
    #[derive(Deserialize)]
    struct Registration {
        client_id: String,
    }
    let reg: Registration = resp.json().await.with_context(|| format!("{name}: registration response is not JSON"))?;
    Ok(reg.client_id)
}

/// Sign the user in and persist the session; the browser is opened unless `open_browser` is false.
/// `store` says where the session goes; later refreshes keep it there.
pub async fn login(home: &Path, spec: &Spec, http: &reqwest::Client, open_browser: bool, store: Store) -> Result<Account> {
    let name = oauth_name(spec)?.to_owned();
    let resource = resource_url(spec);

    let listener = bind_loopback().await?;
    let redirect_uri = format!("http://127.0.0.1:{}/callback", listener.local_addr()?.port());

    let (mut manager, metadata) = resolve(home, &name, &resource, store).await?;
    let client_id = client_id(home, &name, &metadata, http).await?;
    manager
        .configure_client(OAuthClientConfig::new(client_id, &redirect_uri))
        .map_err(|e| anyhow!("{resource}: cannot configure the OAuth client: {e}"))?;
    let scopes: Vec<&str> = spec.auth.scope.as_deref().unwrap_or(DEFAULT_SCOPES).split_whitespace().collect();
    let auth_url = manager
        .get_authorization_url(&scopes)
        .await
        .map_err(|e| anyhow!("{resource}: cannot build the authorization URL: {e}"))?;
    let state = url_state(&auth_url)?;

    if open_browser {
        launch_browser(&auth_url);
    }
    println!("Open this URL to sign in: {auth_url}");

    await_callback(&manager, &listener, &state, &name).await
}

pub fn logout(home: &Path, spec: &Spec) -> Result<bool> {
    let name = oauth_name(spec)?;
    let Some(session) = config::get_one(home, name).and_then(|v| decode(&v)) else { return Ok(false) };
    if !session.has_token() {
        return Ok(false);
    }
    let cleared = Session {
        client_id: session.client_id,
        issuer: session.issuer,
        email: None,
        refresh_token: None,
        access_token: None,
        expires_at: None,
        scopes: Vec::new(),
    };
    config::set_key(home, name, &encode(&cleared)?, config::current_store(home, name))?;
    Ok(true)
}

async fn bind_loopback() -> Result<tokio::net::TcpListener> {
    let mut last = None;
    for port in PORTS {
        match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
            Ok(listener) => return Ok(listener),
            Err(e) => last = Some(e),
        }
    }
    let ports = PORTS.iter().map(u16::to_string).collect::<Vec<_>>().join(", ");
    bail!("no free port among {ports} for the sign-in callback; free one and try again ({})", last.map_or("in use".into(), |e| e.to_string()))
}

fn url_state(auth_url: &str) -> Result<String> {
    let url = Url::parse(auth_url)?;
    url.query_pairs()
        .find(|(k, _)| k == "state")
        .map(|(_, v)| v.into_owned())
        .ok_or_else(|| anyhow!("authorization URL has no state parameter"))
}

/// The callback request target, e.g. `/callback?code=…&state=…`.
enum Callback {
    /// Not the callback path (e.g. `/favicon.ico`): ignore and keep waiting.
    Ignore,
    /// Wrong or missing `state`: reply 400 and keep waiting, without touching rmcp's state.
    BadState,
    /// The user refused sign-in.
    Refused { error: String, description: Option<String> },
    /// A code to exchange.
    Code { code: String, state: String, iss: Option<String> },
}

fn parse_callback(path: &str, expected_state: &str) -> Callback {
    let Some((path_part, _)) = path.split_once('?') else { return Callback::Ignore };
    if path_part != "/callback" {
        return Callback::Ignore;
    }
    let Ok(url) = Url::parse(&format!("http://127.0.0.1{path}")) else { return Callback::BadState };
    let mut code = None;
    let mut state = None;
    let mut iss = None;
    let mut error = None;
    let mut description = None;
    for (k, v) in url.query_pairs() {
        match k.as_ref() {
            "code" => code = Some(v.into_owned()),
            "state" => state = Some(v.into_owned()),
            "iss" => iss = Some(v.into_owned()),
            "error" => error = Some(v.into_owned()),
            "error_description" => description = Some(v.into_owned()),
            _ => {}
        }
    }
    if state.as_deref() != Some(expected_state) {
        return Callback::BadState;
    }
    if let Some(error) = error {
        return Callback::Refused { error, description };
    }
    match code {
        Some(code) => Callback::Code { code, state: state.unwrap_or_default(), iss },
        None => Callback::BadState,
    }
}

async fn await_callback(
    manager: &AuthorizationManager,
    listener: &tokio::net::TcpListener,
    expected_state: &str,
    name: &str,
) -> Result<Account> {
    use tokio::io::AsyncReadExt;
    let deadline = tokio::time::Instant::now() + LOGIN_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let conn = tokio::time::timeout(remaining, listener.accept())
            .await
            .map_err(|_| anyhow!("timed out after 5 minutes waiting for the sign-in to finish"))?;
        let (mut stream, _) = conn?;
        let mut head = Vec::new();
        let mut chunk = [0u8; 1024];
        // A connection that never sends its head must not hold the login past the deadline.
        let read = async {
            loop {
                let n = stream.read(&mut chunk).await?;
                head.extend_from_slice(&chunk[..n]);
                if n == 0 || head.windows(4).any(|w| w == b"\r\n\r\n") || head.len() >= 8192 {
                    return std::io::Result::Ok(());
                }
            }
        };
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        tokio::time::timeout(remaining, read)
            .await
            .map_err(|_| anyhow!("timed out after 5 minutes waiting for the sign-in to finish"))??;
        let head = String::from_utf8_lossy(&head).into_owned();
        let path = request_path(&head);
        match parse_callback(path, expected_state) {
            Callback::Ignore => respond(&mut stream, "404 Not Found", "not found").await?,
            Callback::BadState => respond(&mut stream, "400 Bad Request", "bad or missing state").await?,
            Callback::Refused { error, description } => {
                respond(&mut stream, "400 Bad Request", "sign-in was refused").await?;
                let why = description.map(|d| format!(": {d}")).unwrap_or_default();
                bail!("{name}: sign-in was refused: {error}{why} — try again with `ai-media-router login {name}`");
            }
            Callback::Code { code, state, iss } => {
                match manager.exchange_code_for_token_with_issuer(&code, &state, iss.as_deref()).await {
                    Ok(token) => {
                        respond(&mut stream, "200 OK", "Signed in. You can close this tab.").await?;
                        return Ok(Account { email: id_token_email(&token) });
                    }
                    Err(e) => {
                        respond(&mut stream, "400 Bad Request", "sign-in failed").await?;
                        bail!("{name}: token exchange failed: {e} — sign in again with `ai-media-router login {name}`");
                    }
                }
            }
        }
    }
}

async fn respond(stream: &mut tokio::net::TcpStream, status: &str, body: &str) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let page = format!("<!doctype html><meta charset=utf-8><title>AI Media Router</title><p>{body}</p>");
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}",
        page.len()
    );
    Ok(stream.write_all(response.as_bytes()).await?)
}

fn request_path(head: &str) -> &str {
    head.lines().next().unwrap_or("").split_whitespace().nth(1).unwrap_or("")
}

fn launch_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    #[cfg(target_os = "windows")]
    // Not `cmd /C start`: cmd would split the URL at each `&`.
    let _ = std::process::Command::new("rundll32").arg("url.dll,FileProtocolHandler").arg(url).spawn();
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    let _ = url;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_home() -> PathBuf {
        let home = std::env::temp_dir().join(format!("ai-media-router-oauth-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&home).unwrap();
        home
    }

    fn session() -> Session {
        Session {
            client_id: "client-1".into(),
            issuer: Some("https://issuer.test".into()),
            email: Some("a@b.test".into()),
            refresh_token: Some("refresh-secret".into()),
            access_token: None,
            expires_at: None,
            scopes: vec!["openid".into()],
        }
    }

    #[test]
    fn round_trip_and_status() {
        let s = session();
        let value = encode(&s).unwrap();
        assert!(!value.contains("refresh-secret"), "token must not appear in the encoded value");
        let back = decode(&value).unwrap();
        assert_eq!(back.client_id, "client-1");
        assert_eq!(back.refresh_token.as_deref(), Some("refresh-secret"));

        let st = status(Some(&value));
        assert!(st.signed_in);
        assert_eq!(st.email.as_deref(), Some("a@b.test"));

        let logged_out = Session { client_id: "client-1".into(), issuer: s.issuer, email: None, refresh_token: None, access_token: None, expires_at: None, scopes: Vec::new() };
        let st = status(Some(&encode(&logged_out).unwrap()));
        assert!(!st.signed_in);
        assert_eq!(st.email, None);
    }

    #[test]
    fn logout_keeps_client_id_and_signs_out() {
        let home = temp_home();
        let spec = Spec::parse(
            "key = \"x\"\nname = \"X\"\ncatalog = []\ndocs = \"-\"\nmodel_hint = \"-\"\nbase_url = \"https://x.test\"\n\
             [auth]\ntype = \"oauth\"\nenv = [\"X_SESSION\"]\n[submit]\nurl = \"/s\"\noutputs = [\"/out\"]\n"
        ).unwrap();
        config::set_key(&home, "X_SESSION", &encode(&session()).unwrap(), Store::File).unwrap();
        assert!(logout(&home, &spec).unwrap());
        let value = config::get_one(&home, "X_SESSION").unwrap();
        let s = decode(&value).unwrap();
        assert_eq!(s.client_id, "client-1");
        assert!(!s.has_token());
        assert!(!logout(&home, &spec).unwrap());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn https_endpoints_are_checked() {
        let meta = |auth: &str, token: &str, reg: Option<&str>| {
            let mut m = AuthorizationMetadata::default();
            m.authorization_endpoint = auth.into();
            m.token_endpoint = token.into();
            m.registration_endpoint = reg.map(str::to_owned);
            m
        };
        let https = meta("https://as.test/authorize", "https://as.test/token", None);
        assert!(check_endpoints(&https, "https://mcp.test").is_ok());
        let http = meta("http://as.test/authorize", "https://as.test/token", None);
        assert!(check_endpoints(&http, "https://mcp.test").is_err());
        let loopback = meta("http://127.0.0.1:9/authorize", "http://127.0.0.1:9/token", None);
        assert!(check_endpoints(&loopback, "http://127.0.0.1:9/mcp").is_ok());
        assert!(check_endpoints(&loopback, "https://mcp.test").is_err());
    }

    #[test]
    fn callback_parser_state_checks() {
        assert!(matches!(parse_callback("/favicon.ico", "s"), Callback::Ignore));
        assert!(matches!(parse_callback("/callback?code=c&state=wrong", "s"), Callback::BadState));
        assert!(matches!(parse_callback("/callback?error=access_denied&state=s", "s"), Callback::Refused { error, .. } if error == "access_denied"));
        assert!(matches!(
            parse_callback("/callback?code=c&state=s&iss=i", "s"),
            Callback::Code { code, state, iss: Some(i) } if code == "c" && state == "s" && i == "i"
        ));
    }

    #[test]
    fn id_token_email_is_extracted() {
        let header = URL_SAFE_NO_PAD.encode("{\"alg\":\"RS256\"}");
        let payload = URL_SAFE_NO_PAD.encode("{\"email\":\"me@example.com\"}");
        let id_token = format!("{header}.{payload}.sig");
        assert_eq!(id_token_email_from_jwt(&id_token).as_deref(), Some("me@example.com"));
        assert_eq!(id_token_email_from_jwt("garbage"), None);
    }
}
