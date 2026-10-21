//! Provider-agnostic LLM layer.
//!
//! One [`Provider`] value wraps any backend: the OpenAI-compatible family
//! (OpenRouter, OpenAI, Groq, Ollama, GitHub Copilot, custom endpoints) speaks
//! `/chat/completions`
//! with SSE streaming; Gemini and Anthropic get native wire formats. All
//! callers (engine, settings UI) use [`Provider::chat`] and
//! [`Provider::list_models`] and never touch provider differences.

use crate::ai::config::AiConfig;
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::time::Duration;
use tokio::sync::Mutex as AsyncMutex;

/// GitHub's public Copilot device-login app (the one Copilot CLI / VS Code
/// use). Device flow needs no client secret, so a desktop app can use it.
const COPILOT_CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";
const COPILOT_EDITOR_VERSION: &str = "vscode/1.95.0";
const COPILOT_PLUGIN_VERSION: &str = "copilot-chat/0.26.7";
const COPILOT_USER_AGENT: &str = "GitHubCopilotChat/0.26.7";
const COPILOT_API_VERSION: &str = "2025-04-01";
const COPILOT_DEFAULT_BASE: &str = "https://api.githubcopilot.com";
const COPILOT_TOKEN_URL: &str = "https://api.github.com/copilot_internal/v2/token";
const GITHUB_DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
const GITHUB_ACCESS_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";

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
    GitHubCopilot,
    Gemini,
    Anthropic,
    Custom,
}

impl Flavor {
    pub const ALL: [Flavor; 8] = [
        Flavor::OpenRouter,
        Flavor::OpenAi,
        Flavor::Groq,
        Flavor::Ollama,
        Flavor::GitHubCopilot,
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
            Flavor::GitHubCopilot => "github-copilot",
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
            "github-copilot" | "github_copilot" | "copilot" => Flavor::GitHubCopilot,
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
            Flavor::GitHubCopilot => "GitHub Copilot",
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
            Flavor::GitHubCopilot => COPILOT_DEFAULT_BASE,
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
            Flavor::GitHubCopilot => "sign in with GitHub below",
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
    /// Short-lived Copilot session tokens, cached until `expires_at`.
    copilot: AsyncMutex<Option<CopilotSession>>,
}

struct CopilotSession {
    token: String,
    api_base: String,
    expires_at: i64,
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
            copilot: AsyncMutex::new(None),
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
            Flavor::GitHubCopilot => self.copilot_models().await,
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

    async fn copilot_models(&self) -> Result<Vec<String>, String> {
        let (token, base) = self.copilot_session().await?;
        let mut req = self
            .http
            .get(format!("{}/models", base.trim_end_matches('/')))
            .bearer_auth(&token);
        for (k, v) in copilot_headers("user") {
            req = req.header(k, v);
        }
        let resp = req.send().await.map_err(err_str)?;
        let status = resp.status();
        let body = resp.text().await.map_err(err_str)?;
        if !status.is_success() {
            return Err(format!(
                "GitHub Copilot model list failed (HTTP {}): {}",
                status.as_u16(),
                clip(&body, 240)
            ));
        }
        let v: Value = serde_json::from_str(&body).map_err(|e| format!("bad JSON: {e}"))?;
        let mut out = copilot_model_ids(&v);
        if out.is_empty() {
            return Err("GitHub returned no Copilot chat models for this account. Check that the Copilot subscription is active and allows API use.".into());
        }
        out.sort_by_key(|m| m.to_ascii_lowercase());
        out.dedup();
        Ok(out)
    }

    /// Exchange the long-lived GitHub login token for a short-lived Copilot
    /// API token, caching it until just before it expires.
    async fn copilot_session(&self) -> Result<(String, String), String> {
        let now = chrono::Utc::now().timestamp();
        {
            let guard = self.copilot.lock().await;
            if let Some(s) = guard.as_ref() {
                if s.expires_at > now + 60 {
                    return Ok((s.token.clone(), s.api_base.clone()));
                }
            }
        }

        let github = self.api_key.trim();
        if github.is_empty() {
            return Err(
                "GitHub Copilot is not signed in. Open AI settings and press \"Sign in with GitHub\"."
                    .into(),
            );
        }
        let resp = self
            .http
            .get(COPILOT_TOKEN_URL)
            .header("Authorization", format!("token {github}"))
            .header("Accept", "application/json")
            .header("Content-Type", "application/json")
            .header("Editor-Version", COPILOT_EDITOR_VERSION)
            .header("Editor-Plugin-Version", COPILOT_PLUGIN_VERSION)
            .header("User-Agent", COPILOT_USER_AGENT)
            .header("X-GitHub-Api-Version", COPILOT_API_VERSION)
            .send()
            .await
            .map_err(err_str)?;
        let status = resp.status();
        if !status.is_success() {
            return Err(match status.as_u16() {
                401 => "GitHub rejected the Copilot sign-in. Sign in again from AI settings.".to_string(),
                403 => "This GitHub account does not have Copilot API access. Check that the Copilot plan is active and allows API use, or sign in with another account.".to_string(),
                404 => "GitHub Copilot is not enabled for this account.".to_string(),
                429 => "GitHub rate-limited the Copilot token request. Wait a minute and try again.".to_string(),
                code if (500..=599).contains(&code) => {
                    "GitHub Copilot is temporarily unavailable. Try again shortly.".to_string()
                }
                code => format!("GitHub Copilot token request failed (HTTP {code})."),
            });
        }
        let v: Value = resp.json().await.map_err(err_str)?;
        let token = v["token"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "GitHub returned no Copilot API token.".to_string())?;
        let expires_at = v["expires_at"]
            .as_i64()
            .or_else(|| v["expires_at"].as_str().and_then(|s| s.parse().ok()))
            .unwrap_or(now + 1200);
        let discovered = v["endpoints"]["api"].as_str().unwrap_or("");
        let api_base = copilot_api_base(&self.base_url, discovered);
        let session = CopilotSession {
            token: token.to_string(),
            api_base: api_base.clone(),
            expires_at,
        };
        *self.copilot.lock().await = Some(session);
        Ok((token.to_string(), api_base))
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
            Flavor::GitHubCopilot => {
                self.copilot_chat(model, messages, tools, temperature, stream, on_delta)
                    .await
            }
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
        self.openai_chat_with(
            self.url("/chat/completions"),
            if self.api_key.is_empty() {
                None
            } else {
                Some(self.api_key.as_str())
            },
            &[],
            model,
            messages,
            tools,
            Some(temperature),
            stream,
            on_delta,
        )
        .await
    }

    /// GitHub Copilot speaks the OpenAI wire format behind a short-lived
    /// session token and a handful of Copilot-specific headers.
    async fn copilot_chat(
        &self,
        model: &str,
        messages: &[Msg],
        tools: &[ToolDef],
        temperature: f32,
        stream: bool,
        on_delta: &mut (dyn FnMut(&str) + Send),
    ) -> Result<ChatReply, String> {
        let (token, base) = self.copilot_session().await?;
        let initiator = if messages
            .iter()
            .any(|m| m.role == "assistant" || m.role == "tool")
        {
            "agent"
        } else {
            "user"
        };
        let headers = copilot_headers(initiator);
        // Reasoning models reject an explicit temperature on some Copilot
        // backends; omit it there and let the model use its default.
        let temp = copilot_temperature_supported(model).then_some(temperature);
        let endpoint = format!("{}/chat/completions", base.trim_end_matches('/'));
        let result = self
            .openai_chat_with(
                endpoint.clone(),
                Some(&token),
                &headers,
                model,
                messages,
                tools,
                temp,
                stream,
                on_delta,
            )
            .await;
        // Older Copilot Claude backends rejected the `system` role. Retry
        // once with the system prompt as an assistant turn, before any
        // streaming deltas can have been emitted.
        if let Err(e) = &result {
            if e.contains("HTTP 400") && e.to_ascii_lowercase().contains("system") {
                let converted: Vec<Msg> = messages
                    .iter()
                    .map(|m| {
                        if m.role == "system" {
                            Msg {
                                role: "assistant".into(),
                                ..m.clone()
                            }
                        } else {
                            m.clone()
                        }
                    })
                    .collect();
                return self
                    .openai_chat_with(
                        endpoint,
                        Some(&token),
                        &headers,
                        model,
                        &converted,
                        tools,
                        temp,
                        stream,
                        on_delta,
                    )
                    .await;
            }
        }
        result
    }

    #[allow(clippy::too_many_arguments)]
    async fn openai_chat_with(
        &self,
        endpoint: String,
        bearer: Option<&str>,
        extra: &[(&str, String)],
        model: &str,
        messages: &[Msg],
        tools: &[ToolDef],
        temperature: Option<f32>,
        stream: bool,
        on_delta: &mut (dyn FnMut(&str) + Send),
    ) -> Result<ChatReply, String> {
        let wire: Vec<Value> = messages.iter().map(openai_msg).collect();
        let mut body = json!({
            "model": model,
            "messages": wire,
            "stream": stream,
        });
        if let Some(t) = temperature {
            body["temperature"] = json!(t);
        }
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools.iter().map(openai_tool).collect());
            body["tool_choice"] = json!("auto");
        }

        let mut req = self.http.post(endpoint).json(&body);
        if let Some(key) = bearer.filter(|k| !k.is_empty()) {
            req = req.bearer_auth(key);
        }
        for (k, v) in extra {
            req = req.header(*k, v);
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
            let (text, calls) = parse_openai_reply(&v);
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
                // Copilot's newer Claude models can stream Anthropic-native
                // events even through the OpenAI-compatible endpoint.
                if v["type"].as_str() == Some("message_stop") {
                    done = true;
                    break;
                }
                if v["type"].as_str() == Some("content_block_start")
                    && v["content_block"]["type"].as_str() == Some("tool_use")
                {
                    let idx = v["index"].as_u64().unwrap_or(0) as usize;
                    let e = parts.entry(idx).or_default();
                    if let Some(id) = v["content_block"]["id"].as_str() {
                        e.id = id.to_string();
                    }
                    if let Some(name) = v["content_block"]["name"].as_str() {
                        e.name = name.to_string();
                    }
                    if let Some(input) = v["content_block"].get("input") {
                        let has_input = !input.is_null()
                            && input.as_object().is_none_or(|o| !o.is_empty());
                        if has_input {
                            e.args = input.to_string();
                        }
                    }
                }
                if v["type"].as_str() == Some("content_block_delta") {
                    let delta = &v["delta"];
                    match delta["type"].as_str() {
                        Some("text_delta") => {
                            if let Some(c) = delta["text"].as_str() {
                                if !c.is_empty() {
                                    text.push_str(c);
                                    on_delta(c);
                                }
                            }
                        }
                        Some("input_json_delta") => {
                            let idx = v["index"].as_u64().unwrap_or(0) as usize;
                            let e = parts.entry(idx).or_default();
                            if let Some(p) = delta["partial_json"].as_str() {
                                e.args.push_str(p);
                            }
                        }
                        _ => {}
                    }
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

// ---------------------------------------------------------------------------
// GitHub Copilot helpers
// ---------------------------------------------------------------------------

/// Headers required by the Copilot chat and model endpoints. Authorization is
/// added by the caller (it is the short-lived Copilot token, not the GitHub
/// login token).
fn copilot_headers(initiator: &str) -> Vec<(&'static str, String)> {
    vec![
        ("Accept", "application/json".into()),
        ("Copilot-Integration-Id", "vscode-chat".into()),
        ("Editor-Version", COPILOT_EDITOR_VERSION.into()),
        ("Editor-Plugin-Version", COPILOT_PLUGIN_VERSION.into()),
        ("User-Agent", COPILOT_USER_AGENT.into()),
        ("Openai-Intent", "conversation-panel".into()),
        ("X-GitHub-Api-Version", COPILOT_API_VERSION.into()),
        ("X-Request-Id", uuid::Uuid::new_v4().to_string()),
        (
            "X-Vscode-User-Agent-Library-Version",
            "electron-fetch".into(),
        ),
        ("X-Initiator", initiator.into()),
    ]
}

/// Prefer an explicit base URL, then the endpoint GitHub returns for the
/// account (Business/Enterprise differ), then the public default. Copilot's
/// API lives at the root, so a user-pasted `/v1` suffix is removed.
fn copilot_api_base(configured: &str, discovered: &str) -> String {
    let configured = configured.trim().trim_end_matches('/');
    let discovered = discovered.trim().trim_end_matches('/');
    let chosen = if !configured.is_empty() && configured != COPILOT_DEFAULT_BASE {
        configured
    } else if !discovered.is_empty() {
        discovered
    } else if !configured.is_empty() {
        configured
    } else {
        COPILOT_DEFAULT_BASE
    };
    chosen.trim_end_matches("/v1").trim_end_matches('/').to_string()
}

/// Chat-capable model ids from `GET /models`. `model_picker_enabled` is
/// intentionally ignored: GitHub currently reports false for every model even
/// though the model can still be selected by id.
fn copilot_model_ids(v: &Value) -> Vec<String> {
    let Some(arr) = v["data"].as_array().or_else(|| v["models"].as_array()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for m in arr {
        let kind = m["capabilities"]["type"].as_str().unwrap_or("chat");
        if kind.eq_ignore_ascii_case("embeddings") {
            continue;
        }
        let Some(id) = m["id"].as_str().filter(|s| !s.is_empty()) else {
            continue;
        };
        if id.to_ascii_lowercase().contains("embedding") {
            continue;
        }
        out.push(id.to_string());
    }
    out
}

/// Some reasoning models reject an explicit temperature. Let those use their
/// own default instead of failing the whole turn.
fn copilot_temperature_supported(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    !(m.starts_with("o1")
        || m.starts_with("o3")
        || m.starts_with("o4")
        || m.contains("gpt-5")
        || m.contains("codex"))
}

/// Parse an OpenAI-shaped reply, with a fallback for Copilot's newer Claude
/// models, which can return Anthropic-native content blocks without a
/// `choices` array.
fn parse_openai_reply(v: &Value) -> (String, Vec<ToolCall>) {
    if let Some(msg) = v["choices"][0]["message"].as_object() {
        let text = msg
            .get("content")
            .and_then(|c| c.as_str())
            .map(|s| s.to_string())
            .or_else(|| {
                msg.get("content").and_then(|c| c.as_array()).map(|a| {
                    a.iter()
                        .filter_map(|p| p["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("")
                })
            })
            .unwrap_or_default();
        let mut calls = Vec::new();
        if let Some(arr) = msg.get("tool_calls").and_then(|c| c.as_array()) {
            for (i, c) in arr.iter().enumerate() {
                calls.push(ToolCall {
                    id: c["id"]
                        .as_str()
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| format!("call_{i}")),
                    name: c["function"]["name"].as_str().unwrap_or("").to_string(),
                    args: c["function"]["arguments"]
                        .as_str()
                        .unwrap_or("{}")
                        .to_string(),
                });
            }
        }
        return (text, calls);
    }

    let mut text = v["content"].as_str().unwrap_or("").to_string();
    let mut calls = Vec::new();
    if let Some(blocks) = v["content"].as_array() {
        for block in blocks {
            match block["type"].as_str().unwrap_or("") {
                "text" => text.push_str(block["text"].as_str().unwrap_or("")),
                "tool_use" => {
                    let name = block["name"].as_str().unwrap_or("");
                    if name.is_empty() {
                        continue;
                    }
                    let args = match &block["input"] {
                        Value::String(s) => s.clone(),
                        Value::Null => "{}".to_string(),
                        other => other.to_string(),
                    };
                    calls.push(ToolCall {
                        id: block["id"]
                            .as_str()
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| format!("call_{}", calls.len())),
                        name: name.to_string(),
                        args,
                    });
                }
                _ => {}
            }
        }
    }
    (text, calls)
}

struct GitHubDeviceCode {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: u64,
    interval: u64,
}

fn parse_device_code(v: &Value) -> Result<GitHubDeviceCode, String> {
    let device_code = v["device_code"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "GitHub returned no device code.".to_string())?;
    let user_code = v["user_code"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "GitHub returned no user code.".to_string())?;
    let verification_uri = v["verification_uri"]
        .as_str()
        .or_else(|| v["verification_uri_complete"].as_str())
        .filter(|s| {
            s.starts_with("https://github.com/") || s.starts_with("https://www.github.com/")
        })
        .ok_or_else(|| "GitHub returned no verification URL.".to_string())?;
    Ok(GitHubDeviceCode {
        device_code: device_code.to_string(),
        user_code: user_code.to_string(),
        verification_uri: verification_uri.to_string(),
        expires_in: v["expires_in"].as_u64().unwrap_or(900).clamp(60, 3600),
        interval: v["interval"].as_u64().unwrap_or(5).clamp(1, 60),
    })
}

/// Run GitHub's OAuth device flow. `on_code` is called once with the code the
/// user types at `verification_uri`; the returned string is the long-lived
/// GitHub access token (stored with the other secrets, never logged).
pub async fn github_device_login<F>(on_code: F) -> Result<String, String>
where
    F: FnOnce(&str, &str, u64) + Send + 'static,
{
    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| format!("cannot create GitHub login client: {e}"))?;

    let device: Value = http
        .post(GITHUB_DEVICE_CODE_URL)
        .header("Accept", "application/json")
        .header("User-Agent", COPILOT_USER_AGENT)
        .json(&json!({
            "client_id": COPILOT_CLIENT_ID,
            "scope": "read:user",
        }))
        .send()
        .await
        .map_err(err_str)?
        .error_for_status()
        .map_err(err_str)?
        .json()
        .await
        .map_err(err_str)?;
    let code = parse_device_code(&device)?;
    on_code(&code.user_code, &code.verification_uri, code.expires_in);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(code.expires_in);
    let mut interval = code.interval;
    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err("GitHub sign-in timed out. Try again.".into());
        }
        tokio::time::sleep(Duration::from_secs(interval)).await;
        let poll: Value = http
            .post(GITHUB_ACCESS_TOKEN_URL)
            .header("Accept", "application/json")
            .header("User-Agent", COPILOT_USER_AGENT)
            .json(&json!({
                "client_id": COPILOT_CLIENT_ID,
                "device_code": &code.device_code,
                "grant_type": "urn:ietf:params:oauth:grant-type:device_code",
            }))
            .send()
            .await
            .map_err(err_str)?
            .error_for_status()
            .map_err(err_str)?
            .json()
            .await
            .map_err(err_str)?;

        if let Some(token) = poll["access_token"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return Ok(token.to_string());
        }
        match poll["error"].as_str().unwrap_or("") {
            "authorization_pending" => {}
            "slow_down" => interval = interval.saturating_add(5),
            "expired_token" => return Err("The GitHub sign-in code expired. Try again.".into()),
            "access_denied" => return Err("GitHub sign-in was declined.".into()),
            other if !other.is_empty() => {
                let desc = poll["error_description"].as_str().unwrap_or(other);
                return Err(format!("GitHub sign-in failed: {}", clip(desc, 200)));
            }
            _ => return Err("GitHub returned an unexpected sign-in response.".into()),
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copilot_flavor_round_trips() {
        assert_eq!(Flavor::GitHubCopilot.id(), "github-copilot");
        assert_eq!(Flavor::from_id("github-copilot"), Flavor::GitHubCopilot);
        assert_eq!(Flavor::from_id("copilot"), Flavor::GitHubCopilot);
        assert_eq!(Flavor::GitHubCopilot.label(), "GitHub Copilot");
        assert_eq!(
            Flavor::GitHubCopilot.default_base(),
            "https://api.githubcopilot.com"
        );
        assert!(Flavor::ALL.contains(&Flavor::GitHubCopilot));
    }

    #[test]
    fn copilot_model_ids_skip_embeddings_and_keep_chat_models() {
        let v = json!({
            "data": [
                {
                    "id": "gpt-4o",
                    "model_picker_enabled": false,
                    "capabilities": { "type": "chat" }
                },
                {
                    "id": "text-embedding-3-small",
                    "capabilities": { "type": "embeddings" }
                },
                {
                    "id": "legacy-embedding-model",
                    "capabilities": {}
                },
                { "capabilities": { "type": "chat" } },
                {
                    "id": "claude-sonnet-4",
                    "capabilities": { "type": "chat" }
                }
            ]
        });
        assert_eq!(
            copilot_model_ids(&v),
            vec!["gpt-4o".to_string(), "claude-sonnet-4".to_string()]
        );
    }

    #[test]
    fn copilot_model_ids_accept_models_key() {
        let v = json!({ "models": [{ "id": "gpt-5", "capabilities": { "type": "chat" } }] });
        assert_eq!(copilot_model_ids(&v), vec!["gpt-5".to_string()]);
    }

    #[test]
    fn copilot_headers_include_required_copilot_metadata() {
        let headers = copilot_headers("agent");
        let get = |name: &str| {
            headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("Copilot-Integration-Id"), Some("vscode-chat"));
        assert_eq!(get("Editor-Plugin-Version"), Some("copilot-chat/0.26.7"));
        assert_eq!(get("X-GitHub-Api-Version"), Some("2025-04-01"));
        assert_eq!(get("X-Initiator"), Some("agent"));
        assert!(get("X-Request-Id").is_some_and(|id| !id.is_empty()));
    }

    #[test]
    fn copilot_api_base_uses_discovered_endpoint_but_honours_an_override() {
        assert_eq!(
            copilot_api_base("", "https://api.business.githubcopilot.com/"),
            "https://api.business.githubcopilot.com"
        );
        assert_eq!(
            copilot_api_base("https://api.githubcopilot.com", ""),
            "https://api.githubcopilot.com"
        );
        assert_eq!(
            copilot_api_base("https://proxy.example.test/v1/", "https://ignored.test"),
            "https://proxy.example.test"
        );
    }

    #[test]
    fn copilot_temperature_is_omitted_for_reasoning_models() {
        assert!(!copilot_temperature_supported("o3-mini"));
        assert!(!copilot_temperature_supported("gpt-5.1-codex"));
        assert!(copilot_temperature_supported("gpt-4o"));
        assert!(copilot_temperature_supported("claude-sonnet-4"));
    }

    #[test]
    fn parse_openai_reply_handles_openai_tool_calls() {
        let v = json!({
            "choices": [{
                "message": {
                    "content": "checking",
                    "tool_calls": [{
                        "id": "call_1",
                        "function": {
                            "name": "create_server",
                            "arguments": "{\"name\":\"staging\"}"
                        }
                    }]
                }
            }]
        });
        let (text, calls) = parse_openai_reply(&v);
        assert_eq!(text, "checking");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].name, "create_server");
        assert_eq!(calls[0].args, "{\"name\":\"staging\"}");
    }

    #[test]
    fn parse_openai_reply_handles_anthropic_native_blocks() {
        let v = json!({
            "content": [
                { "type": "text", "text": "I can do that." },
                {
                    "type": "tool_use",
                    "id": "toolu_1",
                    "name": "add_forward",
                    "input": { "session": "staging", "local_port": 15433 }
                }
            ]
        });
        let (text, calls) = parse_openai_reply(&v);
        assert_eq!(text, "I can do that.");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "toolu_1");
        assert_eq!(calls[0].name, "add_forward");
        assert!(calls[0].args.contains("\"local_port\":15433"));
    }

    #[test]
    fn parse_device_code_rejects_non_https_verification_urls() {
        let ok = json!({
            "device_code": "device",
            "user_code": "ABCD-1234",
            "verification_uri": "https://github.com/login/device",
            "expires_in": 900,
            "interval": 5
        });
        let parsed = parse_device_code(&ok).expect("valid device code");
        assert_eq!(parsed.user_code, "ABCD-1234");
        assert_eq!(parsed.interval, 5);

        let bad = json!({
            "device_code": "device",
            "user_code": "ABCD-1234",
            "verification_uri": "javascript:alert(1)"
        });
        assert!(parse_device_code(&bad).is_err());

        let wrong_host = json!({
            "device_code": "device",
            "user_code": "ABCD-1234",
            "verification_uri": "https://example.test/login/device"
        });
        assert!(parse_device_code(&wrong_host).is_err());
    }
}
