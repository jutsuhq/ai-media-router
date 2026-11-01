//! `{{var}}` templates and the JSON-pointer dialect used by provider specs.

use std::borrow::Cow;

use anyhow::{Result, anyhow, bail};
use serde_json::{Map, Value};

use crate::config::Env;

/// Fixed so recorded fixtures can compare request bodies that contain `{{uuid}}`.
pub const REPLAY_UUID: &str = "00000000-0000-4000-8000-000000000000";

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Live,
    /// Fixture replay: deterministic UUIDs.
    Replay,
    /// Dry run: unset env vars and UUIDs render as `<placeholders>` instead of failing.
    DryRun,
}

pub struct Ctx<'a> {
    pub vars: &'a Map<String, Value>,
    pub input: &'a Value,
    pub env: &'a Env,
    pub mode: Mode,
}

impl Ctx<'_> {
    /// JSON value of a variable; strings for `env.*` and `request_uuid`.
    fn value(&self, name: &str) -> Result<Value> {
        match name {
            "input" => Ok(self.input.clone()),
            _ => match self.vars.get(name) {
                Some(v) => Ok(v.clone()),
                None => self.lookup(name).map(Value::String),
            },
        }
    }

    fn lookup(&self, name: &str) -> Result<String> {
        if let Some(var) = name.strip_prefix("env.") {
            return match (self.env.get(var), self.mode) {
                (Some(v), _) => Ok(v.to_owned()),
                (None, Mode::DryRun) => Ok(format!("<{var}>")),
                (None, _) => bail!("environment variable {var} is not set (environment or config.toml [env])"),
            };
        }
        if name == "request_uuid" {
            return Ok(match self.mode {
                Mode::Live => uuid::Uuid::new_v4().to_string(),
                Mode::Replay => REPLAY_UUID.to_owned(),
                Mode::DryRun => "<request_uuid>".to_owned(),
            });
        }
        let value = if name == "input" { Some(self.input) } else { self.vars.get(name) };
        match value {
            Some(Value::String(s)) => Ok(s.clone()),
            Some(v) => Ok(v.to_string()),
            None => {
                let have: Vec<&str> = self.vars.keys().filter(|k| !k.starts_with("__")).map(String::as_str).collect();
                bail!("template variable `{name}` is not available (have: {})", have.join(", "))
            }
        }
    }
}

pub fn render_str(template: &str, ctx: &Ctx) -> Result<String> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        let end = rest[start..]
            .find("}}")
            .ok_or_else(|| anyhow!("unclosed `{{{{` in `{template}`"))?;
        out.push_str(&rest[..start]);
        out.push_str(&ctx.lookup(rest[start + 2..start + end].trim())?);
        rest = &rest[start + end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Renders every string in a JSON template. A string that is exactly one `{{var}}` becomes that
/// variable's JSON value (so `"{{input}}"` inserts the input object); the key `"$merge"` splices an
/// object's fields into its parent after the other keys, so merged input fields win.
pub fn render_value(template: &Value, ctx: &Ctx) -> Result<Value> {
    Ok(match template {
        Value::String(s) => match whole_var(s) {
            Some(name) => ctx.value(name)?,
            None => Value::String(render_str(s, ctx)?),
        },
        Value::Array(items) => {
            Value::Array(items.iter().map(|v| render_value(v, ctx)).collect::<Result<_>>()?)
        }
        Value::Object(map) => {
            let mut out = Map::new();
            for (k, v) in map.iter().filter(|(k, _)| *k != "$merge") {
                out.insert(k.clone(), render_value(v, ctx)?);
            }
            if let Some(merge) = map.get("$merge") {
                match render_value(merge, ctx)? {
                    Value::Object(fields) => out.extend(fields),
                    Value::Null => {}
                    other => bail!("`$merge` needs an object, got {other}"),
                }
            }
            Value::Object(out)
        }
        other => other.clone(),
    })
}

fn whole_var(s: &str) -> Option<&str> {
    let inner = s.trim().strip_prefix("{{")?.strip_suffix("}}")?;
    (!inner.contains("{{") && !inner.contains("}}")).then(|| inner.trim())
}

/// Resolves a pointer; `*` fans out over arrays, `*[key=value]` keeps only the items whose `key`
/// equals `value`, `A#B` parses the string at `A` as JSON.
pub fn resolve(doc: &Value, pointer: &str) -> Vec<Value> {
    let (head, tail) = match pointer.split_once('#') {
        Some((h, t)) => (h, Some(t)),
        None => (pointer, None),
    };
    let mut found = vec![doc];
    for seg in head.split('/').skip(1) {
        let seg = if seg.contains('~') { seg.replace("~1", "/").replace("~0", "~").into() } else { Cow::Borrowed(seg) };
        found = found
            .into_iter()
            .flat_map(|v| -> Vec<&Value> {
                match (v, &*seg) {
                    (Value::Array(items), "*") => items.iter().collect(),
                    (Value::Object(map), "*") => map.values().collect(),
                    (Value::Array(items), filter) if filter.starts_with("*[") && filter.ends_with(']') => {
                        let (key, want) = filter[2..filter.len() - 1].split_once('=').unwrap_or((filter, ""));
                        items.iter().filter(|i| i.get(key).is_some_and(|x| as_text(x) == want)).collect()
                    }
                    (Value::Array(items), idx) => idx.parse::<usize>().ok().and_then(|i| items.get(i)).into_iter().collect(),
                    (Value::Object(map), key) => map.get(key).into_iter().collect(),
                    _ => Vec::new(),
                }
            })
            .collect();
    }
    let found = found.into_iter().filter(|v| !v.is_null());
    match tail {
        None => found.cloned().collect(),
        Some(tail) => found
            .flat_map(|v| match v {
                Value::String(s) => serde_json::from_str(s).map_or_else(|_| Vec::new(), |p| resolve(&p, tail)),
                other => resolve(other, tail),
            })
            .collect(),
    }
}

/// Plain-string form used for status comparison and error messages.
pub fn as_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

pub fn as_number(v: &Value) -> Option<f64> {
    v.as_f64().or_else(|| v.as_str()?.trim().parse().ok())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn env() -> Env {
        Env::from_pairs([("REGION", "us-east1")])
    }

    #[test]
    fn renders_strings_input_and_merge() {
        let vars = json!({ "model": "kling/v1", "id": "t1" });
        let input = json!({ "prompt": "cat", "duration": 5 });
        let env = env();
        let ctx = Ctx { vars: vars.as_object().unwrap(), input: &input, env: &env, mode: Mode::Live };

        assert_eq!(render_str("/x/{{model}}/{{ id }}?r={{env.REGION}}", &ctx).unwrap(), "/x/kling/v1/t1?r=us-east1");
        let body = json!([{ "model": "{{model}}", "input": "{{input}}", "$merge": "{{input}}", "n": 1 }]);
        assert_eq!(
            render_value(&body, &ctx).unwrap(),
            json!([{ "model": "kling/v1", "input": { "prompt": "cat", "duration": 5 }, "prompt": "cat", "duration": 5, "n": 1 }])
        );
        let input = json!({ "taskType": "imageInference" });
        let ctx = Ctx { vars: vars.as_object().unwrap(), input: &input, env: &env, mode: Mode::Live };
        let body = json!({ "taskType": "videoInference", "$merge": "{{input}}", "job": "{{id}}", "obj": "{{ input }}" });
        assert_eq!(
            render_value(&body, &ctx).unwrap(),
            json!({ "taskType": "imageInference", "job": "t1", "obj": { "taskType": "imageInference" } })
        );
        assert!(render_str("{{missing}}", &ctx).is_err());
        assert!(render_str("{{env.NOPE}}", &ctx).is_err());
        let dry = Ctx { mode: Mode::DryRun, ..ctx };
        assert_eq!(render_str("{{env.NOPE}}", &dry).unwrap(), "<NOPE>");
    }

    #[test]
    fn resolves_pointer_dialect() {
        let doc = json!({
            "data": [ { "url": "a" }, { "url": "b" }, { "other": 1 } ],
            "result": "{\"resultUrls\":[\"u1\",\"u2\"]}",
            "done": true,
            "a/b": 3
        });
        assert_eq!(resolve(&doc, "/data/*/url"), vec![json!("a"), json!("b")]);
        assert_eq!(resolve(&doc, "/data/1/url"), vec![json!("b")]);
        assert_eq!(resolve(&doc, "/result#/resultUrls/*"), vec![json!("u1"), json!("u2")]);
        assert_eq!(resolve(&doc, "/a~1b"), vec![json!(3)]);
        assert_eq!(as_text(&resolve(&doc, "/done")[0]), "true");
        assert_eq!(resolve(&doc, ""), vec![doc.clone()]);
        assert!(resolve(&doc, "/nope/x").is_empty());
        let steps = json!({ "steps": [ { "type": "user_input", "d": 1 }, { "type": "model_output", "d": 2 } ] });
        assert_eq!(resolve(&steps, "/steps/*[type=model_output]/d"), vec![json!(2)]);
    }
}
