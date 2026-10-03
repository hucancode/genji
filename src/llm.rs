use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::time::Duration;

use crate::config::{Config, ModelRuntime, ProviderConfig};

crate::storage::string_enum! {
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub enum Role {
        System => "system",
        User => "user",
        #[default]
        Assistant => "assistant",
        Tool => "tool",
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "reasoning_is_empty")]
    pub reasoning_content: Option<String>,
}

fn reasoning_is_empty(value: &Option<String>) -> bool {
    value.as_deref().is_none_or(str::is_empty)
}

#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

impl Serialize for ToolCall {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        use serde::Serialize;
        #[derive(Serialize)]
        struct Function<'a> {
            name: &'a str,
            arguments: &'a str,
        }
        #[derive(Serialize)]
        struct Wire<'a> {
            id: &'a str,
            #[serde(rename = "type")]
            kind: &'static str,
            function: Function<'a>,
        }
        Wire {
            id: &self.id,
            kind: "function",
            function: Function {
                name: &self.name,
                arguments: &self.arguments,
            },
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ToolCall {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Function {
            name: String,
            arguments: String,
        }
        #[derive(Deserialize)]
        struct Wire {
            id: String,
            function: Function,
        }
        let w = Wire::deserialize(d)?;
        Ok(ToolCall {
            id: w.id,
            name: w.function.name,
            arguments: w.function.arguments,
        })
    }
}

#[derive(Debug, Deserialize)]
struct ResponseChoice {
    #[serde(default)]
    message: WireMessage,
    finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct WireMessage {
    content: Option<String>,
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<WireToolCall>,
}

#[derive(Debug, Deserialize)]
struct WireToolCall {
    id: Option<String>,
    function: WireFunction,
}

#[derive(Debug, Default, Deserialize)]
struct WireFunction {
    name: Option<String>,
    arguments: Option<Value>,
}

#[derive(Debug, Default, Deserialize)]
struct Usage {
    prompt_tokens: i64,
    completion_tokens: i64,
    #[serde(default)]
    prompt_tokens_details: PromptDetails,
}

#[derive(Debug, Default, Deserialize)]
struct PromptDetails {
    #[serde(default)]
    cached_tokens: i64,
}

#[derive(Debug, Deserialize)]
struct WireResponse {
    choices: Vec<ResponseChoice>,
    #[serde(default)]
    usage: Usage,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: content.into(),
            ..Default::default()
        }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            ..Default::default()
        }
    }
    #[cfg(test)]
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            ..Default::default()
        }
    }
    pub fn tool_result(tool_call_id: &str, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            tool_call_id: Some(tool_call_id.to_string()),
            ..Default::default()
        }
    }

    pub fn est_tokens(&self) -> i64 {
        // Byte length over-counts non-ASCII text, which is fine for an estimate.
        let mut bytes = self.content.len() + self.reasoning_content.as_ref().map_or(0, String::len);
        for call in &self.tool_calls {
            bytes += call.name.len() + call.arguments.len() + 16;
        }
        estimate_chars(bytes)
    }
}

pub fn estimate_chars(chars: usize) -> i64 {
    i64::try_from(chars / 4)
        .unwrap_or(i64::MAX)
        .saturating_add(4)
}

#[derive(Debug)]
pub struct LlmResponse {
    pub message: ChatMessage,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub cached_tokens: i64,
    pub finish_reason: Option<String>,
}

impl LlmResponse {
    pub fn is_truncated(&self) -> bool {
        self.finish_reason.as_deref() == Some("length")
    }
}

pub fn estimate_messages(messages: &[ChatMessage]) -> i64 {
    messages.iter().map(ChatMessage::est_tokens).sum::<i64>() + 8
}

/// The live context plus an optional one-request hint, serialized as one array
/// without copying the context. The hint is never stored.
struct Messages<'a>(&'a [ChatMessage], Option<ChatMessage>);

impl Serialize for Messages<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(self.0.iter().chain(self.1.iter()))
    }
}

/// Chat-completions request body, serialized straight from the live context.
#[derive(Serialize)]
struct Request<'a> {
    model: &'a str,
    messages: Messages<'a>,
    stream: bool,
    #[serde(skip_serializing_if = "<[Value]>::is_empty")]
    tools: &'a [Value],
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<&'static str>,
    #[serde(flatten)]
    max_tokens: BTreeMap<&'a str, i64>,
}

pub struct LlmClient {
    pub model: String,
    max_tokens_field: String,
    max_tokens: i64,
    send_tool_choice: bool,
    max_retries: u32,
    url: String,
    headers: Vec<(String, String)>,
    agent: ureq::Agent,
}

impl LlmClient {
    pub fn from_runtime(cfg: &Config, runtime: ModelRuntime) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(cfg.time_limit_secs.max(60) + 120))
            .build();
        let api_key = runtime.provider.resolve_api_key();
        let mut headers = Vec::new();
        if !api_key.is_empty() {
            headers.push(if runtime.provider.auth == "api-key" {
                ("api-key".to_string(), api_key)
            } else {
                ("Authorization".to_string(), format!("Bearer {api_key}"))
            });
        }
        headers.extend(runtime.provider.extra_headers.clone());
        Self {
            url: endpoint(&runtime.provider, &runtime.model),
            headers,
            max_tokens_field: runtime.provider.max_tokens_field,
            send_tool_choice: runtime.provider.send_tool_choice,
            max_retries: cfg.llm_max_retries,
            model: runtime.model,
            max_tokens: runtime.limits.max_output_tokens,
            agent,
        }
    }

    /// `hint` is sent as a trailing user message for this request only.
    pub fn chat(
        &self,
        messages: &[ChatMessage],
        tools: &[Value],
        hint: Option<&str>,
    ) -> Result<LlmResponse> {
        let body = serde_json::to_vec(&Request {
            model: &self.model,
            messages: Messages(messages, hint.map(|h| ChatMessage::user(format!("[note] {h}")))),
            stream: false,
            tools,
            tool_choice: (self.send_tool_choice && !tools.is_empty()).then_some("auto"),
            max_tokens: BTreeMap::from([(self.max_tokens_field.as_str(), self.max_tokens)]),
        })?;
        let mut attempt = 0u32;
        loop {
            match self.post_once(&body) {
                Ok(resp) => return Ok(resp),
                Err(f) if f.retry && attempt < self.max_retries => {
                    let wait = f.retry_after.unwrap_or_else(|| backoff(attempt));
                    attempt += 1;
                    eprintln!(
                        "[llm] retry {attempt}/{} after error ({:#}), sleeping {wait:?}",
                        self.max_retries, f.error
                    );
                    std::thread::sleep(wait);
                }
                Err(f) => return Err(f.error),
            }
        }
    }

    /// One request; the failure says whether it is worth retrying.
    fn post_once(&self, body: &[u8]) -> Result<LlmResponse, Failure> {
        let fail = |error, retry| Failure {
            error,
            retry,
            retry_after: None,
        };
        let mut req = self
            .agent
            .post(&self.url)
            .set("Content-Type", "application/json");
        for (k, v) in &self.headers {
            req = req.set(k, v);
        }
        match req.send_bytes(body) {
            Ok(r) => {
                let text = r
                    .into_string()
                    .map_err(|e| fail(anyhow!("reading llm response: {e}"), true))?;
                // A garbled 200 (truncated JSON, a proxy's HTML page) is transient.
                parse_response(&text).map_err(|e| fail(e, true))
            }
            Err(ureq::Error::Status(code, r)) => {
                let retry_after = r
                    .header("retry-after")
                    .and_then(|v| v.trim().parse::<u64>().ok())
                    .map(|s| Duration::from_secs(s.min(60)));
                let txt = r.into_string().unwrap_or_default();
                Err(Failure {
                    error: anyhow!("HTTP {code}: {}", txt.chars().take(600).collect::<String>()),
                    retry: matches!(code, 408 | 409 | 429) || code >= 500,
                    retry_after,
                })
            }
            Err(e) => Err(fail(anyhow!("transport error: {e}"), true)),
        }
    }
}

struct Failure {
    error: anyhow::Error,
    retry: bool,
    retry_after: Option<Duration>,
}

/// Exponential backoff capped at 30s, with up to 25% jitter.
fn backoff(attempt: u32) -> Duration {
    let base = 1000u64 << attempt.min(5);
    let base = base.min(30_000);
    let jitter = crate::storage::util::unix_millis() % (base / 4 + 1);
    Duration::from_millis(base + jitter)
}

/// Endpoint URL for the provider kind.
///
/// * openai-compatible (llama.cpp, `DeepSeek`, `OpenAI`, …): `{base}/chat/completions`
/// * azure: `{base}/openai/deployments/{deployment}/chat/completions?api-version=…`
fn endpoint(provider: &ProviderConfig, model: &str) -> String {
    let base = provider.base_url.trim_end_matches('/');
    let mut url = if provider.is_azure() {
        format!(
            "{base}/openai/deployments/{}/chat/completions",
            url_encode(model)
        )
    } else {
        format!("{base}/chat/completions")
    };
    let api_version = (provider.is_azure() && !provider.api_version.is_empty())
        .then_some(("api-version", &provider.api_version));
    let query = api_version
        .into_iter()
        .chain(provider.extra_query.iter().map(|(k, v)| (k.as_str(), v)))
        .map(|(k, v)| format!("{}={}", url_encode(k), url_encode(v)))
        .collect::<Vec<_>>()
        .join("&");
    if !query.is_empty() {
        url.push('?');
        url.push_str(&query);
    }
    url
}

fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}

fn parse_response(text: &str) -> Result<LlmResponse> {
    let parsed: WireResponse = serde_json::from_str(text)
        .with_context(|| format!("parsing llm response: {}", truncate(text, 400)))?;
    let choice = parsed
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("llm response has no choices: {}", truncate(text, 400)))?;
    let tool_calls = choice
        .message
        .tool_calls
        .into_iter()
        .enumerate()
        .map(|(i, call)| {
            let arguments = call.function.arguments.map_or_else(
                || "{}".to_string(),
                |value| {
                    value
                        .as_str()
                        .map_or_else(|| value.to_string(), ToString::to_string)
                },
            );
            ToolCall {
                id: call.id.unwrap_or_else(|| format!("call_{i}")),
                name: call.function.name.unwrap_or_default(),
                arguments,
            }
        })
        .collect();
    Ok(LlmResponse {
        message: ChatMessage {
            role: Role::Assistant,
            content: choice.message.content.unwrap_or_default(),
            tool_calls,
            tool_call_id: None,
            reasoning_content: choice.message.reasoning_content,
        },
        prompt_tokens: parsed.usage.prompt_tokens,
        completion_tokens: parsed.usage.completion_tokens,
        cached_tokens: parsed.usage.prompt_tokens_details.cached_tokens,
        finish_reason: choice.finish_reason,
    })
}

pub fn truncate(s: &str, max: usize) -> Cow<'_, str> {
    if s.len() <= max {
        return Cow::Borrowed(s);
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    Cow::Owned(format!(
        "{}… [{} bytes truncated]",
        &s[..end],
        s.len() - end
    ))
}

#[cfg(test)]
mod tests {
    use super::{ChatMessage, Request, truncate, url_encode};

    #[test]
    fn truncates_on_char_boundary() {
        let s = "é".repeat(50);
        let out = truncate(&s, 11);
        assert!(out.contains("truncated"));
        assert!(out.len() < 50 * 2 + 40);
    }

    #[test]
    fn keeps_short_strings() {
        assert_eq!(truncate("hi", 10), "hi");
    }

    #[test]
    fn encodes_query_values() {
        assert_eq!(url_encode("2024-10-21"), "2024-10-21");
        assert_eq!(url_encode("a b/c"), "a%20b%2Fc");
    }

    #[test]
    fn parses_cached_tokens() {
        let body = r#"{"choices":[{"message":{"content":"x"}}],"usage":{"prompt_tokens":10,"completion_tokens":2,"prompt_tokens_details":{"cached_tokens":8}}}"#;
        assert_eq!(super::parse_response(body).unwrap().cached_tokens, 8);
    }

    #[test]
    fn detects_truncation() {
        let mut resp = super::LlmResponse {
            message: ChatMessage::default(),
            prompt_tokens: 0,
            completion_tokens: 0,
            cached_tokens: 0,
            finish_reason: Some("length".into()),
        };
        assert!(resp.is_truncated());
        resp.finish_reason = Some("stop".into());
        assert!(!resp.is_truncated());
        resp.finish_reason = None;
        assert!(!resp.is_truncated());
    }

    #[test]
    fn hint_is_one_trailing_user_message() {
        let msgs = [ChatMessage::user("hi")];
        let out = serde_json::to_value(super::Messages(
            &msgs,
            Some(ChatMessage::user("[note] x")),
        ))
        .unwrap();
        assert_eq!(out.as_array().unwrap().len(), 2);
        assert_eq!(out[0]["content"], "hi");
        assert_eq!(out[1]["role"], "user");
        assert_eq!(out[1]["content"], "[note] x");
    }

    #[test]
    fn messages_round_trip_through_json() {
        let mut m = ChatMessage::assistant("a");
        m.tool_calls.push(super::ToolCall {
            id: "c1".into(),
            name: "read".into(),
            arguments: "{}".into(),
        });
        let back: ChatMessage =
            serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
        assert_eq!(back.role, m.role);
        assert_eq!(back.tool_calls[0].id, "c1");
        assert_eq!(back.tool_calls[0].arguments, "{}");
        let t: ChatMessage = serde_json::from_str(
            &serde_json::to_string(&ChatMessage::tool_result("c1", "ok")).unwrap(),
        )
        .unwrap();
        assert_eq!(t.tool_call_id.as_deref(), Some("c1"));
    }

    #[test]
    fn backoff_is_capped() {
        assert!(super::backoff(0).as_millis() >= 1000);
        assert!(super::backoff(20).as_millis() <= 37_500);
    }

    #[test]
    fn request_body_wire_format() {
        let msgs = [ChatMessage::user("hi")];
        let body = |tools: &[serde_json::Value], choice| {
            serde_json::to_value(Request {
                model: "m",
                messages: super::Messages(&msgs, None),
                stream: false,
                tools,
                tool_choice: choice,
                max_tokens: std::collections::BTreeMap::from([("max_completion_tokens", 9)]),
            })
            .unwrap()
        };
        let bare = body(&[], None);
        assert_eq!(bare["max_completion_tokens"], 9);
        assert_eq!(bare["messages"][0]["content"], "hi");
        assert!(bare.get("tools").is_none() && bare.get("tool_choice").is_none());
        let with = body(&[serde_json::json!({"type": "function"})], Some("auto"));
        assert_eq!(with["tools"][0]["type"], "function");
        assert_eq!(with["tool_choice"], "auto");
    }
}
