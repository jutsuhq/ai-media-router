//! Serde model of `providers/*.toml`; the format is documented in docs/provider-spec.md.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, bail, ensure};
use serde::Deserialize;
use serde_json::Value;

use crate::jobs::failures::Patterns;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    pub key: String,
    pub name: String,
    pub catalog: Vec<String>,
    pub docs: String,
    pub model_hint: String,
    pub base_url: String,
    #[serde(default)]
    pub download_auth: bool,
    pub notes: Option<String>,
    /// Unit of the reported cost after `cost_multiplier`, e.g. "USD" or "credits".
    pub cost_unit: Option<String>,
    /// Factor applied to every reported cost, e.g. 1e-10 for xAI's USD ticks.
    pub cost_multiplier: Option<f64>,
    /// Cost of every job, in `cost_unit`, when no step reports one (0 for a self-hosted server).
    pub fixed_cost: Option<f64>,
    pub example: Option<Example>,
    pub auth: Auth,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    pub errors: Option<Errors>,
    /// Runs before submit; its captures are available to submit (e.g. a price quote id).
    pub prepare: Option<Step>,
    /// A GET that only checks the credentials and spends nothing (account, balance or model list).
    pub auth_check: Option<Step>,
    pub submit: Step,
    pub poll: Option<Step>,
    pub result: Option<Step>,
    pub upload: Option<Upload>,
    #[serde(default, rename = "route")]
    pub routes: Vec<Route>,
}

/// A known-good `generate` call shown to agents by `get_provider`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Example {
    pub model: String,
    pub input: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthKind {
    Bearer,
    Header,
    Query,
    Headers,
    Basic,
    JwtHs256,
    Command,
    Oauth,
    None,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Auth {
    #[serde(rename = "type")]
    pub kind: AuthKind,
    #[serde(default)]
    pub env: Vec<String>,
    pub name: Option<String>,
    pub prefix: Option<String>,
    pub param: Option<String>,
    #[serde(default)]
    pub names: Vec<String>,
    #[serde(default)]
    pub prefixes: Vec<String>,
    pub ttl_secs: Option<u64>,
    #[serde(default)]
    pub command: Vec<String>,
    /// Credentials are sent only when every `env` var is set, and never gate "configured".
    #[serde(default)]
    pub optional: bool,
    /// For `oauth`: the protected resource URL; defaults to `base_url`.
    pub resource: Option<String>,
    /// For `oauth`: space-separated scopes; default `openid email offline_access`.
    pub scope: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Errors {
    pub code: Option<String>,
    #[serde(default)]
    pub ok: Vec<Value>,
    #[serde(default, deserialize_with = "one_or_many")]
    pub message: Vec<String>,
    /// Extra text patterns per failure kind, checked before the built-in ones (failures.rs).
    #[serde(default)]
    pub kinds: Patterns,
    /// Text pattern -> what to do about it, appended to a matching error.
    #[serde(default)]
    pub hints: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub method: Option<String>,
    #[serde(default)]
    pub url: String,
    /// Calls this tool on the MCP server at `base_url` instead of sending an HTTP request.
    pub tool: Option<String>,
    /// The tool's arguments, a template like `body`.
    pub arguments: Option<Value>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    pub body: Option<Value>,
    #[serde(default)]
    pub form: bool,
    /// Sends the rendered body object as multipart/form-data text fields (arrays repeat the field).
    #[serde(default)]
    pub multipart: bool,
    #[serde(default)]
    pub capture: BTreeMap<String, String>,
    #[serde(default)]
    pub outputs: Vec<String>,
    #[serde(default)]
    pub outputs_base64: Vec<String>,
    pub output_mime: Option<String>,
    /// Template turning an output object without a URL field into one; its fields are the variables.
    pub output_url: Option<String>,
    pub status: Option<String>,
    #[serde(default)]
    pub success: Vec<Value>,
    #[serde(default)]
    pub failure: Vec<Value>,
    #[serde(default, deserialize_with = "one_or_many")]
    pub error: Vec<String>,
    pub cost: Option<String>,
    #[serde(default)]
    pub binary: bool,
}

impl Step {
    pub fn has_outputs(&self) -> bool {
        !self.outputs.is_empty() || !self.outputs_base64.is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UploadKind {
    Multipart,
    Json,
    DataUri,
    Put,
    PostForm,
}

impl UploadKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Multipart => "multipart",
            Self::Json => "json",
            Self::DataUri => "data_uri",
            Self::Put => "put",
            Self::PostForm => "post_form",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Upload {
    #[serde(rename = "type")]
    pub kind: UploadKind,
    pub method: Option<String>,
    pub url: Option<String>,
    /// `put`: the MCP tool that returns the presigned URL, instead of `url`.
    pub tool: Option<String>,
    pub arguments: Option<Value>,
    pub file_field: Option<String>,
    #[serde(default)]
    pub fields: BTreeMap<String, String>,
    pub body: Option<Value>,
    #[serde(default)]
    pub capture: BTreeMap<String, String>,
    pub result: Option<String>,
    /// `put`: pointer to an object of extra headers the presign response demands on the PUT.
    pub put_headers: Option<String>,
    /// `post_form`: pointer to the object of form fields the presign response demands on the POST.
    pub form_fields: Option<String>,
    /// How the agent puts the returned reference into `input`, when it is not a plain URL.
    pub usage: Option<String>,
    /// `put` / `post_form`: a finishing call after the file transfer; with `result`, the reference
    /// is read from its response instead of `file_url`.
    pub complete: Option<Step>,
    /// How long the returned reference stays usable; placeholders reuse an upload until then.
    pub ttl_secs: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    pub models: Vec<String>,
    /// Removed from the model id before templating, so agents can force a route with e.g. `image:`.
    pub strip_prefix: Option<String>,
    pub auth: Option<Auth>,
    pub prepare: Option<Step>,
    pub submit: Option<Step>,
    pub poll: Option<Step>,
    pub result: Option<Step>,
}

/// The steps that apply to one model after `[[route]]` overrides.
pub struct Steps<'a> {
    /// Index of the matching `[[route]]`, if any.
    pub route: Option<usize>,
    /// The model id as sent to the provider (route `strip_prefix` removed).
    pub model: String,
    pub prepare: Option<&'a Step>,
    pub submit: &'a Step,
    pub poll: Option<&'a Step>,
    pub result: Option<&'a Step>,
}

impl<'a> Steps<'a> {
    pub fn named(&self) -> impl Iterator<Item = (&'static str, &'a Step)> {
        named(self.prepare, Some(self.submit), self.poll, self.result)
    }
}

/// The steps that are present, with their table names, in run order.
fn named<'a>(
    prepare: Option<&'a Step>,
    submit: Option<&'a Step>,
    poll: Option<&'a Step>,
    result: Option<&'a Step>,
) -> impl Iterator<Item = (&'static str, &'a Step)> {
    [("prepare", prepare), ("submit", submit), ("poll", poll), ("result", result)]
        .into_iter()
        .filter_map(|(name, step)| Some((name, step?)))
}

/// Variables every generation step may use besides captures and `env.*`.
const JOB_VARS: &[&str] = &["model", "uuid", "request_uuid", "input"];
const UPLOAD_VARS: &[&str] =
    &["uuid", "request_uuid", "file_name", "mime", "media_type", "base64", "data_uri", "size_bytes"];

impl Spec {
    pub fn parse(text: &str) -> Result<Self> {
        let spec: Self = toml::from_str(text)?;
        spec.validate()?;
        Ok(spec)
    }

    pub fn steps(&self, model: &str) -> Steps<'_> {
        let index = self
            .routes
            .iter()
            .position(|r| r.models.iter().any(|g| glob_match(g, model)));
        let mut steps = self.route_steps(index);
        let prefix = index.and_then(|i| self.routes[i].strip_prefix.as_deref()).unwrap_or("");
        steps.model = model.strip_prefix(prefix).unwrap_or(model).to_owned();
        steps
    }

    fn route_steps(&self, index: Option<usize>) -> Steps<'_> {
        let route = index.map(|i| &self.routes[i]);
        Steps {
            route: index,
            model: String::new(),
            prepare: route.and_then(|r| r.prepare.as_ref()).or(self.prepare.as_ref()),
            submit: route.and_then(|r| r.submit.as_ref()).unwrap_or(&self.submit),
            poll: route.and_then(|r| r.poll.as_ref()).or(self.poll.as_ref()),
            result: route.and_then(|r| r.result.as_ref()).or(self.result.as_ref()),
        }
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            !self.key.is_empty()
                && self.key.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
            "key `{}` must match [a-z0-9-]+",
            self.key
        );
        ensure!(
            self.base_url.starts_with("http") || self.base_url.starts_with("{{"),
            "base_url must be an http(s) URL or start with a template such as {{{{env.SERVER_URL}}}}"
        );

        validate_auth(&self.auth, "[auth]", &self.base_url)?;
        for (i, r) in self.routes.iter().enumerate() {
            if let Some(a) = &r.auth {
                validate_auth(a, &format!("route {i} auth"), &self.base_url)?;
            }
        }

        let check = |steps: Steps, label: &str| -> Result<()> {
            let sync = steps.submit.has_outputs();
            if steps.poll.is_some() {
                ensure!(steps.submit.capture.contains_key("id") || sync, "{label}: submit must capture `id`");
            } else if !sync {
                bail!("{label}: without [poll], submit must declare outputs");
            }
            if let Some(r) = steps.result {
                ensure!(r.binary || r.has_outputs(), "{label}: result needs outputs or binary = true");
            } else if let Some(p) = steps.poll {
                ensure!(p.has_outputs(), "{label}: poll needs outputs when there is no [result]");
            }
            Ok(())
        };
        check(self.route_steps(None), "default")?;
        for i in 0..self.routes.len() {
            check(self.route_steps(Some(i)), &format!("route {i}"))?;
        }

        if let Some(step) = &self.auth_check {
            ensure!(
                step.body.is_none()
                    && !step.form
                    && !step.multipart
                    && step.capture.is_empty()
                    && step.outputs.is_empty()
                    && step.outputs_base64.is_empty(),
                "[auth_check]: a free check sends no body, form or multipart and reads no capture or outputs"
            );
            ensure!(
                step.method.as_deref().is_none_or(|m| m.eq_ignore_ascii_case("GET")),
                "[auth_check]: must be a GET (it only reads)"
            );
        }

        if let Some(u) = &self.upload {
            ensure!(
                u.complete.is_none() || matches!(u.kind, UploadKind::Put | UploadKind::PostForm),
                "upload complete is only for put and post_form"
            );
            match u.kind {
                UploadKind::DataUri => {}
                UploadKind::Put | UploadKind::PostForm => ensure!(
                    (u.url.is_some() || (u.tool.is_some() && u.kind == UploadKind::Put))
                        && u.capture.contains_key("upload_url")
                        && (u.capture.contains_key("file_url") || (u.complete.is_some() && u.result.is_some())),
                    "upload {} needs url, capture upload_url, and capture file_url or complete + result",
                    u.kind.as_str()
                ),
                _ => ensure!(
                    u.url.is_some() && u.tool.is_none() && u.result.is_some(),
                    "upload {} needs url and result",
                    u.kind.as_str()
                ),
            }
        }
        self.validate_tools()?;
        if let Some(e) = &self.errors {
            ensure!(e.code.is_some() || e.ok.is_empty(), "[errors]: `ok` needs `code`");
            ensure!(
                e.hints.iter().all(|(p, h)| !p.trim().is_empty() && !h.trim().is_empty()),
                "[errors.hints]: patterns and hints must be non-empty text"
            );
            for (kind, patterns) in &e.kinds {
                ensure!(
                    !patterns.is_empty() && patterns.iter().all(|p| !p.trim().is_empty()),
                    "[errors.kinds].{}: list at least one non-empty text pattern",
                    kind.as_str()
                );
            }
        }
        self.lint_pointers()?;
        self.lint_templates()
    }

    /// Every step is either an HTTP request (`url`) or an MCP tool call (`tool`), and tool calls
    /// need the signed-in account: `[auth] type = "oauth"`.
    fn validate_tools(&self) -> Result<()> {
        let upload = self.upload.as_ref();
        for (name, step) in self.every_step() {
            match &step.tool {
                Some(tool) => {
                    ensure!(!tool.is_empty() && step.url.is_empty(), "[{name}]: set either `url` or `tool`, not both");
                    ensure!(
                        step.method.is_none() && step.body.is_none() && !step.form && !step.multipart && step.headers.is_empty() && !step.binary,
                        "[{name}]: a tool step sends `arguments` only: remove method, body, form, multipart, headers and binary"
                    );
                }
                None => {
                    ensure!(!step.url.is_empty(), "[{name}]: needs `url` (an HTTP request) or `tool` (an MCP tool call)");
                    ensure!(step.arguments.is_none(), "[{name}]: `arguments` is for tool steps; use `body` with `url`");
                }
            }
        }
        if let Some(u) = upload {
            ensure!(u.tool.is_none() || u.url.is_none(), "[upload]: set either `url` or `tool`, not both");
            ensure!(
                u.tool.is_none() || (u.method.is_none() && u.body.is_none() && u.fields.is_empty()),
                "[upload]: a tool upload sends `arguments` only: remove method, body and fields"
            );
            ensure!(u.arguments.is_none() || u.tool.is_some(), "[upload]: `arguments` is for `tool`; use `body` with `url`");
        }
        ensure!(
            self.tool_steps().is_empty() || (self.auth.kind == AuthKind::Oauth && self.routes.iter().all(|r| r.auth.is_none())),
            "tool steps call the MCP server as the signed-in account: set [auth] type = \"oauth\" and no route auth"
        );
        Ok(())
    }

    /// The MCP tool steps, with their table names (`poll`, `route.0.submit`, `upload`, …).
    pub fn tool_steps(&self) -> Vec<(String, &str, Option<&Value>)> {
        let upload = self.upload.as_ref();
        let mut out: Vec<(String, &str, Option<&Value>)> = self
            .every_step()
            .filter_map(|(name, s)| Some((name, s.tool.as_deref()?, s.arguments.as_ref())))
            .collect();
        if let Some(u) = upload
            && let Some(tool) = &u.tool
        {
            out.push(("upload".to_owned(), tool, u.arguments.as_ref()));
        }
        out
    }

    pub fn error_kinds(&self) -> Option<&Patterns> {
        self.errors.as_ref().map(|e| &e.kinds)
    }

    /// `env.*` variables in `base_url`: without them no request can be sent.
    pub fn base_url_env(&self) -> Vec<&str> {
        template_vars(&self.base_url).into_iter().filter_map(|v| v.strip_prefix("env.")).collect()
    }

    /// Every pointer is `""`, `/…` or `header:Name`; a missing slash would silently match nothing.
    fn lint_pointers(&self) -> Result<()> {
        let mut pointers: Vec<(String, &str)> = Vec::new();
        for (name, step) in self.all_steps() {
            pointers.extend(step.capture.iter().map(|(var, p)| (format!("[{name}].capture.{var}"), p.as_str())));
            let fields = present([("status", &step.status), ("cost", &step.cost)]);
            pointers.extend(fields.map(|(field, p)| (format!("[{name}].{field}"), p)));
            pointers.extend(step.error.iter().map(|p| (format!("[{name}].error"), p.as_str())));
            pointers.extend(step.outputs.iter().chain(&step.outputs_base64).map(|p| (format!("[{name}].outputs"), p.as_str())));
        }
        if let Some(e) = &self.errors {
            pointers.extend(present([("code", &e.code)]).map(|(f, p)| (format!("[errors].{f}"), p)));
            pointers.extend(e.message.iter().map(|p| ("[errors].message".to_owned(), p.as_str())));
        }
        if let Some(u) = &self.upload {
            let fields = present([("result", &u.result), ("put_headers", &u.put_headers), ("form_fields", &u.form_fields)]);
            pointers.extend(fields.map(|(field, p)| (format!("[upload].{field}"), p)));
            pointers.extend(u.capture.iter().map(|(var, p)| (format!("[upload].capture.{var}"), p.as_str())));
        }
        for (field, p) in pointers {
            ensure!(
                p.is_empty() || p.starts_with('/') || p.starts_with("header:"),
                "{field}: pointer `{p}` must start with `/` (JSON pointer) or `header:` (response header)"
            );
        }
        Ok(())
    }

    /// Catches `{{typo}}` before the first real request does.
    fn lint_templates(&self) -> Result<()> {
        let captured: BTreeSet<&str> =
            self.all_steps().flat_map(|(_, s)| s.capture.keys().map(String::as_str)).collect();
        let env = |v: &str| v.starts_with("env.");
        for (name, step) in self.all_steps() {
            let found = templates([&step.url], step.headers.values().chain(self.headers.values()), [&step.body, &step.arguments]);
            let ok = |v: &str| JOB_VARS.contains(&v) || captured.contains(v) || env(v);
            check_vars(&format!("[{name}]"), &found, ok, "is never captured or provided")?;
        }
        if let Some(u) = &self.upload {
            let found = templates(&u.url, u.fields.values().chain(self.headers.values()), [&u.body, &u.arguments]);
            let why = format!("is not available in uploads (have {})", UPLOAD_VARS.join(", "));
            check_vars("[upload]", &found, |v| UPLOAD_VARS.contains(&v) || env(v), &why)?;
            if let Some(c) = &u.complete {
                let found = templates([&c.url], c.headers.values(), [&c.body, &c.arguments]);
                let ok = |v: &str| UPLOAD_VARS.contains(&v) || u.capture.contains_key(v) || env(v);
                check_vars("[upload.complete]", &found, ok, "is neither an upload variable nor captured")?;
            }
        }
        Ok(())
    }

    /// `all_steps` plus `[auth_check]` and `[upload.complete]`.
    fn every_step(&self) -> impl Iterator<Item = (String, &Step)> {
        let complete = self.upload.as_ref().and_then(|u| u.complete.as_ref());
        let extra = [("auth_check", self.auth_check.as_ref()), ("upload.complete", complete)];
        self.all_steps().chain(extra.into_iter().filter_map(|(n, s)| Some((n.to_owned(), s?))))
    }

    fn all_steps(&self) -> impl Iterator<Item = (String, &Step)> {
        let top = named(self.prepare.as_ref(), Some(&self.submit), self.poll.as_ref(), self.result.as_ref())
            .map(|(name, step)| (name.to_owned(), step));
        let routed = self.routes.iter().enumerate().flat_map(|(i, r)| {
            named(r.prepare.as_ref(), r.submit.as_ref(), r.poll.as_ref(), r.result.as_ref())
                .map(move |(name, step)| (format!("route.{i}.{name}"), step))
        });
        top.chain(routed)
    }
}

/// Every template string of a step-like table: url, header values, then strings in the body
/// and the tool arguments.
fn templates<'a>(
    url: impl IntoIterator<Item = &'a String>,
    headers: impl Iterator<Item = &'a String>,
    bodies: [&'a Option<Value>; 2],
) -> Vec<&'a str> {
    let mut out: Vec<&str> = url.into_iter().chain(headers).map(String::as_str).collect();
    for body in bodies.into_iter().flatten() {
        strings(body, &mut out);
    }
    out
}

fn present<'a, const N: usize>(fields: [(&'static str, &'a Option<String>); N]) -> impl Iterator<Item = (&'static str, &'a str)> {
    fields.into_iter().filter_map(|(field, p)| Some((field, p.as_deref()?)))
}

fn check_vars(table: &str, templates: &[&str], ok: impl Fn(&str) -> bool, why: &str) -> Result<()> {
    for var in templates.iter().flat_map(|t| template_vars(t)) {
        ensure!(ok(var), "{table}: template variable `{{{{{var}}}}}` {why}");
    }
    Ok(())
}
fn validate_auth(a: &Auth, label: &str, base_url: &str) -> Result<()> {
    let need_env = match a.kind {
        AuthKind::None | AuthKind::Command => 0,
        AuthKind::Basic | AuthKind::JwtHs256 => 2,
        _ => 1,
    };
    ensure!(a.env.len() >= need_env, "{label}: auth `{:?}` needs {need_env} env var(s)", a.kind);
    match a.kind {
        AuthKind::Header => ensure!(a.name.is_some(), "{label}: auth header needs `name`"),
        AuthKind::Query => ensure!(a.param.is_some(), "{label}: auth query needs `param`"),
        AuthKind::Headers => ensure!(a.names.len() == a.env.len(), "{label}: auth headers needs one name per env var"),
        AuthKind::Command => ensure!(!a.command.is_empty(), "{label}: auth command needs `command`"),
        AuthKind::Oauth => {
            ensure!(a.env.len() == 1, "{label}: auth oauth needs exactly one env var (the session slot)");
            let resource = a.resource.as_deref().unwrap_or(base_url);
            ensure!(
                is_https_or_loopback_http(resource),
                "{label}: auth oauth `resource` `{resource}` must be https or a loopback http://127.0.0.1/localhost URL"
            );
        }
        _ => {}
    }
    Ok(())
}

/// `resource` must be https, or loopback http (only for a local emulator).
fn is_https_or_loopback_http(url: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(url) else { return false };
    match url.scheme() {
        "https" => true,
        "http" => matches!(url.host_str(), Some("127.0.0.1") | Some("localhost") | Some("::1")),
        _ => false,
    }
}

/// A pointer field written as one string or a list of them.
fn one_or_many<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }
    Ok(match OneOrMany::deserialize(d)? {
        OneOrMany::One(s) => vec![s],
        OneOrMany::Many(v) => v,
    })
}

fn strings<'a>(v: &'a Value, out: &mut Vec<&'a str>) {
    match v {
        Value::String(s) => out.push(s),
        Value::Array(items) => items.iter().for_each(|i| strings(i, out)),
        Value::Object(map) => map.values().for_each(|i| strings(i, out)),
        _ => {}
    }
}

fn template_vars(template: &str) -> Vec<&str> {
    let mut vars = Vec::new();
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        let Some(end) = rest[start..].find("}}") else { break };
        vars.push(rest[start + 2..start + end].trim());
        rest = &rest[start + end + 2..];
    }
    vars
}

/// `*` matches any run of characters; everything else is literal.
fn glob_match(pattern: &str, text: &str) -> bool {
    let Some((first, tail)) = pattern.split_once('*') else {
        return pattern == text;
    };
    let Some(mut rest) = text.strip_prefix(first) else {
        return false;
    };
    let (middle, last) = tail.rsplit_once('*').unwrap_or(("", tail));
    for part in middle.split('*') {
        match rest.find(part) {
            Some(i) => rest = &rest[i + part.len()..],
            None => return false,
        }
    }
    rest.ends_with(last)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob() {
        assert!(glob_match("kling/*", "kling/v1/videos"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("a*c*e", "abcde"));
        assert!(glob_match("exact", "exact"));
        assert!(!glob_match("exact", "exactly"));
        assert!(!glob_match("kling/*", "veo/3"));
    }

    #[test]
    fn tool_steps_need_oauth_and_send_only_arguments() {
        let head = "key = \"x\"\nname = \"X\"\ncatalog = []\ndocs = \"-\"\nmodel_hint = \"-\"\nbase_url = \"https://x.test/mcp\"\n";
        let oauth = "[auth]\ntype = \"oauth\"\nenv = [\"X_SESSION\"]\n";
        let submit = "[submit]\ntool = \"gen\"\narguments = { prompt = \"{{input}}\" }\noutputs = [\"/url\"]\n";
        let parse = |text: String| Spec::parse(&text).map_err(|e| format!("{e:#}"));
        let spec = parse(format!("{head}{oauth}{submit}")).unwrap();
        assert_eq!(spec.tool_steps().iter().map(|(t, n, _)| (t.as_str(), *n)).collect::<Vec<_>>(), [("submit", "gen")]);
        let err = parse(format!("{head}[auth]\ntype = \"none\"\n{submit}")).unwrap_err();
        assert!(err.contains("type = \"oauth\""), "{err}");
        let err = parse(format!("{head}{oauth}{submit}body = {{ a = 1 }}\n")).unwrap_err();
        assert!(err.contains("[submit]: a tool step sends `arguments` only"), "{err}");
        let err = parse(format!("{head}{oauth}{submit}url = \"/s\"\n")).unwrap_err();
        assert!(err.contains("either `url` or `tool`"), "{err}");
        let err = parse(format!("{head}{oauth}[submit]\noutputs = [\"/url\"]\n")).unwrap_err();
        assert!(err.contains("needs `url`"), "{err}");
        let err = parse(format!("{head}{oauth}[submit]\ntool = \"gen\"\narguments = {{ p = \"{{{{typo}}}}\" }}\noutputs = [\"/u\"]\n")).unwrap_err();
        assert!(err.contains("`{{typo}}`"), "templates in arguments are linted: {err}");
    }

    const MINIMAL: &str = r#"
key = "x"
name = "X"
catalog = []
docs = "-"
model_hint = "-"
base_url = "https://x.test"
[auth]
type = "none"
[submit]
url = "/s"
capture = { id = "/id" }
[poll]
url = "/p/{{id}}"
outputs = ["/out"]
"#;

    #[test]
    fn auth_check_is_get_only_and_reads_nothing() {
        let spec = |table: &str| Spec::parse(&format!("{MINIMAL}{table}"));
        spec("[auth_check]\nurl = \"/me\"\n").unwrap();
        spec("[auth_check]\nurl = \"/me\"\nmethod = \"get\"\n").unwrap();
        let err = format!("{:#}", spec("[auth_check]\nurl = \"/me\"\nbody = { a = 1 }\n").unwrap_err());
        assert!(err.contains("[auth_check]") && err.contains("no body"), "{err}");
        let err = format!("{:#}", spec("[auth_check]\nurl = \"/me\"\nmethod = \"POST\"\n").unwrap_err());
        assert!(err.contains("[auth_check]") && err.contains("GET"), "{err}");
        let err = format!("{:#}", spec("[auth_check]\nurl = \"/me\"\noutputs = [\"/out\"]\n").unwrap_err());
        assert!(err.contains("[auth_check]"), "{err}");
    }

    #[test]
    fn lints_catch_typos() {
        Spec::parse(MINIMAL).unwrap();
        let err = Spec::parse(&MINIMAL.replace("/p/{{id}}", "/p/{{task_id}}")).unwrap_err().to_string();
        assert!(err.contains("task_id"), "{err}");
        let err = Spec::parse(&MINIMAL.replace("\"/out\"", "\"out\"")).unwrap_err().to_string();
        assert!(err.contains("[poll].outputs"), "{err}");
        Spec::parse(&MINIMAL.replace("\"/out\"", "\"header:X-Out\"")).unwrap();
    }

    #[test]
    fn error_kinds_are_validated() {
        let kinds = |table: &str| Spec::parse(&format!("{MINIMAL}{table}"));
        let spec = kinds("[errors.kinds]
moderation = [\"banned phrase\"]
").unwrap();
        assert_eq!(spec.error_kinds().unwrap()[&crate::jobs::failures::ErrorKind::Moderation], ["banned phrase"]);
        let err = format!("{:#}", kinds("[errors.kinds]
moderaton = [\"x\"]
").unwrap_err());
        assert!(err.contains("moderaton") && err.contains("rate_limit"), "{err}");
        let err = format!("{:#}", kinds("[errors.kinds]
quota = []
").unwrap_err());
        assert!(err.contains("[errors.kinds].quota"), "{err}");
        let err = format!("{:#}", kinds("[errors]
ok = [0]
").unwrap_err());
        assert!(err.contains("needs `code`"), "{err}");
    }

    #[test]
    fn oauth_spec_parses_and_checks_resource() {
        let oauth = MINIMAL.replace("type = \"none\"", "type = \"oauth\"\nenv = [\"X_SESSION\"]");
        Spec::parse(&oauth).unwrap();
        let err = Spec::parse(&oauth.replace("https://x.test", "http://example.com")).unwrap_err().to_string();
        assert!(err.contains("oauth") && err.contains("resource"), "{err}");
        Spec::parse(&oauth.replace("https://x.test", "http://127.0.0.1:9")).unwrap();
    }
}
