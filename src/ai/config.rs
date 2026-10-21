//! AI Copilot configuration: provider selection, endpoint, credentials,
//! model choice and the security posture used before any data leaves the
//! machine. Persisted next to the other Culvert JSON files
//! (`%APPDATA%\culvert\ai.json`).
//!
//! The API key (or GitHub Copilot login token) is sealed with Windows DPAPI
//! (see `crate::secrets`) before it is written, exactly like saved session
//! passwords — the JSON around it
//! stays readable, the key itself does not.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
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

/// One provider's remembered credentials and endpoint. Keeping these per
/// provider means switching Gemini → Groq → Gemini never loses the Gemini key
/// or model (the top-level fields only describe the *active* provider).
#[derive(Clone, Default, Serialize, Deserialize, Debug)]
pub struct ProviderPrefs {
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub model: String,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct AiConfig {
    /// Flavor id: openrouter | openai | groq | gemini | anthropic | ollama | custom.
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
    /// Remembered key/model/endpoint per provider id.
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderPrefs>,
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
            providers: BTreeMap::new(),
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
    let Some(raw) = std::fs::read_to_string(ai_config_file()).ok() else {
        return AiConfig::default();
    };
    let Ok(mut value) =
        serde_json::from_str::<serde_json::Value>(raw.trim_start_matches('\u{feff}'))
    else {
        return AiConfig::default();
    };
    let migrated = crate::secrets::unprotect_value(&mut value);
    let mut cfg: AiConfig = serde_json::from_value(value).unwrap_or_default();
    seed_active(&mut cfg);
    if migrated {
        // Upgrade a plaintext key to DPAPI on the spot.
        let _ = persist(&cfg);
    }
    cfg
}

/// Fill the active provider's entry from the top-level fields when it has no
/// entry yet (files written before per-provider memory existed).
fn seed_active(cfg: &mut AiConfig) {
    if cfg.api_key.is_empty() && cfg.base_url.is_empty() && cfg.model.is_empty() {
        return;
    }
    let e = cfg.providers.entry(cfg.provider.clone()).or_default();
    if e.api_key.is_empty() {
        e.api_key = cfg.api_key.clone();
    }
    if e.base_url.is_empty() {
        e.base_url = cfg.base_url.clone();
    }
    if e.model.is_empty() {
        e.model = cfg.model.clone();
    }
}

/// Mirror the active provider into its remembered entry, so every save (Test
/// connection, model change, …) is switch-proof.
fn mirror_active(cfg: &mut AiConfig) {
    let e = cfg.providers.entry(cfg.provider.clone()).or_default();
    e.api_key = cfg.api_key.clone();
    e.base_url = cfg.base_url.clone();
    e.model = cfg.model.clone();
}

pub fn persist(cfg: &AiConfig) -> anyhow::Result<()> {
    let path = ai_config_file();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut cfg = cfg.clone();
    mirror_active(&mut cfg);
    let mut value = serde_json::to_value(&cfg)?;
    crate::secrets::protect_value(&mut value);
    std::fs::write(&path, serde_json::to_string_pretty(&value)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn providers_remember_key_and_model_per_provider() {
        // Two providers configured; the top-level fields describe the active
        // one, the map keeps the other's key + model for when we switch back.
        let mut cfg = AiConfig::default();
        cfg.provider = "gemini".into();
        cfg.api_key = "AIza-gemini".into();
        cfg.model = "gemini-2.5-flash".into();
        mirror_active(&mut cfg);
        cfg.providers.insert(
            "groq".into(),
            ProviderPrefs {
                api_key: "gsk_groq".into(),
                base_url: String::new(),
                model: "llama-3.3-70b-versatile".into(),
            },
        );

        let text = serde_json::to_string(&cfg).unwrap();
        let back: AiConfig = serde_json::from_str(&text).unwrap();
        assert_eq!(back.providers["groq"].api_key, "gsk_groq");
        assert_eq!(back.providers["groq"].model, "llama-3.3-70b-versatile");
        assert_eq!(back.providers["gemini"].api_key, "AIza-gemini");
        assert_eq!(back.providers["gemini"].model, "gemini-2.5-flash");
    }

    #[test]
    fn legacy_single_provider_config_seeds_the_map() {
        // An ai.json from before per-provider memory: no `providers` map, one
        // key/model at the top level.
        let legacy: AiConfig = serde_json::from_value(serde_json::json!({
            "provider": "openrouter",
            "api_key": "sk-or-abc",
            "model": "openai/gpt-4o-mini",
            "temperature": 0.2
        }))
        .unwrap();
        assert!(legacy.providers.is_empty());

        let mut cfg = legacy;
        seed_active(&mut cfg);
        assert_eq!(cfg.providers["openrouter"].api_key, "sk-or-abc");
        assert_eq!(cfg.providers["openrouter"].model, "openai/gpt-4o-mini");

        // Seeding never overwrites an existing entry.
        cfg.providers.get_mut("openrouter").unwrap().api_key = "sk-or-new".into();
        cfg.api_key = "sk-or-rewritten".into();
        seed_active(&mut cfg);
        assert_eq!(cfg.providers["openrouter"].api_key, "sk-or-new");
    }
}
