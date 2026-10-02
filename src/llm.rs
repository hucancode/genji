use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize, Serializer};
use serde_json::{Value, json};
use std::borrow::Cow;
use std::fmt;
use std::fmt::Write as _;
use std::time::Duration;

use crate::config::{Config, ModelRuntime, ProviderConfig};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    #[default]
    Assistant,
    Tool,
}

impl Role {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
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
    #[serde(default)]
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

    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).expect("ChatMessage is serializable")
    }

    pub fn est_tokens(&self) -> i64 {
        let mut chars = self.content.chars().count();
        if let Some(reasoning) = &self.reasoning_content {
            chars = chars.saturating_add(reasoning.chars().count());
        }
        for call in &self.tool_calls {
            chars = chars
                .saturating_add(call.name.chars().count())
                .saturating_add(call.arguments.chars().count())
                .saturating_add(16);
        }
        estimate_chars(chars)
    }
}

pub fn estimate_chars(chars: usize) -> i64 {
    i64::try_from(chars / 4)
        .unwrap_or(i64::MAX)
        .saturating_add(4)
}

#[derive(Debug, Clone)]
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

pub struct LlmClient {
    provider: ProviderConfig,
    pub model: String,
    max_tokens: i64,
    api_key: String,
    agent: ureq::Agent,
}

impl LlmClient {
    pub fn from_runtime(cfg: &Config, runtime: ModelRuntime) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(cfg.time_limit_secs.max(60) + 120))
            .build();
        let api_key = runtime.provider.resolve_api_key();
        Self {
            provider: runtime.provider,
            model: runtime.model,
            max_tokens: runtime.limits.max_output_tokens,
            api_key,
            agent,
        }
    }

    /// Build the endpoint URL for this provider kind.
    ///
    /// * openai-compatible (llama.cpp, `DeepSeek`, `OpenAI`, …): `{base}/chat/completions`
    /// * azure: `{base}/openai/deployments/{deployment}/chat/completions?api-version=…`
    fn url(&self) -> String {
        let base = self.provider.base_url.trim_end_matches('/');
        let mut url = if self.provider.is_azure() {
            format!(
                "{base}/openai/deployments/{}/chat/completions",
                url_encode(&self.model)
            )
        } else {
            format!("{base}/chat/completions")
        };
        let mut query: Vec<(String, String)> = Vec::new();
        if self.provider.is_azure() && !self.provider.api_version.is_empty() {
            query.push(("api-version".into(), self.provider.api_version.clone()));
        }
        query.extend(
            self.provider
                .extra_query
                .iter()
                .map(|(k, v)| (k.clone(), v.clone())),
        );
        if !query.is_empty() {
            let qs = query
                .iter()
                .map(|(k, v)| format!("{}={}", url_encode(k), url_encode(v)))
                .collect::<Vec<_>>()
                .join("&");
            url.push('?');
            url.push_str(&qs);
        }
        url
    }

    pub fn chat(&self, messages: &[ChatMessage], tools: &[Value]) -> Result<LlmResponse> {
        let msgs: Vec<Value> = messages.iter().map(ChatMessage::to_json).collect();
        let mut body = json!({
            "model": self.model,
            "messages": msgs,
            "stream": false,
        });
        if let Some(obj) = body.as_object_mut() {
            obj.insert(
                self.provider.max_tokens_field.clone(),
                json!(self.max_tokens),
            );
        }
        if !tools.is_empty() {
            body["tools"] = json!(tools);
            if self.provider.send_tool_choice {
                body["tool_choice"] = json!("auto");
            }
        }
        let url = self.url();

        let mut last_err: Option<anyhow::Error> = None;
        for attempt in 0..4u32 {
            match self.post_once(&url, &body) {
                Ok(resp) => return Ok(resp),
                Err(e) => {
                    let retryable = is_retryable(&e);
                    last_err = Some(e);
                    if !retryable || attempt == 3 {
                        break;
                    }
                    let backoff = Duration::from_millis(800 * (1u64 << attempt));
                    eprintln!(
                        "[llm] retry {}/3 after error, sleeping {:?}",
                        attempt + 1,
                        backoff
                    );
                    std::thread::sleep(backoff);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow!("llm request failed")))
    }

    fn post_once(&self, url: &str, body: &Value) -> Result<LlmResponse> {
        let mut req = self.agent.post(url).set("Content-Type", "application/json");
        if !self.api_key.is_empty() {
            if self.provider.auth == "api-key" {
                req = req.set("api-key", &self.api_key);
            } else {
                req = req.set("Authorization", &format!("Bearer {}", self.api_key));
            }
        }
        for (k, v) in &self.provider.extra_headers {
            req = req.set(k, v);
        }

        let resp = req.send_json(body);
        let value: Value = match resp {
            Ok(r) => r.into_json::<Value>().context("decoding llm json")?,
            Err(ureq::Error::Status(code, r)) => {
                let txt = r.into_string().unwrap_or_default();
                let msg = format!(
                    "HTTP {}: {}",
                    code,
                    txt.chars().take(600).collect::<String>()
                );
                return Err(if code == 429 || code >= 500 {
                    anyhow::Error::new(Retryable { msg })
                } else {
                    anyhow!(msg)
                });
            }
            Err(e) => {
                return Err(anyhow::Error::new(Retryable {
                    msg: format!("transport error: {e}"),
                }));
            }
        };
        parse_response(&value)
    }
}

#[derive(Debug)]
struct Retryable {
    msg: String,
}

impl std::fmt::Display for Retryable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.msg)
    }
}

impl std::error::Error for Retryable {}

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

fn is_retryable(e: &anyhow::Error) -> bool {
    e.downcast_ref::<Retryable>().is_some()
}

fn parse_response(value: &Value) -> Result<LlmResponse> {
    let parsed: WireResponse =
        serde_json::from_value(value.clone()).context("parsing llm response")?;
    let choice = parsed.choices.into_iter().next().ok_or_else(|| {
        anyhow!(
            "llm response has no choices: {}",
            truncate(&value.to_string(), 400)
        )
    })?;
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
    use super::{ChatMessage, truncate, url_encode};

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
}
