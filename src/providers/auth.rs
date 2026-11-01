use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, KeyInit, Mac};
use reqwest::{Method, RequestBuilder, Url};
use serde_json::json;
use sha2::Sha256;
use tokio::sync::Mutex;

use super::{
    exchange::AuthPlaceholders,
    spec::{Auth, AuthKind},
};
use crate::{
    config::Env,
    db::now,
};

const COMMAND_TOKEN_TTL: Duration = Duration::from_secs(30 * 60);

pub struct Authenticator {
    auth: Auth,
    env: Env,
    /// Generated bearer token (JWT or command output) and when it stops being reused.
    cached: Mutex<Option<(String, Instant)>>,
}

impl Authenticator {
    pub fn new(auth: Auth, env: Env) -> Self {
        Self { auth, env, cached: Mutex::new(None) }
    }

    /// Env vars that must be set before this provider can be used.
    pub fn missing(&self) -> Vec<String> {
        if self.auth.optional {
            return Vec::new();
        }
        match self.auth.kind {
            AuthKind::Command => {
                let overridden = self.auth.env.first().is_some_and(|v| self.env.get(v).is_some());
                if overridden || on_path(&self.auth.command[0]) {
                    Vec::new()
                } else {
                    vec![self.auth.env.first().cloned().unwrap_or_else(|| self.auth.command[0].clone())]
                }
            }
            _ => self.auth.env.iter().filter(|v| self.env.get(v).is_none()).cloned().collect(),
        }
    }

    pub async fn request(&self, http: &reqwest::Client, method: Method, mut url: Url) -> Result<RequestBuilder> {
        let a = &self.auth;
        if a.kind == AuthKind::Query && !self.skipped() {
            url.query_pairs_mut().append_pair(self.query_param(), self.var(0)?);
        }
        let req = http.request(method, url);
        if self.skipped() {
            return Ok(req);
        }
        Ok(match a.kind {
            AuthKind::None | AuthKind::Query => req,
            AuthKind::Bearer => req.bearer_auth(self.var(0)?),
            AuthKind::Header | AuthKind::Headers => {
                let mut req = req;
                for (i, (name, prefix)) in self.named_headers().into_iter().enumerate() {
                    req = req.header(name, format!("{prefix}{}", self.var(i)?));
                }
                req
            }
            AuthKind::Basic => req.basic_auth(self.var(0)?, Some(self.var(1)?)),
            AuthKind::JwtHs256 | AuthKind::Command => req.bearer_auth(self.token().await?),
        })
    }

    /// What `request` adds, with every secret shown as `<ENV_NAME>`.
    pub fn placeholders(&self) -> AuthPlaceholders {
        let a = &self.auth;
        if self.skipped() {
            return AuthPlaceholders { headers: Vec::new(), query: None };
        }
        let name = |i: usize| format!("<{}>", a.env.get(i).map_or("?", String::as_str));
        let bearer = |token: String| vec![("authorization".to_owned(), format!("Bearer {token}"))];
        let headers = match a.kind {
            AuthKind::None | AuthKind::Query => Vec::new(),
            AuthKind::Bearer => bearer(name(0)),
            AuthKind::Header | AuthKind::Headers => self
                .named_headers()
                .into_iter()
                .enumerate()
                .map(|(i, (header, prefix))| (header.to_ascii_lowercase(), format!("{prefix}{}", name(i))))
                .collect(),
            AuthKind::Basic => vec![("authorization".to_owned(), format!("Basic base64({}:{})", name(0), name(1)))],
            AuthKind::JwtHs256 => bearer(format!("<JWT iss={} signed with {}>", name(0), name(1))),
            AuthKind::Command => bearer(format!("<output of `{}`>", a.command.join(" "))),
        };
        let query = (a.kind == AuthKind::Query).then(|| (self.query_param().to_owned(), name(0)));
        AuthPlaceholders { headers, query }
    }

    /// `(header, prefix)` per env var, for `header` and `headers` auth.
    fn named_headers(&self) -> Vec<(&str, &str)> {
        let a = &self.auth;
        if a.kind == AuthKind::Header {
            return vec![(a.name.as_deref().unwrap_or("Authorization"), a.prefix.as_deref().unwrap_or(""))];
        }
        a.names.iter().enumerate().map(|(i, n)| (n.as_str(), a.prefixes.get(i).map_or("", String::as_str))).collect()
    }

    /// Optional credentials that are not (fully) set: requests go out without them.
    fn skipped(&self) -> bool {
        self.auth.optional && self.auth.env.iter().any(|v| self.env.get(v).is_none())
    }

    fn query_param(&self) -> &str {
        self.auth.param.as_deref().unwrap_or("key")
    }

    fn var(&self, i: usize) -> Result<&str> {
        let name = self.auth.env.get(i).ok_or_else(|| anyhow!("auth env #{i} missing in spec"))?;
        self.env.get(name).ok_or_else(|| anyhow!("{name} is not set"))
    }

    async fn token(&self) -> Result<String> {
        let mut cached = self.cached.lock().await;
        if let Some((token, until)) = cached.as_ref()
            && Instant::now() < *until
        {
            return Ok(token.clone());
        }
        let (token, ttl) = match self.auth.kind {
            AuthKind::JwtHs256 => {
                let ttl = self.auth.ttl_secs.unwrap_or(1800);
                let jwt = jwt_hs256(self.var(0)?, self.var(1)?, ttl)?;
                (jwt, Duration::from_secs(ttl.saturating_sub(60)))
            }
            _ => match self.auth.env.first().and_then(|v| self.env.get(v)) {
                Some(token) => return Ok(token.to_owned()),
                None => (run_command(&self.auth.command).await?, COMMAND_TOKEN_TTL),
            },
        };
        *cached = Some((token.clone(), Instant::now() + ttl));
        Ok(token)
    }
}

fn jwt_hs256(issuer: &str, secret: &str, ttl_secs: u64) -> Result<String> {
    let t = now();
    let header = URL_SAFE_NO_PAD.encode(json!({ "alg": "HS256", "typ": "JWT" }).to_string());
    let claims = json!({ "iss": issuer, "exp": t + ttl_secs as i64, "nbf": t - 5 });
    let payload = URL_SAFE_NO_PAD.encode(claims.to_string());
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).context("bad JWT secret")?;
    mac.update(format!("{header}.{payload}").as_bytes());
    let signature = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
    Ok(format!("{header}.{payload}.{signature}"))
}

async fn run_command(command: &[String]) -> Result<String> {
    // `cmd /C` resolves .cmd/.bat shims such as gcloud.cmd on Windows.
    let mut cmd = if cfg!(windows) {
        let mut c = tokio::process::Command::new("cmd");
        c.arg("/C").args(command);
        c
    } else {
        let mut c = tokio::process::Command::new(&command[0]);
        c.args(&command[1..]);
        c
    };
    let out = cmd.output().await.with_context(|| format!("cannot run {}", command.join(" ")))?;
    if !out.status.success() {
        bail!("{} failed: {}", command.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    let token = String::from_utf8(out.stdout)?.trim().to_owned();
    if token.is_empty() {
        bail!("{} printed no token", command.join(" "));
    }
    Ok(token)
}

fn on_path(program: &str) -> bool {
    let exts: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".EXE;.CMD;.BAT".into())
            .split(';')
            .map(str::to_owned)
            .collect()
    } else {
        vec![String::new()]
    };
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths)
            .any(|dir| exts.iter().any(|ext| dir.join(format!("{program}{ext}")).is_file()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jwt_shape_and_signature() {
        let jwt = jwt_hs256("ak", "sk", 1800).unwrap();
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3);
        let claims: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
        assert_eq!(claims["iss"], "ak");
        let mut mac = Hmac::<Sha256>::new_from_slice(b"sk").unwrap();
        mac.update(format!("{}.{}", parts[0], parts[1]).as_bytes());
        mac.verify_slice(&URL_SAFE_NO_PAD.decode(parts[2]).unwrap()).unwrap();
    }
}
