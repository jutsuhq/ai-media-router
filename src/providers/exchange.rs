//! One HTTP round trip of a provider step. The same `Request` value is sent live, shown by dry
//! runs, stored in a job's trace and compared against recorded fixtures, so all four agree.

use std::{
    collections::{BTreeMap, VecDeque},
    sync::Mutex,
};

use anyhow::{Result, anyhow, ensure};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::template::resolve;

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct Request {
    pub method: String,
    pub url: String,
    /// Lowercase names. In traces and dry runs, auth values appear as `<ENV_NAME>`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// JSON body, form fields, or a description of the file being uploaded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<Value>,
    #[serde(skip)]
    pub payload: Payload,
    /// Whether the provider's credentials are attached (false for presigned upload URLs).
    #[serde(skip)]
    pub auth: bool,
}

#[derive(Debug, Clone, Default)]
pub enum Payload {
    #[default]
    None,
    Json,
    Form,
    /// `body` object as multipart text fields.
    FormData,
    /// Text fields come from `body`; the file goes into `file_field`.
    Multipart { file_field: String, file_name: String, mime: String, bytes: Vec<u8> },
    Bytes(Vec<u8>),
}

/// What auth adds to a request, with every secret shown as `<ENV_NAME>`.
pub struct AuthPlaceholders {
    pub headers: Vec<(String, String)>,
    pub query: Option<(String, String)>,
}

impl Request {
    /// The request without file bytes, with auth placeholders added.
    pub fn record(&self, auth: AuthPlaceholders) -> Self {
        let mut record = Self {
            method: self.method.clone(),
            url: self.url.clone(),
            headers: self.headers.clone(),
            body: self.body.clone(),
            payload: Payload::None,
            auth: self.auth,
        };
        if self.auth {
            record.headers.extend(auth.headers);
            if let Some((param, value)) = auth.query {
                let sep = if record.url.contains('?') { '&' } else { '?' };
                record.url = format!("{}{sep}{param}={value}", record.url);
            }
        }
        record
    }
}

/// A response as seen by the engine; `headers` names are lowercase. `json` is `text` parsed once,
/// `None` when it is blank or not JSON.
pub struct Response {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub text: String,
    pub json: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ResponseRecord {
    pub status: u16,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// Parsed JSON, or the raw text when the body is not JSON.
    #[serde(default)]
    pub body: Value,
}

/// One step's request and what came back. A job's trace and a fixture's `exchanges` use this shape,
/// so a trace from a real job can be pasted into a fixture file.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Exchange {
    /// `prepare`, `submit`, `poll`, `result`, `upload`, `upload_put`, `upload_post` or `upload_complete`.
    pub step: String,
    pub request: Request,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<ResponseRecord>,
    /// Network-level failure (no response).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub type Trace = Vec<Exchange>;

impl Response {
    pub fn new(status: u16, headers: BTreeMap<String, String>, text: String) -> Self {
        let json = if text.trim().is_empty() { None } else { serde_json::from_str(&text).ok() };
        Self { status, headers, text, json }
    }

    pub fn record(&self) -> ResponseRecord {
        let mut body = self.json.clone().unwrap_or_else(|| json!(self.text));
        elide(&mut body);
        let headers = self.headers.iter().filter(|(k, _)| *k != "set-cookie").map(|(k, v)| (k.clone(), v.clone())).collect();
        ResponseRecord { status: self.status, headers, body }
    }
}

impl Exchange {
    pub fn new(step: &str, mut request: Request, response: Result<&Response, String>) -> Self {
        if let Some(body) = &mut request.body {
            elide(body);
        }
        let (response, error) = match response {
            Ok(r) => (Some(r.record()), None),
            Err(e) => (None, Some(e)),
        };
        Self { step: step.to_owned(), request, response, error }
    }
}

/// A parsed response body plus headers, addressed by spec pointers (`/json/pointer` or `header:Name`).
pub struct Doc {
    pub body: Value,
    pub headers: BTreeMap<String, String>,
}

impl Doc {
    pub fn all(&self, pointer: &str) -> Vec<Value> {
        match pointer.strip_prefix("header:") {
            Some(name) => self.headers.get(&name.trim().to_ascii_lowercase()).map(|v| json!(v)).into_iter().collect(),
            None => resolve(&self.body, pointer),
        }
    }

    pub fn first(&self, pointer: &str) -> Option<Value> {
        self.all(pointer).into_iter().next()
    }
}

pub enum Transport {
    Http(reqwest::Client),
    /// Answers each request with the next recorded exchange after checking it matches.
    Replay(Mutex<VecDeque<Exchange>>),
}

/// Checks a request against the next recorded one and returns the recorded response.
pub fn replay(queue: &Mutex<VecDeque<Exchange>>, step: &str, actual: &Request) -> Result<Response> {
    let mut queue = queue.lock().unwrap_or_else(|e| e.into_inner());
    let next = queue.pop_front().ok_or_else(|| anyhow!("fixture has no recorded exchange left for `{step}`"))?;
    let want = &next.request;
    ensure!(next.step == step, "fixture expects step `{}` next, the engine made `{step}`", next.step);
    ensure!(
        want.method.eq_ignore_ascii_case(&actual.method) && want.url == actual.url,
        "request is {} {}, fixture recorded {} {}",
        actual.method,
        actual.url,
        want.method,
        want.url
    );
    for (name, value) in &want.headers {
        let got = actual.headers.get(&name.to_ascii_lowercase());
        ensure!(got == Some(value), "header `{name}` is {got:?}, fixture recorded {value:?}");
    }
    if let Some(body) = &want.body
        && let Some(diff) = diff(body, actual.body.as_ref().unwrap_or(&Value::Null), "")
    {
        return Err(anyhow!("request body differs from the fixture: {diff}"));
    }
    let r = next.response.ok_or_else(|| anyhow!("fixture exchange `{step}` has no response"))?;
    let headers = r.headers.into_iter().map(|(k, v)| (k.to_ascii_lowercase(), v)).collect();
    Ok(match r.body {
        Value::String(text) => Response::new(r.status, headers, text),
        body => Response { status: r.status, headers, text: body.to_string(), json: Some(body) },
    })
}

/// First difference between two JSON values, as `pointer: expected X, got Y`.
fn diff(want: &Value, got: &Value, at: &str) -> Option<String> {
    match (want, got) {
        (Value::Object(w), Value::Object(g)) => w
            .iter()
            .find_map(|(k, wv)| diff(wv, g.get(k).unwrap_or(&Value::Null), &format!("{at}/{k}")))
            .or_else(|| g.keys().find(|k| !w.contains_key(*k)).map(|k| format!("{at}/{k}: unexpected field"))),
        (Value::Array(w), Value::Array(g)) if w.len() == g.len() => {
            w.iter().zip(g).enumerate().find_map(|(i, (wv, gv))| diff(wv, gv, &format!("{at}/{i}")))
        }
        _ if want == got => None,
        _ => Some(format!("{}: expected {want}, got {got}", if at.is_empty() { "/" } else { at })),
    }
}

/// Shortens long strings (inline base64 media) so traces stay readable and small.
fn elide(v: &mut Value) {
    const MAX: usize = 300;
    match v {
        Value::String(s) if s.len() > MAX => {
            let head: String = s.chars().take(60).collect();
            *s = format!("{head}…<{} chars elided>", s.len());
        }
        Value::Array(items) => items.iter_mut().for_each(elide),
        Value::Object(map) => map.values_mut().for_each(elide),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_names_the_first_mismatch() {
        let want = json!({ "model": "a", "input": { "prompt": "cat", "n": 1 } });
        assert_eq!(diff(&want, &want.clone(), ""), None);
        let got = json!({ "model": "a", "input": { "prompt": "dog", "n": 1 } });
        assert_eq!(diff(&want, &got, "").unwrap(), "/input/prompt: expected \"cat\", got \"dog\"");
        let got = json!({ "model": "a", "input": { "prompt": "cat", "n": 1 }, "x": 1 });
        assert_eq!(diff(&want, &got, "").unwrap(), "/x: unexpected field");
    }

    #[test]
    fn doc_reads_headers_and_body() {
        let doc = Doc { body: json!({ "a": [1] }), headers: BTreeMap::from([("x-cost".into(), "3".into())]) };
        assert_eq!(doc.first("header:X-Cost"), Some(json!("3")));
        assert_eq!(doc.first("/a/0"), Some(json!(1)));
    }
}
