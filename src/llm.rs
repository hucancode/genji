use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::time::Duration;

use crate::config::{Config, ProviderConfig};

#[derive(Debug, Clone, Default)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub tool_call_id: Option<String>,
    pub reasoning_content: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: content.into(),
            ..Default::default()
        }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: content.into(),
            ..Default::default()
        }
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".into(),
            content: content.into(),
            ..Default::default()
        }
    }
    pub fn tool_result(tool_call_id: &str, content: impl Into<String>) -> Self {
        Self {
            role: "tool".into(),
            content: content.into(),
            tool_call_id: Some(tool_call_id.to_string()),
            ..Default::default()
        }
    }

    pub fn to_json(&self) -> Value {
        let mut obj = serde_json::Map::new();
        obj.insert("role".into(), json!(self.role));
        obj.insert("content".into(), json!(self.content));
        if let Some(rc) = &self.reasoning_content {
            if !rc.is_empty() {
                obj.insert("reasoning_content".into(), json!(rc));
            }
        }
        if !self.tool_calls.is_empty() {
            let calls: Vec<Value> = self
                .tool_calls
                .iter()
                .map(|c| {
                    json!({
                        "id": c.id,
                        "type": "function",
                        "function": { "name": c.name, "arguments": c.arguments }
                    })
                })
                .collect();
            obj.insert("tool_calls".into(), json!(calls));
        }
        if let Some(id) = &self.tool_call_id {
            obj.insert("tool_call_id".into(), json!(id));
        }
        Value::Object(obj)
    }

    /// Rough token estimate (≈4 chars/token) used for compaction decisions.
    pub fn est_tokens(&self) -> i64 {
        let mut n = self.content.chars().count();
        if let Some(r) = &self.reasoning_content {
            n += r.chars().count();
        }
        for c in &self.tool_calls {
            n += c.name.chars().count() + c.arguments.chars().count() + 16;
        }
        ((n / 4) + 4) as i64
    }
}

#[derive(Debug, Clone)]
pub struct LlmResponse {
    pub message: ChatMessage,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub finish_reason: Option<String>,
}

impl LlmResponse {
    /// True when the provider stopped early because it hit the output cap,
    /// leaving the answer and/or tool calls incomplete.
    pub fn is_truncated(&self) -> bool {
        self.finish_reason.as_deref() == Some("length")
    }
}

pub fn estimate_messages(messages: &[ChatMessage]) -> i64 {
    messages.iter().map(|m| m.est_tokens()).sum::<i64>() + 8
}

pub struct LlmClient {
    provider: ProviderConfig,
    pub model: String,
    max_tokens: i64,
    agent: ureq::Agent,
}

impl LlmClient {
    pub fn new(cfg: &Config, model: &str) -> Result<Self> {
        let provider = cfg.resolve_active_provider();
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(cfg.time_limit_secs.max(60) + 120))
            .build();
        Ok(Self {
            provider,
            model: model.to_string(),
            max_tokens: cfg.limits_for_model(model).max_output_tokens,
            agent,
        })
    }

    /// Build the endpoint URL for this provider kind.
    ///
    /// * openai-compatible (llama.cpp, DeepSeek, OpenAI, …): `{base}/chat/completions`
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

    /// OpenAI-compatible chat completion with tool definitions.
    pub fn chat(&self, messages: &[ChatMessage], tools: &[Value]) -> Result<LlmResponse> {
        let msgs: Vec<Value> = messages.iter().map(|m| m.to_json()).collect();
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

        // Auth: local servers usually need none, so only send when we have a key.
        let key = self.provider.resolve_api_key();
        if !key.is_empty() {
            if self.provider.auth == "api-key" {
                req = req.set("api-key", &key);
            } else {
                req = req.set("Authorization", &format!("Bearer {key}"));
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
                // Retryability is decided where the status code is known, not
                // re-parsed from the message later.
                return Err(if code == 429 || code >= 500 {
                    anyhow::Error::new(Retryable { msg })
                } else {
                    anyhow!(msg)
                });
            }
            Err(e) => {
                return Err(anyhow::Error::new(Retryable {
                    msg: format!("transport error: {e}"),
                }))
            }
        };
        parse_response(&value)
    }
}

/// Wraps an error the client may retry. `chat` detects it via downcast instead
/// of matching on the rendered message.
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

/// Minimal percent-encoding for query-string keys/values.
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn is_retryable(e: &anyhow::Error) -> bool {
    e.downcast_ref::<Retryable>().is_some()
}

fn parse_response(value: &Value) -> Result<LlmResponse> {
    let choice = value.get("choices").and_then(|c| c.get(0)).ok_or_else(|| {
        anyhow!(
            "llm response has no choices: {}",
            truncate(value.to_string(), 400)
        )
    })?;
    let msg = choice.get("message").cloned().unwrap_or(Value::Null);
    let content = msg
        .get("content")
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .to_string();
    let reasoning = msg
        .get("reasoning_content")
        .and_then(|c| c.as_str())
        .map(|s| s.to_string());
    let mut tool_calls = Vec::new();
    if let Some(calls) = msg.get("tool_calls").and_then(|c| c.as_array()) {
        for (i, c) in calls.iter().enumerate() {
            let id = c
                .get("id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| format!("call_{i}"));
            let f = c.get("function").cloned().unwrap_or(Value::Null);
            let name = f
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let arguments = match f.get("arguments") {
                Some(Value::String(s)) => s.clone(),
                Some(v) => v.to_string(),
                None => "{}".into(),
            };
            tool_calls.push(ToolCall {
                id,
                name,
                arguments,
            });
        }
    }
    let usage = value.get("usage").cloned().unwrap_or(Value::Null);
    let prompt_tokens = usage
        .get("prompt_tokens")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let completion_tokens = usage
        .get("completion_tokens")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    Ok(LlmResponse {
        message: ChatMessage {
            role: "assistant".into(),
            content,
            tool_calls,
            tool_call_id: None,
            reasoning_content: reasoning,
        },
        prompt_tokens,
        completion_tokens,
        finish_reason: choice
            .get("finish_reason")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
    })
}

pub fn truncate(s: String, max: usize) -> String {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… [{} bytes truncated]", &s[..end], s.len() - end)
}

#[cfg(test)]
mod tests {
    use super::{truncate, url_encode};

    #[test]
    fn truncates_on_char_boundary() {
        let s = "é".repeat(50);
        let out = truncate(s, 11);
        assert!(out.contains("truncated"));
        assert!(out.len() < 50 * 2 + 40);
    }

    #[test]
    fn keeps_short_strings() {
        assert_eq!(truncate("hi".into(), 10), "hi");
    }

    #[test]
    fn encodes_query_values() {
        assert_eq!(url_encode("2024-10-21"), "2024-10-21");
        assert_eq!(url_encode("a b/c"), "a%20b%2Fc");
    }

    #[test]
    fn detects_truncation() {
        let mut resp = super::LlmResponse {
            message: Default::default(),
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
