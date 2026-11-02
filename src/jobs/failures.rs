//! Why a job failed, as a kind the agent can act on. Classified from the provider's own words
//! (built-in patterns plus a spec's `[errors.kinds]`), then the HTTP status (or a 3-digit
//! `[errors].code`), then the engine's own messages: a pointer that matched nothing is a spec problem.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    Moderation,
    Auth,
    Quota,
    RateLimit,
    InvalidInput,
    NotFound,
    ProviderError,
    Network,
    Timeout,
    Spec,
    Cancelled,
}

/// Spec-declared patterns per kind, matched case-insensitively as substrings.
pub type Patterns = BTreeMap<ErrorKind, Vec<String>>;

/// Checked in this order, before the HTTP status: a 400 saying "content policy" is moderation.
const PATTERNS: &[(ErrorKind, &[&str])] = &[
    (ErrorKind::Moderation, &[
        "content policy",
        "content_policy",
        "moderation",
        "nsfw",
        "safety system",
        "safety filter",
        "flagged",
        "inappropriate",
        "prohibited content",
        "sensitive content",
        "violates",
    ]),
    (ErrorKind::Quota, &[
        "insufficient",
        "not enough credit",
        "out of credit",
        "no credits",
        "credit balance",
        "account balance",
        "quota",
        "payment required",
        "billing",
        "top up",
    ]),
    (ErrorKind::RateLimit, &["rate limit", "rate_limit", "ratelimit", "too many requests", "concurrency limit"]),
    (ErrorKind::Auth, &[
        "unauthorized",
        "unauthenticated",
        "invalid api key",
        "invalid_api_key",
        "incorrect api key",
        "authentication",
        "permission denied",
        "uses credentials",
        "not signed in",
    ]),
];

/// Messages the engine itself writes when the response does not fit the spec (providers/engine.rs).
const SPEC_MARKERS: &[&str] = &["matched nothing", "matched no media", "invalid base64", "non-JSON response", "no [poll] step"];
const TIMEOUT_MARKERS: &[&str] = &["timed out", "timeout", "gave up after 24h"];
const NETWORK_MARKERS: &[&str] = &["error sending request", "error trying to connect", "tcp connect", "dns error", "connection"];

impl ErrorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Moderation => "moderation",
            Self::Auth => "auth",
            Self::Quota => "quota",
            Self::RateLimit => "rate_limit",
            Self::InvalidInput => "invalid_input",
            Self::NotFound => "not_found",
            Self::ProviderError => "provider_error",
            Self::Network => "network",
            Self::Timeout => "timeout",
            Self::Spec => "spec",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        serde_json::from_value(serde_json::Value::String(s.to_owned())).ok()
    }

    /// Whether the same call can succeed later without changing anything.
    pub fn retryable(self) -> bool {
        matches!(self, Self::RateLimit | Self::ProviderError | Self::Network | Self::Timeout)
    }

    pub fn hint(self) -> &'static str {
        match self {
            Self::Moderation => "The provider refused the content: rewrite the prompt (or replace the input image) and generate \
                again; the same input fails the same way.",
            Self::Auth => "The API key is missing or was refused: run `ai-media-router keys set NAME` in a terminal (the value is \
                read from a hidden prompt, never from the conversation), or set it in the environment; for account sign-in run \
                `ai-media-router login PROVIDER` instead; list_providers shows what is set; it applies without a restart.",
            Self::Quota => "The account is out of credits or over its quota: top up with the provider, or switch to another \
                provider (search_prices with configured_only).",
            Self::RateLimit => "Rate limited: wait a minute and generate again, or switch provider.",
            Self::InvalidInput => "The provider rejected the input: compare model and input with get_provider's example and the \
                provider docs, fix them and generate again.",
            Self::NotFound => "The model or endpoint does not exist: check the model id against get_provider's model_hint and docs.",
            Self::ProviderError => "The provider failed on its side: generate again later or switch provider.",
            Self::Network => "The provider could not be reached: check the network and the spec's base_url, then generate again.",
            Self::Timeout => "The provider did not answer in time: generate again, or switch provider.",
            Self::Spec => "The response no longer matches the provider spec: get_job with debug=true shows the raw response, \
                then fix the pointer with get_spec -> check_spec -> save_spec.",
            Self::Cancelled => "The job was cancelled; generate again if it is still needed.",
        }
    }
}

pub fn classify(message: &str, extra: Option<&Patterns>) -> ErrorKind {
    let text = message.to_lowercase();
    if text.starts_with("cancelled") {
        return ErrorKind::Cancelled;
    }
    let custom = extra.into_iter().flatten().map(|(kind, words)| (*kind, words.iter().map(String::as_str).collect::<Vec<_>>()));
    let builtin = PATTERNS.iter().map(|(kind, words)| (*kind, words.to_vec()));
    for (kind, words) in custom.chain(builtin) {
        if words.iter().any(|w| text.contains(&w.to_lowercase())) {
            return kind;
        }
    }
    if let Some(kind) = status_code(&text).and_then(by_status) {
        return kind;
    }
    let has = |markers: &[&str]| markers.iter().any(|m| text.contains(&m.to_lowercase()));
    if has(SPEC_MARKERS) {
        ErrorKind::Spec
    } else if has(TIMEOUT_MARKERS) {
        ErrorKind::Timeout
    } else if has(NETWORK_MARKERS) {
        ErrorKind::Network
    } else {
        ErrorKind::ProviderError
    }
}

/// `HTTP 429: …` from the engine, or `provider error code `402`` when a provider puts HTTP-like codes in the body.
fn status_code(text: &str) -> Option<u16> {
    ["http ", "provider error code `"].iter().find_map(|prefix| {
        let rest = &text[text.find(prefix)? + prefix.len()..];
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        (digits.len() == 3).then(|| digits.parse().ok()).flatten()
    })
}

fn by_status(status: u16) -> Option<ErrorKind> {
    Some(match status {
        401 | 403 => ErrorKind::Auth,
        402 => ErrorKind::Quota,
        404 | 410 => ErrorKind::NotFound,
        408 | 504 => ErrorKind::Timeout,
        429 => ErrorKind::RateLimit,
        400 | 409 | 413 | 415 | 422 => ErrorKind::InvalidInput,
        500..=599 => ErrorKind::ProviderError,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_beat_status_and_spec_patterns_beat_builtin() {
        use ErrorKind::*;
        let cases = [
            ("kie submit: HTTP 400: {\"msg\":\"prompt violates our content policy\"}", Moderation),
            ("HTTP 401: {\"error\":\"bad key\"}", Auth),
            ("HTTP 402: {}", Quota),
            ("provider error code `433` at [errors].code `/code`: Insufficient credits", Quota),
            ("provider error code `429` at [errors].code `/code`: slow", RateLimit),
            ("HTTP 429: {}", RateLimit),
            ("HTTP 422: {\"detail\":\"duration must be 5 or 10\"}", InvalidInput),
            ("HTTP 404: no such model", NotFound),
            ("HTTP 503: upstream", ProviderError),
            ("mock submit (override x): capture `id` pointer `/id` matched nothing in the response: {}", Spec),
            ("error sending request for url (http://127.0.0.1:9/submit): tcp connect error", Network),
            ("gave up after 24h without a result", Timeout),
            ("provider status `failed`", ProviderError),
            ("cancelled", Cancelled),
        ];
        for (msg, want) in cases {
            assert_eq!(classify(msg, None), want, "{msg}");
        }
        let extra = Patterns::from([(Moderation, vec!["Banned Phrase".to_owned()])]);
        assert_eq!(classify("HTTP 400: prompt has a banned phrase", Some(&extra)), Moderation);
        assert_eq!(classify("HTTP 400: prompt has a banned phrase", None), InvalidInput);
    }
}
