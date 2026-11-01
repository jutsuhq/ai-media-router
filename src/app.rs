use std::{path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Result, anyhow};
use directories::ProjectDirs;
use tokio::sync::OnceCell;

use crate::{
    config::Env,
    db::Db,
    pricing::catalog::{self, PriceRow},
    providers::{Provider, Providers, Registry},
};

/// Shared state behind every interface (MCP, REST, CLI); holds no transport-specific logic.
#[derive(Clone)]
pub struct App {
    pub db: Db,
    pub http: reqwest::Client,
    pub home: PathBuf,
    pub assets_dir: PathBuf,
    pub providers: Arc<Providers>,
    /// Whether save_spec / reset_spec may write `<home>/providers/` (off for remote HTTP clients).
    pub spec_writes: bool,
    catalog: Arc<OnceCell<Vec<PriceRow>>>,
}

impl App {
    pub fn init(home: Option<PathBuf>) -> Result<Self> {
        let home = match home {
            Some(h) => h,
            None => default_home().context("cannot determine the data directory; pass --home")?,
        };
        let assets_dir = home.join("assets");
        std::fs::create_dir_all(&assets_dir)
            .with_context(|| format!("cannot create {}", assets_dir.display()))?;

        let http = reqwest::Client::builder()
            .user_agent(concat!("ai-media-router/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(Duration::from_secs(120))
            .build()?;

        Ok(Self {
            db: Db::open(&home.join("ai-media-router.db"))?,
            providers: Arc::new(Providers::load(&home, &http)?),
            http,
            assets_dir,
            catalog: Arc::default(),
            home,
            spec_writes: true,
        })
    }

    /// Current specs and env; picks up edited overrides and config.toml without a restart.
    pub fn registry(&self) -> Arc<Registry> {
        self.providers.current()
    }

    pub fn env(&self) -> Env {
        self.registry().env.clone()
    }

    /// A provider by key, or an error listing the known keys.
    pub fn find_provider(&self, key: &str) -> Result<Arc<Provider>> {
        let registry = self.registry();
        registry.get(key).cloned().ok_or_else(|| {
            let keys: Vec<_> = registry.iter().map(|p| p.spec.key.as_str()).collect();
            NotFound(format!("unknown provider `{key}`; known: {}", keys.join(", "))).into()
        })
    }

    /// A provider that is ready to call; the error tells the agent what to set.
    pub fn provider(&self, key: &str) -> Result<Arc<Provider>> {
        let provider = self.find_provider(key)?;
        let missing = provider.missing_env();
        if !missing.is_empty() {
            return Err(anyhow!(
                "provider `{key}` is not configured: run `ai-media-router keys set {}` in a terminal (the value is \
                 read from a hidden prompt, never from the conversation), or set it in the environment; picked up \
                 without restart",
                missing.join(", ")
            ));
        }
        Ok(provider)
    }

    pub async fn catalog(&self) -> Result<&[PriceRow]> {
        self.catalog
            .get_or_try_init(|| catalog::load(&self.http, &self.home))
            .await
            .map(Vec::as_slice)
    }
}

/// Where data lives when `--home` is not given.
pub fn default_home() -> Option<PathBuf> {
    ProjectDirs::from("", "jutsu", "ai-media-router").map(|dirs| dirs.data_dir().to_owned())
}

/// Lookup miss, reported as 404 by the REST API.
#[derive(Debug)]
pub struct NotFound(pub String);

impl std::fmt::Display for NotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NotFound {}
