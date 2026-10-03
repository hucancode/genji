use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize, Serializer};
use serde_json::Value;
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::time::Duration;

use crate::config::Provider;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    #[default]
    Assistant,
    Tool,
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

/// A tool call in chat-completions wire shape. `arguments` is kept as the raw
/// string the model produced so a replayed request is byte-identical.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type", default = "function")]
    pub kind: String,
    pub function: Function,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Function {
    pub name: String,
    pub arguments: String,
}

fn function() -> String {
    "function".into()
}

impl ToolCall {
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            kind: function(),
            function: Function {
                name: name.into(),
                arguments: arguments.into(),
            },
        }
    }
    pub fn name(&self) -> &str {
        &self.function.name
    }
    pub fn args(&self) -> &str {
        &self.function.arguments
    }
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
            bytes += call.name().len() + call.args().len() + 16;
        }
        i64::try_from(bytes / 4)
            .unwrap_or(i64::MAX)
            .saturating_add(4)
    }
}

pub fn estimate_messages(messages: &[ChatMessage]) -> i64 {
    messages.iter().map(ChatMessage::est_tokens).sum::<i64>() + 8
}

#[derive(Debug, Deserialize)]
struct WireResponse {
    choices: Vec<WireChoice>,
    #[serde(default)]
    usage: Usage,
}

#[derive(Debug, Deserialize)]
struct WireChoice {
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
    #[serde(default)]
    function: WireFunction,
}

#[derive(Debug, Default, Deserialize)]
struct WireFunction {
    name: Option<String>,
    arguments: Option<Value>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Usage {
    prompt_tokens: i64,
    completion_tokens: i64,
    prompt_tokens_details: PromptDetails,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PromptDetails {
    cached_tokens: i64,
}

#[derive(Debug)]
pub struct LlmResponse {
    pub message: ChatMessage,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub cached_tokens: i64,
    pub truncated: bool,
}

/// The live context plus an optional one-request hint, serialized as one array
/// without copying the context. The hint is never stored.
struct Messages<'a>(&'a [ChatMessage], Option<ChatMessage>);

impl Serialize for Messages<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(self.0.iter().chain(self.1.iter()))
    }
}

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
    provider: Provider,
    max_retries: u32,
    agent: ureq::Agent,
}

impl LlmClient {
    pub fn new(provider: Provider, model: String, max_retries: u32, timeout_secs: u64) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(timeout_secs))
            .build();
        Self {
            model,
            provider,
            max_retries,
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
        let p = &self.provider;
        let body = serde_json::to_vec(&Request {
            model: &self.model,
            messages: Messages(
                messages,
                hint.map(|h| ChatMessage::user(format!("[note] {h}"))),
            ),
            stream: false,
            tools,
            tool_choice: (p.send_tool_choice && !tools.is_empty()).then_some("auto"),
            max_tokens: BTreeMap::from([(p.max_tokens_field.as_str(), p.max_output_tokens)]),
        })?;
        if let Some(dir) = std::env::var_os("GENJI_DUMP_REQUESTS") {
            let path = crate::storage::util::tmp_file(dir.as_ref(), "request", "json");
            let _ = crate::storage::util::write_file(&path, &body);
        }
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
        let p = &self.provider;
        let url = format!("{}/chat/completions", p.base_url.trim_end_matches('/'));
        let mut req = self
            .agent
            .post(&url)
            .set("Content-Type", "application/json");
        let key = p.api_key();
        if !key.is_empty() {
            req = if p.auth == "api-key" {
                req.set("api-key", &key)
            } else {
                req.set("Authorization", &format!("Bearer {key}"))
            };
        }
        for (k, v) in &p.headers {
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
                    error: anyhow!("HTTP {code}: {}", truncate(&txt, 600)),
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
    let base = (1000u64 << attempt.min(5)).min(30_000);
    let jitter = crate::storage::util::unix_millis() % (base / 4 + 1);
    Duration::from_millis(base + jitter)
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
            let arguments = match call.function.arguments {
                None => "{}".to_string(),
                Some(Value::String(s)) => s,
                Some(v) => v.to_string(),
            };
            ToolCall::new(
                call.id.unwrap_or_else(|| format!("call_{i}")),
                call.function.name.unwrap_or_default(),
                arguments,
            )
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
        truncated: choice.finish_reason.as_deref() == Some("length"),
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
    use super::{ChatMessage, Request, ToolCall, truncate};

    #[test]
    fn truncates_on_char_boundary() {
        let s = "é".repeat(50);
        let out = truncate(&s, 11);
        assert!(out.contains("truncated"));
        assert!(out.len() < 50 * 2 + 40);
        assert_eq!(truncate("hi", 10), "hi");
    }

    #[test]
    fn parses_cached_tokens_and_truncation() {
        let body = r#"{"choices":[{"message":{"content":"x"},"finish_reason":"length"}],"usage":{"prompt_tokens":10,"completion_tokens":2,"prompt_tokens_details":{"cached_tokens":8}}}"#;
        let r = super::parse_response(body).unwrap();
        assert_eq!(r.cached_tokens, 8);
        assert!(r.truncated);
    }

    #[test]
    fn keeps_raw_argument_strings() {
        let body = r#"{"choices":[{"message":{"tool_calls":[{"id":"a","function":{"name":"read","arguments":"{ \"path\" : \"x\" }"}}]}}]}"#;
        let r = super::parse_response(body).unwrap();
        assert_eq!(r.message.tool_calls[0].args(), r#"{ "path" : "x" }"#);
    }

    #[test]
    fn messages_round_trip_through_json() {
        let mut m = ChatMessage::user("a");
        m.tool_calls.push(ToolCall::new("c1", "read", "{}"));
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains(r#""type":"function""#));
        let back: ChatMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tool_calls[0].name(), "read");
        assert_eq!(serde_json::to_string(&back).unwrap(), json);
    }

    #[test]
    fn backoff_is_capped() {
        assert!(super::backoff(0).as_millis() >= 1000);
        assert!(super::backoff(20).as_millis() <= 37_500);
    }

    #[test]
    fn request_body_wire_format() {
        let msgs = [ChatMessage::user("hi")];
        let body = |tools: &[serde_json::Value], choice, hint| {
            serde_json::to_value(Request {
                model: "m",
                messages: super::Messages(&msgs, hint),
                stream: false,
                tools,
                tool_choice: choice,
                max_tokens: std::collections::BTreeMap::from([("max_completion_tokens", 9)]),
            })
            .unwrap()
        };
        let bare = body(&[], None, None);
        assert_eq!(bare["max_completion_tokens"], 9);
        assert_eq!(bare["messages"][0]["content"], "hi");
        assert!(bare.get("tools").is_none() && bare.get("tool_choice").is_none());
        let with = body(
            &[serde_json::json!({"type": "function"})],
            Some("auto"),
            Some(ChatMessage::user("[note] x")),
        );
        assert_eq!(with["tools"][0]["type"], "function");
        assert_eq!(with["tool_choice"], "auto");
        assert_eq!(with["messages"][1]["content"], "[note] x");
    }
}
