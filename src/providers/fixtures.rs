//! Recorded request/response pairs per provider (`providers/fixtures/<key>/*.json`), replayed
//! through the real engine so a spec change is checked against what the provider actually returns.

use std::{collections::BTreeMap, path::Path};

use anyhow::{Context, Result, bail, ensure};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{BUILTIN_FIXTURES, Provider, TaskState, exchange::Exchange, spec::Spec};
use crate::config::Env;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fixture {
    /// Where the recorded responses come from (docs example, real job trace…).
    #[allow(dead_code)]
    pub source: Option<String>,
    pub model: Option<String>,
    #[serde(default)]
    pub input: Value,
    /// Values for `{{env.X}}` templates; auth secrets are never needed.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Replays an upload of a small file instead of a generation.
    pub upload: Option<UploadCase>,
    pub exchanges: Vec<Exchange>,
    pub expect: Expect,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UploadCase {
    pub file_name: String,
    pub mime: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expect {
    /// Output URLs, or `inline:<mime>:<bytes>` for decoded media, in order.
    #[serde(default)]
    pub outputs: Vec<String>,
    pub cost: Option<f64>,
    /// The run must fail with a message containing this text.
    pub error: Option<String>,
    /// Upload result.
    pub reference: Option<Value>,
}

#[derive(Serialize, JsonSchema)]
pub struct FixtureResult {
    pub name: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Built-in fixtures for `key`, then `<home>/providers/fixtures/<key>/*.json` (same name replaces).
pub fn for_key(key: &str, home: Option<&Path>) -> Vec<(String, String)> {
    let mut all: BTreeMap<String, String> = BUILTIN_FIXTURES
        .iter()
        .filter(|(k, _, _)| *k == key)
        .map(|(_, name, text)| ((*name).to_owned(), (*text).to_owned()))
        .collect();
    if let Some(home) = home {
        let dir = home.join("providers").join("fixtures").join(key);
        for path in std::fs::read_dir(dir).into_iter().flatten().filter_map(|e| e.ok().map(|e| e.path())) {
            if path.extension().is_some_and(|e| e == "json")
                && let (Some(name), Ok(text)) = (path.file_stem(), std::fs::read_to_string(&path))
            {
                all.insert(name.to_string_lossy().into_owned(), text);
            }
        }
    }
    all.into_iter().collect()
}

pub async fn run_all(spec: &Spec, fixtures: &[(String, String)]) -> Vec<FixtureResult> {
    let mut results = Vec::new();
    for (name, text) in fixtures {
        let error = run(spec, text).await.err().map(|e| format!("{e:#}"));
        results.push(FixtureResult { name: name.clone(), ok: error.is_none(), error });
    }
    results
}

enum Outcome {
    Done(Vec<String>, Option<f64>),
    Failed(String),
    Reference(Value),
}

pub async fn run(spec: &Spec, text: &str) -> Result<()> {
    let f: Fixture = serde_json::from_str(text).context("invalid fixture JSON")?;
    let rounds = f.exchanges.len();
    let p = Provider::replaying(spec.clone(), Env::from_map(f.env), f.exchanges);
    let mut trace = Vec::new();
    let outcome: Result<Outcome> = async {
        if let Some(u) = &f.upload {
            return Ok(Outcome::Reference(p.upload(b"fixture".to_vec(), &u.file_name, Some(&u.mime), &mut trace).await?));
        }
        let model = f.model.as_deref().context("fixture needs `model` or `upload`")?;
        let vars = p.submit(model, &f.input, &mut trace).await?;
        for _ in 0..=rounds {
            match p.poll(&vars, &mut trace).await? {
                TaskState::Pending(_) => {}
                TaskState::Done { outputs, cost } => {
                    return Ok(Outcome::Done(outputs.iter().map(|o| o.describe()).collect(), cost));
                }
                TaskState::Failed(msg) => return Ok(Outcome::Failed(msg)),
            }
        }
        bail!("still running after every recorded exchange")
    }
    .await;

    if let Some(want) = &f.expect.error {
        let got = match &outcome {
            Err(e) => format!("{e:#}"),
            Ok(Outcome::Failed(m)) => m.clone(),
            Ok(_) => bail!("expected a failure containing `{want}`, but the run succeeded"),
        };
        ensure!(got.contains(want.as_str()), "expected a failure containing `{want}`, got: {got}");
    } else {
        match outcome? {
            Outcome::Failed(msg) => bail!("job failed: {msg}"),
            Outcome::Done(outputs, cost) => {
                ensure!(!f.expect.outputs.is_empty(), "fixture `expect.outputs` is empty; the run produced {outputs:?}");
                ensure!(outputs == f.expect.outputs, "outputs {outputs:?}, expected {:?}", f.expect.outputs);
                if let Some(want) = f.expect.cost {
                    ensure!(cost.is_some_and(|c| (c - want).abs() < 1e-9), "cost {cost:?}, expected {want}");
                }
            }
            Outcome::Reference(r) => {
                ensure!(Some(&r) == f.expect.reference.as_ref(), "upload returned {r}, expected {:?}", f.expect.reference);
            }
        }
    }
    let left = p.unreplayed();
    ensure!(left == 0, "{left} recorded exchange(s) were never requested");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::builtin_spec;

    #[tokio::test]
    async fn builtin_fixtures_replay() {
        let mut failures = Vec::new();
        for (key, name, text) in BUILTIN_FIXTURES {
            let Some((_, spec_text)) = builtin_spec(key) else {
                failures.push(format!("{key}/{name}: no providers/{key}.toml"));
                continue;
            };
            let spec = Spec::parse(spec_text).unwrap();
            if let Err(e) = run(&spec, text).await {
                failures.push(format!("{key}/{name}: {e:#}"));
            }
        }
        assert!(failures.is_empty(), "fixture failures:\n{}", failures.join("\n"));
        assert!(BUILTIN_FIXTURES.len() >= 3);
    }
}
