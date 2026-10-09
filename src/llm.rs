use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize, Serializer};
use serde_json::Value;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use crate::config::{Api, Auth, Provider};
use crate::storage::util::truncate;

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
    timeout: Duration,
    /// No request or retry wait runs past this instant.
    deadline: Instant,
    agent: ureq::Agent,
}

impl LlmClient {
    pub fn new(
        provider: Provider,
        model: String,
        max_retries: u32,
        timeout: Duration,
        deadline: Instant,
    ) -> Self {
        Self {
            model,
            provider,
            max_retries,
            timeout,
            deadline,
            agent: ureq::agent(),
        }
    }

    /// `hint` is sent as a trailing user message for this request only.
    /// `require_tool` makes the model answer with a tool call.
    pub fn chat(
        &self,
        messages: &[ChatMessage],
        tools: &[Value],
        hint: Option<&str>,
        require_tool: bool,
    ) -> Result<LlmResponse> {
        let body = self.body(messages, tools, hint, require_tool)?;
        if let Some(dir) = std::env::var_os("GENJI_DUMP_REQUESTS") {
            let path = crate::storage::util::tmp_file(dir.as_ref(), "request", "json");
            let _ = crate::storage::util::write_file(&path, &body);
        }
        let mut attempt = 0u32;
        loop {
            let left = self.deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                bail!("time limit reached");
            }
            match self.post_once(&body, self.timeout.min(left)) {
                Ok(resp) => return Ok(resp),
                Err(f) if f.retry && attempt < self.max_retries => {
                    let wait = f.retry_after.unwrap_or_else(|| backoff(attempt));
                    if wait >= self.deadline.saturating_duration_since(Instant::now()) {
                        return Err(f.error);
                    }
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

    fn body(
        &self,
        messages: &[ChatMessage],
        tools: &[Value],
        hint: Option<&str>,
        require_tool: bool,
    ) -> Result<Vec<u8>> {
        let p = &self.provider;
        let hint = hint.map(|h| ChatMessage::user(format!("[note] {h}")));
        Ok(if p.api == Api::Anthropic {
            let mut req =
                anthropic_request(&self.model, messages, hint, tools, p.max_output_tokens);
            if require_tool && !tools.is_empty() {
                req["tool_choice"] = serde_json::json!({"type": "any"});
            }
            serde_json::to_vec(&req)?
        } else {
            serde_json::to_vec(&Request {
                model: &self.model,
                messages: Messages(messages, hint),
                stream: false,
                tools,
                tool_choice: (p.send_tool_choice && !tools.is_empty()).then_some(if require_tool {
                    "required"
                } else {
                    "auto"
                }),
                max_tokens: BTreeMap::from([(p.max_tokens_field.as_str(), p.max_output_tokens)]),
            })?
        })
    }

    /// One request; the failure says whether it is worth retrying.
    fn post_once(&self, body: &[u8], timeout: Duration) -> Result<LlmResponse, Failure> {
        let fail = |error, retry| Failure {
            error,
            retry,
            retry_after: None,
        };
        let p = &self.provider;
        let anthropic = p.api == Api::Anthropic;
        let url = format!(
            "{}/{}",
            p.base_url.trim_end_matches('/'),
            if anthropic {
                "messages"
            } else {
                "chat/completions"
            }
        );
        let req = authorize(
            p,
            self.agent
                .post(&url)
                .timeout(timeout)
                .set("Content-Type", "application/json"),
        );
        match req.send_bytes(body) {
            Ok(r) => {
                let text = r
                    .into_string()
                    .map_err(|e| fail(anyhow!("reading llm response: {e}"), true))?;
                // A garbled 200 (truncated JSON, a proxy's HTML page) is transient.
                (if anthropic {
                    parse_anthropic(&text)
                } else {
                    parse_response(&text)
                })
                .map_err(|e| fail(e, true))
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

/// Auth, version and custom headers every request to `p` carries.
fn authorize(p: &Provider, mut req: ureq::Request) -> ureq::Request {
    let anthropic = p.api == Api::Anthropic;
    if anthropic {
        req = req.set("anthropic-version", "2023-06-01");
    }
    let key = p.api_key();
    if !key.is_empty() {
        req = if anthropic {
            req.set("x-api-key", &key)
        } else if p.auth == Auth::ApiKey {
            req.set("api-key", &key)
        } else {
            req.set("Authorization", &format!("Bearer {key}"))
        };
    }
    for (k, v) in &p.headers {
        req = req.set(k, v);
    }
    req
}

/// The model's context size as the server reports it: the anthropic models API, the
/// OpenAI-style `/models` list (vLLM, OpenRouter, llama.cpp), else llama.cpp's `/props`.
/// `None` when the server does not say or cannot be reached.
pub fn model_context_window(p: &Provider, model: &str) -> Option<i64> {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(5))
        .build();
    let base = p.base_url.trim_end_matches('/');
    let get =
        |url: &str| -> Option<Value> { authorize(p, agent.get(url)).call().ok()?.into_json().ok() };
    if p.api == Api::Anthropic {
        return get(&format!("{base}/models/{model}")).and_then(|v| v["max_input_tokens"].as_i64());
    }
    get(&format!("{base}/models"))
        .and_then(|v| listed_context_window(&v, model))
        .or_else(|| {
            let root = base.strip_suffix("/v1").unwrap_or(base);
            get(&format!("{root}/props"))
                .and_then(|v| v["default_generation_settings"]["n_ctx"].as_i64())
        })
        .filter(|n| *n > 0)
}

/// The context size of `model` in an OpenAI-style `/models` response.
fn listed_context_window(list: &Value, model: &str) -> Option<i64> {
    let entry = list["data"].as_array()?.iter().find(|m| m["id"] == model)?;
    ["context_length", "max_model_len", "context_window"]
        .iter()
        .find_map(|k| entry[*k].as_i64())
        .or_else(|| entry["meta"]["n_ctx_train"].as_i64())
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
    let v: Value = serde_json::from_str(text)
        .with_context(|| format!("parsing llm response: {}", truncate(text, 400)))?;
    let choice = &v["choices"][0];
    if choice.is_null() {
        bail!("llm response has no choices: {}", truncate(text, 400));
    }
    let m = &choice["message"];
    let tool_calls = m["tool_calls"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .map(|(i, c)| {
            let f = &c["function"];
            // Kept as the raw string the model produced so a replayed request is byte-identical.
            let arguments = match &f["arguments"] {
                Value::Null => "{}".to_string(),
                Value::String(s) => s.clone(),
                a => a.to_string(),
            };
            ToolCall::new(
                c["id"]
                    .as_str()
                    .map_or_else(|| format!("call_{i}"), String::from),
                f["name"].as_str().unwrap_or_default(),
                arguments,
            )
        })
        .collect();
    let u = &v["usage"];
    Ok(LlmResponse {
        message: ChatMessage {
            role: Role::Assistant,
            content: m["content"].as_str().unwrap_or_default().to_string(),
            tool_calls,
            tool_call_id: None,
            reasoning_content: m["reasoning_content"].as_str().map(String::from),
        },
        prompt_tokens: u["prompt_tokens"].as_i64().unwrap_or(0),
        completion_tokens: u["completion_tokens"].as_i64().unwrap_or(0),
        cached_tokens: u["prompt_tokens_details"]["cached_tokens"]
            .as_i64()
            .unwrap_or(0),
        truncated: choice["finish_reason"] == "length",
    })
}

/// Chat-completions history as an Anthropic Messages request. System text
/// becomes `system`; assistant tool calls become `tool_use` blocks; tool
/// results become `tool_result` blocks of a user turn, merged with adjacent
/// user content because the API wants strictly alternating roles.
fn anthropic_request(
    model: &str,
    messages: &[ChatMessage],
    hint: Option<ChatMessage>,
    tools: &[Value],
    max_tokens: i64,
) -> Value {
    let mut system = Vec::new();
    let mut turns: Vec<(&str, Vec<Value>)> = Vec::new();
    let mut push = |role: &'static str, block: Value| match turns.last_mut() {
        Some((r, blocks)) if *r == role => blocks.push(block),
        _ => turns.push((role, vec![block])),
    };
    for m in messages.iter().chain(hint.iter()) {
        match m.role {
            Role::System => system.push(m.content.as_str()),
            Role::User => {
                if !m.content.is_empty() {
                    push(
                        "user",
                        serde_json::json!({"type": "text", "text": m.content}),
                    );
                }
            }
            Role::Tool => push(
                "user",
                serde_json::json!({
                    "type": "tool_result",
                    "tool_use_id": m.tool_call_id.as_deref().unwrap_or_default(),
                    "content": if m.content.is_empty() { "(empty)" } else { m.content.as_str() },
                }),
            ),
            Role::Assistant => {
                let mut any = false;
                if !m.content.is_empty() {
                    any = true;
                    push(
                        "assistant",
                        serde_json::json!({"type": "text", "text": m.content}),
                    );
                }
                for c in &m.tool_calls {
                    any = true;
                    let input = serde_json::from_str::<Value>(c.args())
                        .ok()
                        .filter(Value::is_object)
                        .unwrap_or_else(|| serde_json::json!({}));
                    push(
                        "assistant",
                        serde_json::json!({"type": "tool_use", "id": c.id, "name": c.name(), "input": input}),
                    );
                }
                if !any {
                    push(
                        "assistant",
                        serde_json::json!({"type": "text", "text": "(no output)"}),
                    );
                }
            }
        }
    }
    // tool_result blocks must lead the user turn that answers a tool_use.
    for (_, blocks) in &mut turns {
        blocks.sort_by_key(|b| b["type"] != "tool_result");
    }
    // Prompt caching: breakpoints after the tools, the system text and the latest block,
    // so each request re-reads the unchanged prefix at the cached rate.
    let cache = serde_json::json!({"type": "ephemeral"});
    if let Some((_, blocks)) = turns.last_mut()
        && let Some(last) = blocks.last_mut()
    {
        last["cache_control"] = cache.clone();
    }
    let tools: Vec<Value> = tools
        .iter()
        .map(|t| {
            let f = &t["function"];
            serde_json::json!({
                "name": f["name"],
                "description": f["description"],
                "input_schema": f["parameters"],
            })
        })
        .collect();
    let mut req = serde_json::json!({
        "model": model,
        "max_tokens": max_tokens,
        "messages": turns
            .into_iter()
            .map(|(role, content)| serde_json::json!({"role": role, "content": content}))
            .collect::<Vec<_>>(),
    });
    if !system.is_empty() {
        req["system"] = serde_json::json!([
            {"type": "text", "text": system.join("\n\n"), "cache_control": cache.clone()}
        ]);
    }
    if !tools.is_empty() {
        req["tools"] = tools.into();
        if let Some(last) = req["tools"].as_array_mut().and_then(|t| t.last_mut()) {
            last["cache_control"] = cache;
        }
    }
    req
}

fn parse_anthropic(text: &str) -> Result<LlmResponse> {
    let v: Value = serde_json::from_str(text)
        .with_context(|| format!("parsing llm response: {}", truncate(text, 400)))?;
    let blocks = v["content"]
        .as_array()
        .ok_or_else(|| anyhow!("llm response has no content: {}", truncate(text, 400)))?;
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut tool_calls = Vec::new();
    for b in blocks {
        match b["type"].as_str() {
            Some("text") => content.push_str(b["text"].as_str().unwrap_or_default()),
            Some("thinking") => reasoning.push_str(b["thinking"].as_str().unwrap_or_default()),
            Some("tool_use") => tool_calls.push(ToolCall::new(
                b["id"].as_str().unwrap_or_default(),
                b["name"].as_str().unwrap_or_default(),
                b["input"].to_string(),
            )),
            _ => {}
        }
    }
    let u = &v["usage"];
    let n = |k: &str| u[k].as_i64().unwrap_or(0);
    let cached = n("cache_read_input_tokens");
    Ok(LlmResponse {
        message: ChatMessage {
            role: Role::Assistant,
            content,
            tool_calls,
            tool_call_id: None,
            reasoning_content: (!reasoning.is_empty()).then_some(reasoning),
        },
        prompt_tokens: n("input_tokens") + cached + n("cache_creation_input_tokens"),
        completion_tokens: n("output_tokens"),
        cached_tokens: cached,
        truncated: v["stop_reason"].as_str() == Some("max_tokens"),
    })
}

#[cfg(test)]
mod tests {
    use super::{Api, ChatMessage, Duration, Instant, Request, ToolCall, Value};

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

    fn m_last_cache(r: &serde_json::Value) -> String {
        let m = r["messages"].as_array().unwrap().last().unwrap();
        m["content"].as_array().unwrap().last().unwrap()["cache_control"]["type"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[test]
    fn anthropic_request_merges_turns_and_maps_tools() {
        let mut a = ChatMessage::default();
        a.tool_calls
            .push(ToolCall::new("t1", "read", r#"{"path":"x"}"#));
        let msgs = [
            ChatMessage::system("sys"),
            ChatMessage::user("go"),
            a,
            ChatMessage::tool_result("t1", "ok"),
        ];
        let tools = [
            serde_json::json!({"type":"function","function":{"name":"read","description":"d","parameters":{"type":"object"}}}),
        ];
        let r =
            super::anthropic_request("m", &msgs, Some(ChatMessage::user("[note] h")), &tools, 9);
        assert_eq!(r["system"][0]["text"], "sys");
        assert_eq!(r["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(r["tools"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(m_last_cache(&r), "ephemeral");
        assert_eq!(r["max_tokens"], 9);
        assert_eq!(r["tools"][0]["input_schema"]["type"], "object");
        let m = r["messages"].as_array().unwrap();
        assert_eq!(m.len(), 3);
        assert_eq!(m[1]["content"][0]["input"]["path"], "x");
        assert_eq!(m[2]["content"][0]["type"], "tool_result");
        assert_eq!(m[2]["content"][1]["text"], "[note] h");
    }

    #[test]
    fn parses_anthropic_response() {
        let body = r#"{"content":[{"type":"text","text":"hi"},{"type":"tool_use","id":"t","name":"read","input":{"path":"x"}}],"stop_reason":"max_tokens","usage":{"input_tokens":5,"cache_read_input_tokens":3,"output_tokens":2}}"#;
        let r = super::parse_anthropic(body).unwrap();
        assert_eq!(r.message.content, "hi");
        assert_eq!(r.message.tool_calls[0].args(), r#"{"path":"x"}"#);
        assert_eq!(
            (r.prompt_tokens, r.cached_tokens, r.completion_tokens),
            (8, 3, 2)
        );
        assert!(r.truncated);
    }

    #[test]
    fn reads_context_window_from_model_lists() {
        let list = serde_json::json!({"data": [
            {"id": "a", "max_model_len": 32768},
            {"id": "b", "context_length": 131072},
            {"id": "c", "meta": {"n_ctx_train": 8192}},
            {"id": "d"}
        ]});
        assert_eq!(super::listed_context_window(&list, "a"), Some(32768));
        assert_eq!(super::listed_context_window(&list, "b"), Some(131072));
        assert_eq!(super::listed_context_window(&list, "c"), Some(8192));
        assert_eq!(super::listed_context_window(&list, "d"), None);
        assert_eq!(super::listed_context_window(&list, "e"), None);
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

    #[test]
    fn a_required_tool_call_is_sent_and_the_deadline_holds() {
        let tools = [serde_json::json!({"type": "function", "function": {"name": "finish"}})];
        let msgs = [ChatMessage::user("hi")];
        for (api, want) in [
            (Api::Chat, serde_json::json!("required")),
            (Api::Anthropic, serde_json::json!({"type": "any"})),
        ] {
            let p = crate::config::Provider {
                api,
                ..Default::default()
            };
            let c = super::LlmClient::new(p, "m".into(), 0, Duration::from_secs(1), Instant::now());
            let body: Value =
                serde_json::from_slice(&c.body(&msgs, &tools, None, true).unwrap()).unwrap();
            assert_eq!(body["tool_choice"], want);
            let e = c.chat(&msgs, &tools, None, false).unwrap_err();
            assert_eq!(format!("{e:#}"), "time limit reached");
        }
    }
}
