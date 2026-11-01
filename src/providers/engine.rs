//! Runs a provider spec: submit → poll → result → outputs, plus reference uploads and dry runs.

use std::collections::{BTreeMap, VecDeque};

use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use reqwest::{
    Method, Url,
    multipart::{Form, Part},
};
use serde_json::{Map, Value, json};

use super::{
    Origin,
    auth::Authenticator,
    exchange::{Doc, Exchange, Payload, Request, Response, Trace, Transport, replay},
    spec::{Spec, Step, UploadKind},
    template::{Ctx, Mode, REPLAY_UUID, as_number, as_text, render_str, render_value},
};
use crate::config::Env;

/// Where a sync provider's outputs are kept between submit and the first poll.
const SYNC_OUTPUTS: &str = "__outputs";
const SUBMIT_COST: &str = "__cost";
/// The model id the agent passed, when a route's `strip_prefix` changed `model`.
const ROUTE_MODEL: &str = "__route_model";
const URL_FIELDS: &[&str] = &["url", "uri", "video_url", "image_url", "audio_url", "downloadUrl", "download_url"];
const BASE64_FIELDS: &[&str] = &["data", "bytesBase64Encoded", "b64_json"];
const MIME_FIELDS: &[&str] = &["mime_type", "mimeType", "media_type"];

pub enum Output {
    Url { url: String, auth: bool },
    Inline { data: Vec<u8>, mime: Option<String> },
}

impl Output {
    /// Stable text form used by fixtures: the URL, or `inline:<mime>:<bytes>`.
    pub fn describe(&self) -> String {
        match self {
            Self::Url { url, .. } => url.clone(),
            Self::Inline { data, mime } => format!("inline:{}:{}", mime.as_deref().unwrap_or("?"), data.len()),
        }
    }
}

pub enum TaskState {
    /// Still running; the text says what the provider reported (or why the spec cannot tell).
    Pending(String),
    Done { outputs: Vec<Output>, cost: Option<f64> },
    Failed(String),
}

pub struct Provider {
    pub spec: Spec,
    pub origin: Origin,
    auth: Authenticator,
    /// One entry per `[[route]]`; `None` uses the top-level auth.
    route_auth: Vec<Option<Authenticator>>,
    env: Env,
    transport: Transport,
}

impl Provider {
    pub fn new(spec: Spec, origin: Origin, env: Env, http: reqwest::Client) -> Self {
        Self::with_transport(spec, origin, env, Transport::Http(http))
    }

    /// A provider that answers from recorded exchanges instead of the network.
    pub fn replaying(spec: Spec, env: Env, exchanges: Vec<Exchange>) -> Self {
        Self::with_transport(spec, Origin::Candidate, env, Transport::Replay(VecDeque::from(exchanges).into()))
    }

    fn with_transport(spec: Spec, origin: Origin, env: Env, transport: Transport) -> Self {
        let auth = Authenticator::new(spec.auth.clone(), env.clone());
        let route_auth =
            spec.routes.iter().map(|r| r.auth.clone().map(|a| Authenticator::new(a, env.clone()))).collect();
        Self { spec, origin, auth, route_auth, env, transport }
    }

    /// Env vars still to set. Empty when the top-level auth or any route's auth is complete.
    pub fn missing_env(&self) -> Vec<String> {
        let any_route_ready = self.route_auth.iter().flatten().any(|a| a.missing().is_empty());
        let mut missing = if any_route_ready { Vec::new() } else { self.auth.missing() };
        missing.extend(self.missing_base_env());
        missing
    }

    fn missing_base_env(&self) -> Vec<String> {
        self.spec.base_url_env().into_iter().filter(|v| self.env.get(v).is_none()).map(str::to_owned).collect()
    }

    pub fn configured(&self) -> bool {
        self.missing_env().is_empty()
    }

    fn authenticator(&self, route: Option<usize>) -> &Authenticator {
        route.and_then(|i| self.route_auth[i].as_ref()).unwrap_or(&self.auth)
    }

    /// Recorded exchanges a replay has not consumed yet.
    pub fn unreplayed(&self) -> usize {
        match &self.transport {
            Transport::Http(_) => 0,
            Transport::Replay(queue) => queue.lock().map_or(0, |q| q.len()),
        }
    }

    fn mode(&self) -> Mode {
        match self.transport {
            Transport::Http(_) => Mode::Live,
            Transport::Replay(_) => Mode::Replay,
        }
    }

    /// `kie poll (providers/kie.toml)`: prefix of every step error, so the failing spec file is obvious.
    fn at(&self, step: &str) -> String {
        format!("{} {step} ({})", self.spec.key, self.origin)
    }

    /// Starts a task and returns the variables later steps need (always `model`, `uuid`, usually `id`).
    pub async fn submit(&self, model: &str, input: &Value, trace: &mut Trace) -> Result<Map<String, Value>> {
        let steps = self.spec.steps(model);
        let auth = self.authenticator(steps.route);
        let mut missing = auth.missing();
        missing.extend(self.missing_base_env());
        if self.mode() == Mode::Live && !missing.is_empty() {
            bail!(
                "{}: model `{model}` uses credentials {}; set them in the environment or config.toml [env]",
                self.spec.key,
                missing.join(", ")
            );
        }
        let mut vars = Map::new();
        vars.insert("model".into(), json!(steps.model));
        if steps.model != model {
            vars.insert(ROUTE_MODEL.into(), json!(model));
        }
        let uuid = match self.mode() {
            Mode::Live => uuid::Uuid::new_v4().to_string(),
            _ => REPLAY_UUID.to_owned(),
        };
        vars.insert("uuid".into(), json!(uuid));

        if let Some(prepare) = steps.prepare {
            let doc = self.call("prepare", prepare, auth, &vars, input, trace).await?;
            capture(prepare, &doc, &mut vars);
        }
        let doc = self.call("submit", steps.submit, auth, &vars, input, trace).await?;
        capture(steps.submit, &doc, &mut vars);
        if let Some(cost) = cost_of(steps.submit, &doc) {
            vars.insert(SUBMIT_COST.into(), json!(cost));
        }
        let outputs = self.outputs("submit", steps.submit, &doc)?;
        if !outputs.is_empty() {
            vars.insert(SYNC_OUTPUTS.into(), Value::Array(outputs.iter().map(output_to_json).collect()));
        } else if !vars.contains_key("id") {
            let why = match steps.submit.capture.get("id") {
                Some(p) => format!("capture `id` pointer `{p}` matched nothing"),
                None => format!("outputs pointers {:?} matched no media", steps.submit.outputs),
            };
            bail!("{}: {why} in the response: {}", self.at("submit"), snippet(&doc.body));
        }
        Ok(vars)
    }

    pub async fn poll(&self, vars: &Map<String, Value>, trace: &mut Trace) -> Result<TaskState> {
        if let Some(Value::Array(items)) = vars.get(SYNC_OUTPUTS) {
            let outputs = items.iter().filter_map(|v| output_from_json(v, self.spec.download_auth)).collect();
            let cost = self.scale(vars.get(SUBMIT_COST).and_then(Value::as_f64)).or(self.spec.fixed_cost);
            return Ok(TaskState::Done { outputs, cost });
        }
        let model = vars.get(ROUTE_MODEL).or(vars.get("model")).and_then(Value::as_str).unwrap_or("");
        let steps = self.spec.steps(model);
        let auth = self.authenticator(steps.route);
        let poll = steps.poll.ok_or_else(|| anyhow!("{}: no [poll] step", self.at("poll")))?;
        let doc = self.call("poll", poll, auth, vars, &Value::Null, trace).await?;
        let mut vars = vars.clone();
        capture(poll, &doc, &mut vars);

        let mut poll_outputs = None;
        let error = texts(&doc, &poll.error);
        let finished = match &poll.status {
            Some(pointer) => {
                let Some(status) = doc.first(pointer).map(|v| as_text(&v)) else {
                    if !not_blank(&doc.body) {
                        return Ok(TaskState::Pending("provider returned an empty response; still running".into()));
                    }
                    return Ok(TaskState::Pending(format!(
                        "{}: status pointer `{pointer}` matched nothing; if this persists, [poll].status is wrong: {}",
                        self.at("poll"),
                        snippet(&doc.body)
                    )));
                };
                if one_of(&poll.failure, &status) {
                    let error = error.unwrap_or_else(|| format!("provider status `{status}`"));
                    return Ok(TaskState::Failed(self.hinted(error)));
                }
                if !one_of(&poll.success, &status) {
                    return Ok(TaskState::Pending(format!("provider status `{status}`")));
                }
                true
            }
            None => {
                let found = self.outputs("poll", poll, &doc)?;
                let finished = error.is_some() || !found.is_empty();
                poll_outputs = Some(found);
                finished
            }
        };
        if !finished {
            return Ok(TaskState::Pending("no outputs yet".into()));
        }
        // With failure values listed, status is authoritative and `error` only explains a failure;
        // without them (fal, comfy) a finished task carrying an error is the failure signal.
        if let Some(error) = error.filter(|_| poll.status.is_none() || poll.failure.is_empty()) {
            return Ok(TaskState::Failed(self.hinted(error)));
        }

        let mut cost = cost_of(poll, &doc).or_else(|| vars.get(SUBMIT_COST).and_then(Value::as_f64));
        let (outputs, step, step_name, last) = match steps.result {
            Some(r) if r.binary => {
                let ctx = self.ctx(&vars, &Value::Null, self.mode());
                let url = self.url(&r.url, &ctx).with_context(|| format!("{}: rendering url", self.at("result")))?;
                (vec![Output::Url { url, auth: true }], r, "result", None)
            }
            Some(r) => {
                let result = self.call("result", r, auth, &vars, &Value::Null, trace).await?;
                cost = cost_of(r, &result).or(cost);
                (self.outputs("result", r, &result)?, r, "result", Some(result))
            }
            None => match poll_outputs {
                Some(found) => (found, poll, "poll", None),
                None => (self.outputs("poll", poll, &doc)?, poll, "poll", None),
            },
        };
        if outputs.is_empty() {
            let body = last.as_ref().unwrap_or(&doc);
            let pointers: Vec<&String> = step.outputs.iter().chain(&step.outputs_base64).collect();
            return Ok(TaskState::Failed(format!(
                "{}: finished, but outputs pointers {pointers:?} matched no media in the response: {}",
                self.at(step_name),
                snippet(&body.body)
            )));
        }
        Ok(TaskState::Done { outputs, cost: self.scale(cost).or(self.spec.fixed_cost) })
    }

    fn scale(&self, cost: Option<f64>) -> Option<f64> {
        cost.map(|c| c * self.spec.cost_multiplier.unwrap_or(1.0))
    }

    /// The requests a generation would send, without sending them. Variables a later step needs
    /// from an earlier response are shown as `<var from submit /pointer>` unless given in `vars`.
    pub fn dry_run(&self, model: &str, input: &Value, vars: &Map<String, Value>) -> Result<Vec<Exchange>> {
        let steps = self.spec.steps(model);
        let auth = self.authenticator(steps.route);
        let mut all = Map::new();
        all.insert("model".into(), json!(steps.model));
        all.insert("uuid".into(), json!("<uuid>"));
        for (name, step) in steps.named() {
            for (var, pointer) in &step.capture {
                all.insert(var.clone(), json!(format!("<{var} from {name} {pointer}>")));
            }
        }
        all.extend(vars.clone());

        let mut out = Vec::new();
        for (name, step) in steps.named() {
            let (method, input) = if starts_job(name) { ("POST", input) } else { ("GET", &Value::Null) };
            let ctx = self.ctx(&all, input, Mode::DryRun);
            let req = self.prepare(name, step, method, &ctx)?;
            out.push(Exchange::new(name, req.record(auth.placeholders()), Err("dry run: not sent".into())));
        }
        Ok(out)
    }

    /// One free request that only checks the credentials, per the spec's `[auth_check]`.
    pub async fn check_auth(&self, trace: &mut Trace) -> Result<()> {
        let step = self.spec.auth_check.as_ref().ok_or_else(|| anyhow!("{}: no [auth_check] step", self.at("auth_check")))?;
        let req = self.prepare("auth_check", step, "GET", &self.ctx(&Map::new(), &Value::Null, self.mode()))?;
        self.round_trip("auth_check", &self.auth, req, false, trace).await?;
        Ok(())
    }

    /// Credential values of the top-level and route auths, at least 6 characters.
    pub fn secrets(&self) -> Vec<String> {
        let auths = std::iter::once(&self.spec.auth).chain(self.spec.routes.iter().filter_map(|r| r.auth.as_ref()));
        auths.flat_map(|a| a.env.iter()).filter_map(|n| self.env.get(n)).filter(|v| v.len() >= 6).map(str::to_owned).collect()
    }

    /// `text` with every credential value replaced, for errors that may quote a request or response.
    pub fn redact(&self, text: &str) -> String {
        self.secrets().iter().fold(text.to_owned(), |out, secret| out.replace(secret.as_str(), "<redacted>"))
    }

    /// Makes a file usable as a reference input; returns the provider's URL, id or a data URI.
    pub async fn upload(&self, mut bytes: Vec<u8>, file_name: &str, mime: Option<&str>, trace: &mut Trace) -> Result<Value> {
        let upload = self.spec.upload.as_ref().ok_or_else(|| {
            anyhow!(
                "{} has no [upload]: put a public https URL of the file into `input` instead{}",
                self.spec.key,
                self.spec.notes.as_ref().map_or("", |_| " (see get_provider notes)")
            )
        })?;
        let mime = mime.unwrap_or("application/octet-stream");
        let base64 = STANDARD.encode(&bytes);
        let data_uri = format!("data:{mime};base64,{base64}");
        if upload.kind == UploadKind::DataUri {
            return Ok(Value::String(data_uri));
        }

        let mut vars = Map::new();
        vars.insert("uuid".into(), json!(uuid::Uuid::new_v4().to_string()));
        vars.insert("file_name".into(), json!(file_name));
        vars.insert("mime".into(), json!(mime));
        vars.insert("base64".into(), Value::String(base64));
        vars.insert("data_uri".into(), Value::String(data_uri));
        vars.insert("size_bytes".into(), json!(bytes.len()));
        let at = self.at("upload");
        let ctx = self.ctx(&vars, &Value::Null, self.mode());
        let url = self.url(upload.url.as_deref().unwrap_or_default(), &ctx)?;
        let mut headers = self.headers(&Step::default(), &ctx)?;
        let method = method_name(upload.method.as_deref(), "POST");
        let file_field = upload.file_field.as_deref().unwrap_or("file");
        let (body, payload) = if upload.kind == UploadKind::Multipart {
            let mut fields = Map::new();
            for (k, v) in &upload.fields {
                fields.insert(k.clone(), json!(render_str(v, &ctx).with_context(|| format!("{at}: rendering fields.{k}"))?));
            }
            headers.insert("content-type".into(), "multipart/form-data".into());
            multipart(fields, file_field, file_name, mime, std::mem::take(&mut bytes))
        } else {
            let body = render_value(upload.body.as_ref().unwrap_or(&json!({})), &ctx)
                .with_context(|| format!("{at}: rendering body"))?;
            headers.insert("content-type".into(), "application/json".into());
            (Some(body), Payload::Json)
        };
        let req = Request { method, url, headers, body, payload, auth: true };
        let doc = self.round_trip("upload", &self.auth, req, true, trace).await?;

        if !matches!(upload.kind, UploadKind::Put | UploadKind::PostForm) {
            return pointed(&doc, upload.result.as_deref().unwrap_or_default(), &at, "result");
        }
        let mut got = BTreeMap::new();
        for (var, pointer) in &upload.capture {
            got.insert(var.as_str(), pointed(&doc, pointer, &at, &format!("capture `{var}`"))?);
        }
        let url = as_text(&got["upload_url"]);
        let (step, req) = if upload.kind == UploadKind::Put {
            let mut headers = BTreeMap::from([("content-type".to_owned(), mime.to_owned())]);
            if let Some(Value::Object(extra)) = upload.put_headers.as_deref().and_then(|p| doc.first(p)) {
                headers.extend(extra.iter().map(|(k, v)| (k.to_ascii_lowercase(), as_text(v))));
            }
            let body = Some(json!(format!("<{} bytes>", bytes.len())));
            ("upload_put", Request { method: "PUT".into(), url, headers, body, payload: Payload::Bytes(bytes), auth: false })
        } else {
            let fields = match upload.form_fields.as_deref().and_then(|p| doc.first(p)) {
                Some(Value::Object(f)) => f,
                _ => Map::new(),
            };
            let headers = BTreeMap::from([("content-type".to_owned(), "multipart/form-data".to_owned())]);
            let (body, payload) = multipart(fields, file_field, file_name, mime, bytes);
            ("upload_post", Request { method: "POST".into(), url, headers, body, payload, auth: false })
        };
        self.round_trip(step, &self.auth, req, false, trace).await?;
        let Some(complete) = &upload.complete else {
            return Ok(got.remove("file_url").unwrap_or_default());
        };
        vars.extend(got.iter().map(|(k, v)| ((*k).to_owned(), v.clone())));
        let ctx = self.ctx(&vars, &Value::Null, self.mode());
        let req = self.prepare("upload_complete", complete, "POST", &ctx)?;
        let doc = self.round_trip("upload_complete", &self.auth, req, false, trace).await?;
        match upload.result.as_deref() {
            Some(pointer) => pointed(&doc, pointer, &self.at("upload_complete"), "result"),
            None => Ok(got.remove("file_url").unwrap_or_default()),
        }
    }

    /// GET for an output file, authenticated when the spec says files need the API key.
    pub async fn download(&self, url: &str, auth: bool) -> Result<reqwest::Response> {
        let Transport::Http(http) = &self.transport else { bail!("downloads are not replayed") };
        let url = Url::parse(url)?;
        let req = if auth { self.auth.request(http, Method::GET, url).await? } else { http.get(url) };
        Ok(req.send().await?.error_for_status()?)
    }

    fn ctx<'a>(&'a self, vars: &'a Map<String, Value>, input: &'a Value, mode: Mode) -> Ctx<'a> {
        Ctx { vars, input, env: &self.env, mode }
    }

    async fn call(
        &self,
        name: &str,
        step: &Step,
        auth: &Authenticator,
        vars: &Map<String, Value>,
        input: &Value,
        trace: &mut Trace,
    ) -> Result<Doc> {
        let method = if starts_job(name) { "POST" } else { "GET" };
        let req = self.prepare(name, step, method, &self.ctx(vars, input, self.mode()))?;
        self.round_trip(name, auth, req, true, trace).await
    }

    /// Renders a step into the exact request that will be sent (minus credentials).
    fn prepare(&self, name: &str, step: &Step, default: &str, ctx: &Ctx) -> Result<Request> {
        let at = self.at(name);
        let url = self.url(&step.url, ctx).with_context(|| format!("{at}: rendering url"))?;
        let mut headers = self.headers(step, ctx).with_context(|| format!("{at}: rendering headers"))?;
        let (body, payload) = match &step.body {
            Some(b) => {
                let body = render_value(b, ctx).with_context(|| format!("{at}: rendering body"))?;
                let (mime, payload) = match (step.form, step.multipart) {
                    (true, _) => ("application/x-www-form-urlencoded", Payload::Form),
                    (_, true) => ("multipart/form-data", Payload::FormData),
                    _ => ("application/json", Payload::Json),
                };
                headers.insert("content-type".into(), mime.into());
                (Some(body), payload)
            }
            None => (None, Payload::None),
        };
        Ok(Request { method: method_name(step.method.as_deref(), default), url, headers, body, payload, auth: true })
    }

    fn headers(&self, step: &Step, ctx: &Ctx) -> Result<BTreeMap<String, String>> {
        self.spec
            .headers
            .iter()
            .chain(&step.headers)
            .map(|(k, v)| Ok((k.to_ascii_lowercase(), render_str(v, ctx)?)))
            .collect()
    }

    /// Sends (or replays) one request, records it in the trace and checks the response.
    async fn round_trip(&self, step: &str, auth: &Authenticator, req: Request, json_required: bool, trace: &mut Trace) -> Result<Doc> {
        let record = req.record(auth.placeholders());
        let result = match &self.transport {
            Transport::Http(http) => send(http, auth, req).await,
            Transport::Replay(queue) => replay(queue, step, &record),
        };
        trace.push(Exchange::new(step, record, result.as_ref().map_err(|e| format!("{e:#}"))));
        self.check(result.with_context(|| self.at(step))?, json_required).with_context(|| self.at(step))
    }

    /// HTTP status, JSON parsing and the spec's `[errors]` block.
    fn check(&self, resp: Response, json_required: bool) -> Result<Doc> {
        let Response { status, headers, text, json } = resp;
        let messages = self.spec.errors.as_ref().map_or(&[][..], |e| &e.message[..]);
        if !(200..300).contains(&status) {
            let explained = json.map(|body| Doc { body, headers }).and_then(|doc| texts(&doc, messages));
            bail!("{}", self.hinted(format!("HTTP {status}: {}", explained.unwrap_or_else(|| truncate(&text)))));
        }
        let body = match json {
            Some(v) => v,
            None if text.trim().is_empty() => Value::Null,
            None if !json_required => Value::String(text),
            None => bail!("non-JSON response: {}", truncate(&text)),
        };
        let doc = Doc { body, headers };
        if let Some(errors) = &self.spec.errors
            && let Some(pointer) = &errors.code
            && let Some(code) = doc.first(pointer)
        {
            let code = as_text(&code);
            if !one_of(&errors.ok, &code) {
                let msg = texts(&doc, messages).unwrap_or_else(|| snippet(&doc.body));
                bail!("{}", self.hinted(format!("provider error code `{code}` at [errors].code `{pointer}`: {msg}")));
            }
        }
        Ok(doc)
    }

    /// Relative URLs join `base_url`; dry runs keep `<placeholders>` unparsed.
    fn url(&self, template: &str, ctx: &Ctx) -> Result<String> {
        let rendered = render_str(template, ctx)?;
        let dry = ctx.mode == Mode::DryRun;
        if rendered.starts_with("http://") || rendered.starts_with("https://") || (dry && rendered.starts_with('<')) {
            return Ok(rendered);
        }
        let base = render_str(&self.spec.base_url, ctx)?;
        let full = format!("{}/{}", base.trim_end_matches('/'), rendered.trim_start_matches('/'));
        if !dry {
            Url::parse(&full).with_context(|| format!("invalid URL `{full}`"))?;
        }
        Ok(full)
    }

    /// Appends the advice of the first `[errors.hints]` pattern found in the error.
    fn hinted(&self, error: String) -> String {
        let lower = error.to_lowercase();
        let hint = self.spec.errors.iter().flat_map(|e| &e.hints).find(|(p, _)| lower.contains(&p.to_lowercase()));
        match hint {
            Some((_, hint)) => format!("{error} — fix: {hint}"),
            None => error,
        }
    }

    /// `output_url` rendered with an output object's fields (percent-encoded); relative results
    /// join `base_url` and are downloaded with credentials.
    fn object_url(&self, step: &Step, name: &str, fields: &Map<String, Value>) -> Result<Option<Output>> {
        let Some(template) = &step.output_url else { return Ok(None) };
        let vars = fields.iter().map(|(k, v)| (k.clone(), json!(percent_encode(&as_text(v))))).collect();
        let url = self
            .url(template, &self.ctx(&vars, &Value::Null, self.mode()))
            .with_context(|| format!("{}: rendering output_url with output {}", self.at(name), Value::Object(fields.clone())))?;
        let relative = !template.starts_with("http://") && !template.starts_with("https://");
        Ok(Some(Output::Url { url, auth: relative || self.spec.download_auth }))
    }

    fn outputs(&self, name: &str, step: &Step, doc: &Doc) -> Result<Vec<Output>> {
        let mut found = Vec::new();
        let base = (!self.spec.base_url.contains("{{")).then(|| self.spec.base_url.trim_end_matches('/'));
        let object_url = |fields: &Map<String, Value>| self.object_url(step, name, fields);
        for pointer in &step.outputs {
            for value in doc.all(pointer) {
                collect_outputs(&value, self.spec.download_auth, base, &object_url, &mut found)?;
            }
        }
        for pointer in &step.outputs_base64 {
            for value in doc.all(pointer) {
                let (b64, mime) = match &value {
                    Value::String(s) => (Some(s.as_str()), None),
                    Value::Object(m) => (
                        BASE64_FIELDS.iter().find_map(|k| m.get(*k)?.as_str()),
                        MIME_FIELDS.iter().find_map(|k| m.get(*k)?.as_str()),
                    ),
                    _ => (None, None),
                };
                if let Some(b64) = b64 {
                    let data = STANDARD
                        .decode(b64.trim())
                        .with_context(|| format!("{}: invalid base64 at outputs_base64 `{pointer}`", self.at(name)))?;
                    let mime = mime.map(str::to_owned).or_else(|| step.output_mime.clone());
                    found.push(Output::Inline { data, mime });
                }
            }
        }
        let mut seen = std::collections::HashSet::new();
        found.retain(|o| match o {
            Output::Url { url, .. } => seen.insert(url.clone()),
            Output::Inline { .. } => true,
        });
        Ok(found)
    }
}

async fn send(http: &reqwest::Client, auth: &Authenticator, req: Request) -> Result<Response> {
    let method = Method::from_bytes(req.method.as_bytes())?;
    let url = Url::parse(&req.url)?;
    let mut builder = if req.auth { auth.request(http, method, url).await? } else { http.request(method, url) };
    let sets_own_type = matches!(req.payload, Payload::Form | Payload::FormData | Payload::Multipart { .. });
    for (name, value) in &req.headers {
        if !(sets_own_type && name == "content-type") {
            builder = builder.header(name, value);
        }
    }
    let body = req.body.unwrap_or(Value::Null);
    builder = match req.payload {
        Payload::None => builder,
        Payload::Json => builder.body(serde_json::to_vec(&body)?),
        Payload::Form => builder.form(&body),
        Payload::Bytes(bytes) => builder.body(bytes),
        Payload::FormData => {
            let mut form = Form::new();
            for (k, v) in body.as_object().into_iter().flatten() {
                match v {
                    Value::Array(items) => {
                        for item in items {
                            form = form.text(k.clone(), as_text(item));
                        }
                    }
                    other => form = form.text(k.clone(), as_text(other)),
                }
            }
            builder.multipart(form)
        }
        Payload::Multipart { file_field, file_name, mime, bytes } => {
            let mut form = Form::new();
            for (k, v) in body.as_object().into_iter().flatten().filter(|(k, _)| **k != file_field) {
                form = form.text(k.clone(), as_text(v));
            }
            form = form.part(file_field, Part::bytes(bytes).file_name(file_name).mime_str(&mime)?);
            builder.multipart(form)
        }
    };
    let resp = builder.send().await?;
    let status = resp.status().as_u16();
    let mut headers = BTreeMap::<String, String>::new();
    for (name, value) in resp.headers() {
        let value = String::from_utf8_lossy(value.as_bytes()).into_owned();
        headers
            .entry(name.as_str().to_owned())
            .and_modify(|v| {
                v.push_str(", ");
                v.push_str(&value);
            })
            .or_insert(value);
    }
    Ok(Response::new(status, headers, resp.text().await?))
}

/// Form fields plus the file: `body` describes the file for traces, `payload` carries it.
fn multipart(mut fields: Map<String, Value>, field: &str, file_name: &str, mime: &str, bytes: Vec<u8>) -> (Option<Value>, Payload) {
    fields.insert(field.to_owned(), json!(format!("<file {file_name}, {mime}, {} bytes>", bytes.len())));
    let payload = Payload::Multipart { file_field: field.to_owned(), file_name: file_name.to_owned(), mime: mime.to_owned(), bytes };
    (Some(Value::Object(fields)), payload)
}

fn pointed(doc: &Doc, pointer: &str, at: &str, what: &str) -> Result<Value> {
    doc.first(pointer)
        .ok_or_else(|| anyhow!("{at}: {what} pointer `{pointer}` matched nothing in the response: {}", snippet(&doc.body)))
}

fn starts_job(step: &str) -> bool {
    matches!(step, "prepare" | "submit")
}

fn capture(step: &Step, doc: &Doc, vars: &mut Map<String, Value>) {
    for (var, pointer) in &step.capture {
        if let Some(v) = doc.first(pointer) {
            vars.insert(var.clone(), v);
        }
    }
}

fn cost_of(step: &Step, doc: &Doc) -> Option<f64> {
    step.cost.as_deref().and_then(|p| doc.first(p)).and_then(|v| as_number(&v))
}

type ObjectUrl<'a> = dyn Fn(&Map<String, Value>) -> Result<Option<Output>> + 'a;

/// Relative paths (`/v1/files/1`) resolve against `base` and are downloaded with credentials;
/// objects without a URL field go through `object_url`.
fn collect_outputs(value: &Value, auth: bool, base: Option<&str>, object_url: &ObjectUrl, out: &mut Vec<Output>) -> Result<()> {
    match value {
        Value::String(s) if s.starts_with("http://") || s.starts_with("https://") => {
            out.push(Output::Url { url: s.clone(), auth });
        }
        Value::String(s) if s.starts_with("data:") => out.push(decode_data_uri(s)?),
        Value::String(s) if s.starts_with('/') && base.is_some() => {
            out.push(Output::Url { url: format!("{}{s}", base.unwrap_or_default()), auth: true });
        }
        Value::Array(items) => {
            for item in items {
                collect_outputs(item, auth, base, object_url, out)?;
            }
        }
        Value::Object(map) => match URL_FIELDS.iter().find_map(|k| map.get(*k)) {
            Some(v) => collect_outputs(v, auth, base, object_url, out)?,
            None => out.extend(object_url(map)?),
        },
        _ => {}
    }
    Ok(())
}

fn decode_data_uri(uri: &str) -> Result<Output> {
    let (head, data) = uri.split_once(',').ok_or_else(|| anyhow!("malformed data URI"))?;
    let mime = head.trim_start_matches("data:").split(';').next().filter(|m| !m.is_empty());
    let data = STANDARD.decode(data.trim()).context("invalid base64 in data URI")?;
    Ok(Output::Inline { data, mime: mime.map(str::to_owned) })
}

fn output_to_json(o: &Output) -> Value {
    match o {
        Output::Url { url, .. } => json!(url),
        Output::Inline { data, mime } => json!(format!(
            "data:{};base64,{}",
            mime.as_deref().unwrap_or("application/octet-stream"),
            STANDARD.encode(data)
        )),
    }
}

fn output_from_json(v: &Value, auth: bool) -> Option<Output> {
    let mut out = Vec::new();
    collect_outputs(v, auth, None, &|_| Ok(None), &mut out).ok()?;
    out.pop()
}

/// Every non-blank value the pointers find, deduplicated and joined with `; `.
fn texts(doc: &Doc, pointers: &[String]) -> Option<String> {
    let mut found: Vec<String> = Vec::new();
    for text in pointers.iter().flat_map(|p| doc.all(p)).filter(not_blank).map(|v| as_text(&v)) {
        if !found.contains(&text) {
            found.push(text);
        }
    }
    (!found.is_empty()).then(|| found.join("; "))
}

/// RFC 3986 unreserved characters stay, every other byte becomes `%XX`.
fn percent_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn one_of(values: &[Value], status: &str) -> bool {
    values.iter().any(|v| as_text(v).eq_ignore_ascii_case(status))
}

fn not_blank(v: &Value) -> bool {
    match v {
        Value::String(s) => !s.trim().is_empty(),
        Value::Object(m) => !m.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Bool(b) => *b,
        _ => true,
    }
}

fn method_name(method: Option<&str>, default: &str) -> String {
    method.unwrap_or(default).to_ascii_uppercase()
}

fn snippet(v: &Value) -> String {
    truncate(&v.to_string())
}

fn truncate(s: &str) -> String {
    const MAX: usize = 400;
    match s.char_indices().nth(MAX) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use axum::{
        Json, Router,
        extract::{Path, State},
        http::HeaderMap,
        routing::{get, post},
    };

    use super::*;

    const SPEC: &str = r#"
key = "mock"
name = "Mock"
catalog = []
docs = "-"
model_hint = "-"
base_url = "BASE"

[auth]
type = "bearer"
env = ["MOCK_KEY"]

[errors]
code = "/code"
ok = [200]
message = "/msg"

[submit]
url = "/submit"
body = { model = "{{model}}", input = "{{input}}" }
capture = { id = "/data/taskId", status_url = "/data/status_url" }

[poll]
url = "{{status_url}}"
status = "/data/state"
success = ["DONE"]
failure = ["failed"]
error = "/data/err"
cost = "/data/cost"
capture = { file = "/data/file" }

[result]
url = "/files/{{file}}"
outputs = ["/result#/urls"]
cost = "header:X-Credits"

[[route]]
models = ["sync/*"]
[route.submit]
url = "/sync"
outputs = ["/out"]
"#;

    type Hits = Arc<AtomicUsize>;

    fn authed(h: &HeaderMap) -> bool {
        h.get("authorization").and_then(|v| v.to_str().ok()) == Some("Bearer k")
    }

    async fn mock() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let b = base.clone();
        let router = Router::new()
            .route(
                "/submit",
                post(move |h: HeaderMap, Json(body): Json<Value>| async move {
                    if !authed(&h) || body["input"]["prompt"] != "cat" {
                        return Json(json!({ "code": 401, "msg": "bad auth or body" }));
                    }
                    if body["model"] == "bad" {
                        return Json(json!({ "code": 422, "msg": "unknown model" }));
                    }
                    Json(json!({ "code": 200, "data": { "taskId": "t1", "status_url": format!("{b}/status/t1") } }))
                }),
            )
            .route(
                "/status/{id}",
                get(|State(hits): State<Hits>, Path(id): Path<String>| async move {
                    assert_eq!(id, "t1");
                    match hits.fetch_add(1, Ordering::SeqCst) {
                        0 => Json(json!({ "code": 200, "data": { "state": "queued" } })),
                        _ => Json(json!({ "code": 200, "data": { "state": "done", "cost": "1.5", "file": "f9", "err": "" } })),
                    }
                }),
            )
            .route(
                "/files/{file}",
                get(|Path(file): Path<String>| async move {
                    let body = Json(json!({ "code": 200, "result": format!("{{\"urls\":[\"https://cdn.test/{file}.mp4\"]}}") }));
                    ([("x-credits", "7")], body)
                }),
            )
            .route("/sync", post(|| async { Json(json!({ "code": 200, "out": "data:image/png;base64,aGk=" })) }))
            .with_state(Hits::default());
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        base
    }

    async fn provider() -> Provider {
        let base = mock().await;
        let spec = Spec::parse(&SPEC.replace("BASE", &base)).unwrap();
        Provider::new(spec, Origin::Candidate, Env::from_pairs([("MOCK_KEY", "k")]), reqwest::Client::new())
    }

    #[tokio::test]
    async fn submit_poll_result_flow() {
        let p = provider().await;
        let input = json!({ "prompt": "cat" });
        let mut trace = Trace::new();
        let vars = p.submit("video/x", &input, &mut trace).await.unwrap();
        assert_eq!(vars["id"], "t1");

        assert!(matches!(p.poll(&vars, &mut trace).await.unwrap(), TaskState::Pending(s) if s.contains("queued")));
        let TaskState::Done { outputs, cost } = p.poll(&vars, &mut trace).await.unwrap() else { panic!("expected done") };
        assert_eq!(cost, Some(7.0), "result header cost wins over poll cost");
        assert!(matches!(&outputs[..], [Output::Url { url, auth: false }] if url == "https://cdn.test/f9.mp4"));
        let steps: Vec<&str> = trace.iter().map(|e| e.step.as_str()).collect();
        assert_eq!(steps, ["submit", "poll", "poll", "result"]);
        assert_eq!(trace[0].request.headers["authorization"], "Bearer <MOCK_KEY>", "secrets never reach the trace");

        let err = format!("{:#}", p.submit("bad", &input, &mut trace).await.unwrap_err());
        assert!(err.contains("mock submit") && err.contains("422") && err.contains("unknown model"), "{err}");
    }

    #[tokio::test]
    async fn sync_route_returns_inline_output() {
        let p = provider().await;
        let mut trace = Trace::new();
        let vars = p.submit("sync/img", &json!({ "prompt": "cat" }), &mut trace).await.unwrap();
        let TaskState::Done { outputs, .. } = p.poll(&vars, &mut trace).await.unwrap() else { panic!("expected done") };
        assert!(matches!(&outputs[..], [Output::Inline { data, mime: Some(m) }] if data == b"hi" && m == "image/png"));
    }

    #[test]
    fn dry_run_shows_requests_without_secrets() {
        let spec = Spec::parse(&SPEC.replace("BASE", "https://x.test")).unwrap();
        let p = Provider::new(spec, Origin::Candidate, Env::default(), reqwest::Client::new());
        assert_eq!(p.missing_env(), vec!["MOCK_KEY"]);
        let reqs = p.dry_run("video/x", &json!({ "prompt": "cat" }), &Map::new()).unwrap();
        assert_eq!(reqs[0].request.url, "https://x.test/submit");
        assert_eq!(reqs[0].request.body, Some(json!({ "model": "video/x", "input": { "prompt": "cat" } })));
        assert_eq!(reqs[1].request.url, "<status_url from submit /data/status_url>");
        assert_eq!(reqs[2].request.url, "https://x.test/files/<file from poll /data/file>");
    }
}
