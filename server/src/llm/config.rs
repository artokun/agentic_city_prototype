//! TOML config parsing for LLM providers and session profiles.

use bevy::prelude::*;
use serde::Deserialize;
use std::collections::HashMap;

/// Top-level config from `config/llm.toml`.
#[derive(Debug, Clone, Default, Deserialize, Resource)]
pub struct LlmConfig {
    #[serde(default)]
    pub providers: HashMap<String, ProviderConfig>,
    #[serde(default)]
    pub profiles: HashMap<String, SessionProfile>,
}

/// A configured LLM provider (e.g. a CLI adapter or API adapter).
#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct ProviderConfig {
    /// Provider type string: "claude_cli", "openai_responses", etc.
    #[serde(rename = "type")]
    pub provider_type: String,
    /// Default model for this provider.
    pub model: String,
    /// Environment variable name holding the API key (if needed).
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Provider-specific extra settings.
    #[serde(default)]
    pub extra: HashMap<String, toml::Value>,
}

/// A session profile that binds a provider + model + tuning.
#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct SessionProfile {
    /// Key into `providers` table.
    pub provider: String,
    /// Model override (uses provider default if absent).
    #[serde(default)]
    pub model: Option<String>,
    /// System prompt file path (relative to project root).
    #[serde(default)]
    pub system_prompt_file: Option<String>,
    /// Token threshold before auto-compaction.
    #[serde(default = "default_compact_threshold")]
    pub compact_threshold: u32,
    /// Which tool sets this profile has access to.
    #[serde(default)]
    pub tool_sets: Vec<String>,
}

fn default_compact_threshold() -> u32 {
    50_000
}

impl LlmConfig {
    /// Load from a TOML file path.
    pub fn from_file(path: &str) -> Result<Self, String> {
        let contents = std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read {path}: {e}"))?;
        toml::from_str(&contents)
            .map_err(|e| format!("failed to parse {path}: {e}"))
    }

    /// Get a session profile by name.
    pub fn profile(&self, name: &str) -> Option<&SessionProfile> {
        self.profiles.get(name)
    }

    /// Get a provider config by name.
    pub fn provider(&self, name: &str) -> Option<&ProviderConfig> {
        self.providers.get(name)
    }

    /// Resolve the effective model for a profile (profile override or provider default).
    pub fn effective_model(&self, profile: &SessionProfile) -> Option<String> {
        if let Some(ref m) = profile.model {
            Some(m.clone())
        } else {
            self.providers.get(&profile.provider).map(|p| p.model.clone())
        }
    }
}
