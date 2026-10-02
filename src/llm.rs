use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize, Serializer};
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

#[derive(Debug, Clone, Default, Serialize)]
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

/// Chat-completions request body, serialized straight from the live context.
#[derive(Serialize)]
struct Request<'a> {
    model: &'a str,
    messages: &'a [ChatMessage],
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
            model: runtime.model,
            max_tokens: runtime.limits.max_output_tokens,
            agent,
        }
    }

    pub fn chat(&self, messages: &[ChatMessage], tools: &[Value]) -> Result<LlmResponse> {
        let body = serde_json::to_vec(&Request {
            model: &self.model,
            messages,
            stream: false,
            tools,
            tool_choice: (self.send_tool_choice && !tools.is_empty()).then_some("auto"),
            max_tokens: BTreeMap::from([(self.max_tokens_field.as_str(), self.max_tokens)]),
        })?;
        for attempt in 0..4u32 {
            match self.post_once(&body) {
                Ok(resp) => return Ok(resp),
                Err((e, true)) if attempt < 3 => {
                    let backoff = Duration::from_millis(800 * (1u64 << attempt));
                    eprintln!(
                        "[llm] retry {}/3 after error ({e:#}), sleeping {backoff:?}",
                        attempt + 1
                    );
                    std::thread::sleep(backoff);
                }
                Err((e, _)) => return Err(e),
            }
        }
        unreachable!("the final attempt always returns")
    }

    /// One request; the error carries whether it is worth retrying.
    fn post_once(&self, body: &[u8]) -> Result<LlmResponse, (anyhow::Error, bool)> {
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
                    .map_err(|e| (anyhow!("reading llm response: {e}"), true))?;
                parse_response(&text).map_err(|e| (e, false))
            }
            Err(ureq::Error::Status(code, r)) => {
                let txt = r.into_string().unwrap_or_default();
                Err((
                    anyhow!("HTTP {code}: {}", txt.chars().take(600).collect::<String>()),
                    code == 429 || code >= 500,
                ))
            }
            Err(e) => Err((anyhow!("transport error: {e}"), true)),
        }
    }
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
    fn detects_truncation() {
        let mut resp = super::LlmResponse {
            message: ChatMessage::default(),
            prompt_tokens: 0,
            completion_tokens: 0,
            finish_reason: Some("length".into()),
        };
        assert!(resp.is_truncated());
        resp.finish_reason = Some("stop".into());
        assert!(!resp.is_truncated());
        resp.finish_reason = None;
        assert!(!resp.is_truncated());
    }

    #[test]
    fn request_body_wire_format() {
        let msgs = [ChatMessage::user("hi")];
        let body = |tools: &[serde_json::Value], choice| {
            serde_json::to_value(Request {
                model: "m",
                messages: &msgs,
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
