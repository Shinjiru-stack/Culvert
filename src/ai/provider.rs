//! Provider-agnostic LLM layer.
//!
//! One [`Provider`] value wraps any backend: the OpenAI-compatible family
//! (OpenRouter, OpenAI, Groq, Ollama, custom endpoints) speaks `/chat/completions`
//! with SSE streaming; Gemini and Anthropic get native wire formats. All
//! callers (engine, settings UI) use [`Provider::chat`] and
//! [`Provider::list_models`] and never touch provider differences.

use crate::ai::config::AiConfig;
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::collections::BTreeMap;

// ---------------------------------------------------------------------------
// Wire-neutral conversation model
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// Raw JSON arguments string as produced by the model.
    pub args: String,
}

#[derive(Clone, Debug, Default)]
pub struct Msg {
    /// system | user | assistant | tool
    pub role: String,
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub tool_call_id: String,
    pub name: String,
}

impl Msg {
    pub fn system(text: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: text.into(),
            ..Default::default()
        }
    }
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: text.into(),
            ..Default::default()
        }
    }
    pub fn assistant(text: impl Into<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: "assistant".into(),
            content: text.into(),
            tool_calls,
            ..Default::default()
        }
    }
    pub fn tool_result(id: &str, name: &str, text: impl Into<String>) -> Self {
        Self {
            role: "tool".into(),
            content: text.into(),
            tool_call_id: id.to_string(),
            name: name.to_string(),
            ..Default::default()
        }
    }
}

/// A tool the model may call. Schemas use a JSON-Schema subset that all
/// providers accept (object/string/number/boolean/array + required).
pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    pub parameters: Value,
}

#[derive(Default)]
pub struct ChatReply {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
}

// ---------------------------------------------------------------------------
// Flavor
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Flavor {
    OpenRouter,
    OpenAi,
    Groq,
    Ollama,
    Gemini,
    Anthropic,
    Custom,
}

impl Flavor {
    pub const ALL: [Flavor; 7] = [
        Flavor::OpenRouter,
        Flavor::OpenAi,
        Flavor::Groq,
        Flavor::Ollama,
        Flavor::Gemini,
        Flavor::Anthropic,
        Flavor::Custom,
    ];

    pub fn id(&self) -> &'static str {
        match self {
            Flavor::OpenRouter => "openrouter",
            Flavor::OpenAi => "openai",
            Flavor::Groq => "groq",
            Flavor::Ollama => "ollama",
            Flavor::Gemini => "gemini",
            Flavor::Anthropic => "anthropic",
            Flavor::Custom => "custom",
        }
    }

    pub fn from_id(id: &str) -> Flavor {
        match id {
            "openai" => Flavor::OpenAi,
            "groq" => Flavor::Groq,
            "ollama" => Flavor::Ollama,
            "gemini" => Flavor::Gemini,
            "anthropic" => Flavor::Anthropic,
            "custom" => Flavor::Custom,
            _ => Flavor::OpenRouter,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Flavor::OpenRouter => "OpenRouter",
            Flavor::OpenAi => "OpenAI",
            Flavor::Groq => "Groq",
            Flavor::Ollama => "Ollama (local)",
            Flavor::Gemini => "Google Gemini",
            Flavor::Anthropic => "Anthropic",
            Flavor::Custom => "Custom (OpenAI-compatible)",
        }
    }

    pub fn default_base(&self) -> &'static str {
        match self {
            Flavor::OpenRouter => "https://openrouter.ai/api/v1",
            Flavor::OpenAi => "https://api.openai.com/v1",
            Flavor::Groq => "https://api.groq.com/openai/v1",
            Flavor::Ollama => "http://localhost:11434/v1",
            Flavor::Gemini => "https://generativelanguage.googleapis.com/v1beta",
            Flavor::Anthropic => "https://api.anthropic.com",
            Flavor::Custom => "http://localhost:8080/v1",
        }
    }

    pub fn key_hint(&self) -> &'static str {
        match self {
            Flavor::OpenRouter => "sk-or-…",
            Flavor::OpenAi => "sk-…",
            Flavor::Groq => "gsk_…",
            Flavor::Ollama => "not required for a local Ollama",
            Flavor::Gemini => "AIza…",
            Flavor::Anthropic => "sk-ant-…",
            Flavor::Custom => "depends on your endpoint",
        }
    }
}

fn looks_local(url: &str) -> bool {
    let low = url.to_lowercase();
    low.contains("localhost") || low.contains("127.0.0.1") || low.contains("[::1]")
}

// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------

pub struct Provider {
    pub flavor: Flavor,
    pub base_url: String,
    pub api_key: String,
    http: reqwest::Client,
}

fn err_str(e: reqwest::Error) -> String {
    if e.is_connect() {
        format!("cannot reach the endpoint ({e})")
    } else if e.is_timeout() {
        "request timed out".to_string()
    } else {
        format!("{e}")
    }
}

fn clip(s: &str, n: usize) -> String {
    let t = s.trim();
    if t.chars().count() > n {
        let cut: String = t.chars().take(n).collect();
        format!("{cut}…")
    } else {
        t.to_string()
    }
}

impl Provider {
    pub fn new(cfg: &AiConfig) -> Self {
        let flavor = Flavor::from_id(&cfg.provider);
        let base_url = if cfg.base_url.trim().is_empty() {
            flavor.default_base().to_string()
        } else {
            cfg.base_url.trim().trim_end_matches('/').to_string()
        };
        let http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(15))
            .timeout(std::time::Duration::from_secs(180))
            .build()
            .unwrap_or_default();
        Self {
            flavor,
            base_url,
            api_key: cfg.api_key.trim().to_string(),
            http,
        }
    }

    /// True when requests never leave the machine (Ollama, loopback URLs):
    /// the security layer relaxes gating for these.
    pub fn is_local(&self) -> bool {
        self.flavor == Flavor::Ollama || looks_local(&self.base_url)
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url.trim_end_matches('/'), path)
    }

    fn origin(&self) -> String {
        // Strip a trailing /v1 (Ollama's native API lives at the root).
        let b = self.base_url.trim_end_matches('/');
        b.strip_suffix("/v1").unwrap_or(b).to_string()
    }

    // -- Model listing ------------------------------------------------------

    pub async fn list_models(&self) -> Result<Vec<String>, String> {
        match self.flavor {
            Flavor::Ollama => {
                // Native tags endpoint first; fall back to the compat /models.
                if let Ok(list) = self.ollama_tags().await {
                    if !list.is_empty() {
                        return Ok(list);
                    }
                }
                self.openai_models().await
            }
            Flavor::Gemini => self.gemini_models().await,
            Flavor::Anthropic => self.anthropic_models().await,
            _ => self.openai_models().await,
        }
    }

    async fn openai_models(&self) -> Result<Vec<String>, String> {
        let mut req = self.http.get(self.url("/models"));
        if !self.api_key.is_empty() {
            req = req.bearer_auth(&self.api_key);
        }
        req = self.extra_headers(req);
        let resp = req.send().await.map_err(err_str)?;
        let status = resp.status();
        let body = resp.text().await.map_err(err_str)?;
        if !status.is_success() {
            return Err(format!("HTTP {}: {}", status.as_u16(), clip(&body, 300)));
        }
        let v: Value = serde_json::from_str(&body).map_err(|e| format!("bad JSON: {e}"))?;
        let mut out: Vec<String> = v["data"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|m| m["id"].as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        out.sort();
        Ok(out)
    }

    async fn ollama_tags(&self) -> Result<Vec<String>, String> {
        let resp = self
            .http
            .get(format!("{}/api/tags", self.origin()))
            .send()
            .await
            .map_err(err_str)?;
        if !resp.status().is_success() {
            return Err(format!("HTTP {}", resp.status().as_u16()));
        }
        let v: Value = resp.json().await.map_err(err_str)?;
        let mut out: Vec<String> = v["models"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|m| m["name"].as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        out.sort();
        Ok(out)
    }

    async fn gemini_models(&self) -> Result<Vec<String>, String> {
        let resp = self
            .http
            .get(self.url("/models"))
            .query(&[("key", &self.api_key)])
            .send()
            .await
            .map_err(err_str)?;
        let status = resp.status();
        let body = resp.text().await.map_err(err_str)?;
        if !status.is_success() {
            return Err(format!("HTTP {}: {}", status.as_u16(), clip(&body, 300)));
        }
        let v: Value = serde_json::from_str(&body).map_err(|e| format!("bad JSON: {e}"))?;
        let mut out: Vec<String> = v["models"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|m| {
                        m["name"]
                            .as_str()
                            .map(|s| s.trim_start_matches("models/").to_string())
                    })
                    .filter(|s| s.starts_with("gemini"))
                    .collect()
            })
            .unwrap_or_default();
        out.sort();
        Ok(out)
    }

    async fn anthropic_models(&self) -> Result<Vec<String>, String> {
        let resp = self
            .http
            .get(self.url("/v1/models"))
            .query(&[("limit", "100")])
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .send()
            .await
            .map_err(err_str)?;
        let status = resp.status();
        let body = resp.text().await.map_err(err_str)?;
        if !status.is_success() {
            return Err(format!("HTTP {}: {}", status.as_u16(), clip(&body, 300)));
        }
        let v: Value = serde_json::from_str(&body).map_err(|e| format!("bad JSON: {e}"))?;
        let mut out: Vec<String> = v["data"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|m| m["id"].as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        out.sort();
        Ok(out)
    }

    // -- Chat ---------------------------------------------------------------

    /// One full conversation turn. `on_delta` receives streamed text as it
    /// arrives (buffered providers call it once with the final text).
    pub async fn chat(
        &self,
        model: &str,
        messages: &[Msg],
        tools: &[ToolDef],
        temperature: f32,
        stream: bool,
        on_delta: &mut (dyn FnMut(&str) + Send),
    ) -> Result<ChatReply, String> {
        if model.trim().is_empty() {
            return Err("no model selected — open AI settings and pick a model".to_string());
        }
        match self.flavor {
            Flavor::Gemini => self.gemini_chat(model, messages, tools, temperature, on_delta).await,
            Flavor::Anthropic => {
                self.anthropic_chat(model, messages, tools, temperature, on_delta)
                    .await
            }
            _ => {
                self.openai_chat(model, messages, tools, temperature, stream, on_delta)
                    .await
            }
        }
    }

    fn extra_headers(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if self.flavor == Flavor::OpenRouter {
            req.header("HTTP-Referer", "https://github.com/Shinjiru-stack/Culvert")
                .header("X-Title", "Culvert")
        } else {
            req
        }
    }

    async fn openai_chat(
        &self,
        model: &str,
        messages: &[Msg],
        tools: &[ToolDef],
        temperature: f32,
        stream: bool,
        on_delta: &mut (dyn FnMut(&str) + Send),
    ) -> Result<ChatReply, String> {
        let wire: Vec<Value> = messages.iter().map(openai_msg).collect();
        let mut body = json!({
            "model": model,
            "messages": wire,
            "temperature": temperature,
            "stream": stream,
        });
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools.iter().map(openai_tool).collect());
            body["tool_choice"] = json!("auto");
        }

        let mut req = self.http.post(self.url("/chat/completions")).json(&body);
        if !self.api_key.is_empty() {
            req = req.bearer_auth(&self.api_key);
        }
        req = self.extra_headers(req);
        let resp = req.send().await.map_err(err_str)?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("HTTP {}: {}", status.as_u16(), clip(&body, 400)));
        }

        if stream {
            self.openai_sse(resp, on_delta).await
        } else {
            let v: Value = resp.json().await.map_err(err_str)?;
            let msg = &v["choices"][0]["message"];
            let text = msg["content"]
                .as_str()
                .map(|s| s.to_string())
                // Some models emit content as parts.
                .or_else(|| {
                    msg["content"].as_array().map(|a| {
                        a.iter()
                            .filter_map(|p| p["text"].as_str())
                            .collect::<Vec<_>>()
                            .join("")
                    })
                })
                .unwrap_or_default();
            let mut calls = Vec::new();
            if let Some(arr) = msg["tool_calls"].as_array() {
                for (i, c) in arr.iter().enumerate() {
                    calls.push(ToolCall {
                        id: c["id"]
                            .as_str()
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| format!("call_{i}")),
                        name: c["function"]["name"].as_str().unwrap_or("").to_string(),
                        args: c["function"]["arguments"].as_str().unwrap_or("{}").to_string(),
                    });
                }
            }
            if !text.is_empty() {
                on_delta(&text);
            }
            Ok(ChatReply { text, tool_calls: calls })
        }
    }

    async fn openai_sse(
        &self,
        resp: reqwest::Response,
        on_delta: &mut (dyn FnMut(&str) + Send),
    ) -> Result<ChatReply, String> {
        #[derive(Default, Clone)]
        struct Partial {
            id: String,
            name: String,
            args: String,
        }
        let mut stream = resp.bytes_stream();
        let mut buf = String::new();
        let mut text = String::new();
        let mut parts: BTreeMap<usize, Partial> = BTreeMap::new();
        let mut done = false;

        while !done {
            let chunk = match stream.next().await {
                Some(Ok(c)) => c,
                Some(Err(e)) => return Err(err_str(e)),
                None => break,
            };
            buf.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(pos) = buf.find('\n') {
                let line = buf[..pos].trim_end().to_string();
                buf.drain(..=pos);
                let Some(data) = line.strip_prefix("data:") else {
                    continue;
                };
                let data = data.trim();
                if data.is_empty() {
                    continue;
                }
                if data == "[DONE]" {
                    done = true;
                    break;
                }
                let Ok(v) = serde_json::from_str::<Value>(data) else {
                    continue;
                };
                if let Some(err) = v.get("error") {
                    return Err(format!("provider error: {}", clip(&err.to_string(), 300)));
                }
                let delta = &v["choices"][0]["delta"];
                if let Some(c) = delta["content"].as_str() {
                    if !c.is_empty() {
                        text.push_str(c);
                        on_delta(c);
                    }
                }
                if let Some(arr) = delta["tool_calls"].as_array() {
                    for c in arr {
                        let idx = c["index"].as_u64().unwrap_or(0) as usize;
                        let e = parts.entry(idx).or_default();
                        if let Some(id) = c["id"].as_str() {
                            e.id = id.to_string();
                        }
                        if let Some(n) = c["function"]["name"].as_str() {
                            if !n.is_empty() {
                                e.name = n.to_string();
                            }
                        }
                        if let Some(a) = c["function"]["arguments"].as_str() {
                            e.args.push_str(a);
                        }
                    }
                }
            }
        }

        let calls = parts
            .into_iter()
            .filter(|(_, p)| !p.name.is_empty())
            .map(|(i, p)| ToolCall {
                id: if p.id.is_empty() {
                    format!("call_{i}")
                } else {
                    p.id
                },
                name: p.name,
                args: if p.args.trim().is_empty() {
                    "{}".to_string()
                } else {
                    p.args
                },
            })
            .collect();
        Ok(ChatReply { text, tool_calls: calls })
    }

    // -- Gemini -------------------------------------------------------------

    async fn gemini_chat(
        &self,
        model: &str,
        messages: &[Msg],
        tools: &[ToolDef],
        temperature: f32,
        on_delta: &mut (dyn FnMut(&str) + Send),
    ) -> Result<ChatReply, String> {
        let mut system = String::new();
        // contents: merge consecutive same-role turns (Gemini likes clean
        // alternation); tool results ride as user functionResponse parts.
        let mut contents: Vec<(String, Vec<Value>)> = Vec::new();
        for m in messages {
            match m.role.as_str() {
                "system" => {
                    if !system.is_empty() {
                        system.push_str("\n\n");
                    }
                    system.push_str(&m.content);
                }
                "user" => push_content(&mut contents, "user", json!({ "text": m.content })),
                "assistant" => {
                    if !m.content.is_empty() {
                        push_content(&mut contents, "model", json!({ "text": m.content }));
                    }
                    for c in &m.tool_calls {
                        let args: Value =
                            serde_json::from_str(&c.args).unwrap_or_else(|_| json!({}));
                        push_content(
                            &mut contents,
                            "model",
                            json!({ "functionCall": { "name": c.name, "args": args } }),
                        );
                    }
                }
                "tool" => push_content(
                    &mut contents,
                    "user",
                    json!({ "functionResponse": { "name": m.name, "response": { "output": m.content } } }),
                ),
                _ => {}
            }
        }

        let mut body = json!({
            "contents": contents.iter().map(|(r, parts)| json!({ "role": r, "parts": parts })).collect::<Vec<_>>(),
            "generationConfig": { "temperature": temperature },
        });
        if !system.is_empty() {
            body["systemInstruction"] = json!({ "parts": [{ "text": system }] });
        }
        if !tools.is_empty() {
            body["tools"] = json!([{
                "functionDeclarations": tools.iter().map(|t| {
                    json!({
                        "name": t.name,
                        "description": t.description,
                        "parameters": gemini_schema(t.parameters.clone()),
                    })
                }).collect::<Vec<_>>()
            }]);
        }

        let resp = self
            .http
            .post(self.url(&format!("/models/{model}:generateContent")))
            .query(&[("key", &self.api_key)])
            .json(&body)
            .send()
            .await
            .map_err(err_str)?;
        let status = resp.status();
        let body = resp.text().await.map_err(err_str)?;
        if !status.is_success() {
            return Err(format!("HTTP {}: {}", status.as_u16(), clip(&body, 400)));
        }
        let v: Value = serde_json::from_str(&body).map_err(|e| format!("bad JSON: {e}"))?;
        if let Some(err) = v.get("error") {
            return Err(format!("provider error: {}", clip(&err.to_string(), 300)));
        }
        let parts = v["candidates"][0]["content"]["parts"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let mut text = String::new();
        let mut calls = Vec::new();
        for (i, p) in parts.iter().enumerate() {
            if let Some(t) = p["text"].as_str() {
                text.push_str(t);
            }
            if let Some(fc) = p.get("functionCall") {
                calls.push(ToolCall {
                    id: format!("gem_{i}"),
                    name: fc["name"].as_str().unwrap_or("").to_string(),
                    args: fc["args"].to_string(),
                });
            }
        }
        if !text.is_empty() {
            on_delta(&text);
        }
        Ok(ChatReply { text, tool_calls: calls })
    }

    // -- Anthropic ----------------------------------------------------------

    async fn anthropic_chat(
        &self,
        model: &str,
        messages: &[Msg],
        tools: &[ToolDef],
        temperature: f32,
        on_delta: &mut (dyn FnMut(&str) + Send),
    ) -> Result<ChatReply, String> {
        let mut system = String::new();
        let mut turns: Vec<(String, Vec<Value>)> = Vec::new();
        for m in messages {
            match m.role.as_str() {
                "system" => {
                    if !system.is_empty() {
                        system.push_str("\n\n");
                    }
                    system.push_str(&m.content);
                }
                "user" => push_content(&mut turns, "user", json!({ "type": "text", "text": m.content })),
                "assistant" => {
                    if !m.content.is_empty() {
                        push_content(&mut turns, "assistant", json!({ "type": "text", "text": m.content }));
                    }
                    for c in &m.tool_calls {
                        let input: Value =
                            serde_json::from_str(&c.args).unwrap_or_else(|_| json!({}));
                        push_content(
                            &mut turns,
                            "assistant",
                            json!({ "type": "tool_use", "id": c.id, "name": c.name, "input": input }),
                        );
                    }
                }
                "tool" => push_content(
                    &mut turns,
                    "user",
                    json!({ "type": "tool_result", "tool_use_id": m.tool_call_id, "content": m.content }),
                ),
                _ => {}
            }
        }

        let mut body = json!({
            "model": model,
            "max_tokens": 4096,
            "temperature": temperature,
            "messages": turns.iter().map(|(r, parts)| json!({ "role": r, "content": parts })).collect::<Vec<_>>(),
        });
        if !system.is_empty() {
            body["system"] = json!(system);
        }
        if !tools.is_empty() {
            body["tools"] = Value::Array(
                tools
                    .iter()
                    .map(|t| {
                        json!({
                            "name": t.name,
                            "description": t.description,
                            "input_schema": t.parameters,
                        })
                    })
                    .collect(),
            );
        }

        let resp = self
            .http
            .post(self.url("/v1/messages"))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&body)
            .send()
            .await
            .map_err(err_str)?;
        let status = resp.status();
        let body = resp.text().await.map_err(err_str)?;
        if !status.is_success() {
            return Err(format!("HTTP {}: {}", status.as_u16(), clip(&body, 400)));
        }
        let v: Value = serde_json::from_str(&body).map_err(|e| format!("bad JSON: {e}"))?;
        if let Some(err) = v.get("error") {
            return Err(format!("provider error: {}", clip(&err.to_string(), 300)));
        }
        let blocks = v["content"].as_array().cloned().unwrap_or_default();
        let mut text = String::new();
        let mut calls = Vec::new();
        for (i, b) in blocks.iter().enumerate() {
            if b["type"] == "text" {
                if let Some(t) = b["text"].as_str() {
                    text.push_str(t);
                }
            } else if b["type"] == "tool_use" {
                calls.push(ToolCall {
                    id: b["id"]
                        .as_str()
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| format!("toolu_{i}")),
                    name: b["name"].as_str().unwrap_or("").to_string(),
                    args: b["input"].to_string(),
                });
            }
        }
        if !text.is_empty() {
            on_delta(&text);
        }
        Ok(ChatReply { text, tool_calls: calls })
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Append a part, merging into the previous content when the role matches
/// (Anthropic requires alternating roles; Gemini prefers it).
fn push_content(contents: &mut Vec<(String, Vec<Value>)>, role: &str, part: Value) {
    if let Some((last_role, parts)) = contents.last_mut() {
        if last_role == role {
            parts.push(part);
            return;
        }
    }
    contents.push((role.to_string(), vec![part]));
}

fn openai_msg(m: &Msg) -> Value {
    match m.role.as_str() {
        "assistant" => {
            let mut v = json!({ "role": "assistant", "content": m.content });
            if !m.tool_calls.is_empty() {
                v["tool_calls"] = Value::Array(
                    m.tool_calls
                        .iter()
                        .map(|c| {
                            json!({
                                "id": c.id,
                                "type": "function",
                                "function": { "name": c.name, "arguments": c.args },
                            })
                        })
                        .collect(),
                );
            }
            v
        }
        "tool" => json!({
            "role": "tool",
            "tool_call_id": m.tool_call_id,
            "content": m.content,
        }),
        role => json!({ "role": role, "content": m.content }),
    }
}

fn openai_tool(t: &ToolDef) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": t.name,
            "description": t.description,
            "parameters": t.parameters,
        }
    })
}

/// Gemini wants upper-case schema types and rejects a few JSON-Schema keys.
fn gemini_schema(v: Value) -> Value {
    match v {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, val) in map {
                match k.as_str() {
                    "type" => {
                        if let Some(t) = val.as_str() {
                            out.insert("type".into(), json!(t.to_uppercase()));
                            continue;
                        }
                        out.insert("type".into(), gemini_schema(val));
                    }
                    "properties" => {
                        if let Value::Object(props) = val {
                            let mut p2 = serde_json::Map::new();
                            for (pk, pv) in props {
                                p2.insert(pk, gemini_schema(pv));
                            }
                            out.insert("properties".into(), Value::Object(p2));
                        }
                    }
                    "items" | "additionalProperties" if k == "items" => {
                        out.insert("items".into(), gemini_schema(val));
                    }
                    "additionalProperties" => {}
                    _ => {
                        out.insert(k, val);
                    }
                }
            }
            Value::Object(out)
        }
        Value::Array(arr) => Value::Array(arr.into_iter().map(gemini_schema).collect()),
        other => other,
    }
}
