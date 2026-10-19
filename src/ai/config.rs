//! AI Copilot configuration: provider selection, endpoint, credentials,
//! model choice and the security posture used before any data leaves the
//! machine. Persisted next to the other Culvert JSON files
//! (`%APPDATA%\culvert\ai.json`).
//!
//! NOTE: the API key shares the same on-disk tradeoff as saved session
//! passwords — plain JSON in the user profile (exactly what MobaXterm does
//! for saved sessions). Protect your Windows profile accordingly.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// How much data the security layer lets through to an external model.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Debug, Default)]
pub enum SecMode {
    /// The model only ever sees (redacted) command output; file contents are
    /// blocked outright.
    Strict,
    /// File contents are allowed, secrets are auto-redacted and sensitive
    /// files require one explicit confirmation.
    #[default]
    Balanced,
    /// Everything is allowed; sensitive findings still need a confirmation.
    Full,
}

impl SecMode {
    pub const ALL: [SecMode; 3] = [SecMode::Strict, SecMode::Balanced, SecMode::Full];

    pub fn label(&self) -> &'static str {
        match self {
            SecMode::Strict => "STRICT",
            SecMode::Balanced => "BALANCED",
            SecMode::Full => "FULL ACCESS",
        }
    }

    pub fn description(&self) -> &'static str {
        match self {
            SecMode::Strict => {
                "Model sees command output only (redacted) — file contents are blocked."
            }
            SecMode::Balanced => {
                "File contents allowed with automatic redaction; sensitive files ask first."
            }
            SecMode::Full => {
                "Everything may be sent; sensitive findings still require one confirmation."
            }
        }
    }
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct AiConfig {
    /// Flavor id: openrouter | openai | gemini | anthropic | ollama | custom.
    #[serde(default = "def_provider")]
    pub provider: String,
    #[serde(default)]
    pub api_key: String,
    /// Overrides the flavor's default endpoint when non-empty.
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub model: String,
    #[serde(default = "def_temp")]
    pub temperature: f32,
    #[serde(default = "def_true")]
    pub streaming: bool,
    #[serde(default)]
    pub security: SecMode,
    /// Auto-approve read-only commands (ls, cat, df, …). Everything else
    /// still asks; high-risk commands always ask.
    #[serde(default = "def_true")]
    pub auto_safe: bool,
}

fn def_provider() -> String {
    "openrouter".to_string()
}
fn def_temp() -> f32 {
    0.2
}
fn def_true() -> bool {
    true
}

impl Default for AiConfig {
    fn default() -> Self {
        Self {
            provider: def_provider(),
            api_key: String::new(),
            base_url: String::new(),
            model: String::new(),
            temperature: def_temp(),
            streaming: true,
            security: SecMode::Balanced,
            auto_safe: true,
        }
    }
}

fn config_base() -> PathBuf {
    // Same base as session.rs; duplicated here to keep the module standalone.
    dirs::config_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn ai_config_file() -> PathBuf {
    config_base().join("culvert").join("ai.json")
}

pub fn audit_log_file() -> PathBuf {
    config_base().join("culvert").join("ai-audit.log")
}

pub fn load() -> AiConfig {
    std::fs::read_to_string(ai_config_file())
        .ok()
        .and_then(|raw| serde_json::from_str(raw.trim_start_matches('\u{feff}')).ok())
        .unwrap_or_default()
}

pub fn persist(cfg: &AiConfig) -> anyhow::Result<()> {
    let path = ai_config_file();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(cfg)?)?;
    Ok(())
}
