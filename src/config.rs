use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelsConfig {
    pub plan: String,
    pub build: String,
    pub explore: String,
    pub retro: String,
}

impl ModelsConfig {
    pub fn for_mode(&self, mode: crate::storage::modes::Mode) -> &str {
        use crate::storage::modes::Mode::*;
        match mode {
            Plan => &self.plan,
            Build => &self.build,
            Explore => &self.explore,
            Retro => &self.retro,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelLimits {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_limit: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<i64>,
}

#[derive(Debug, Clone, Copy)]
pub struct EffectiveLimits {
    pub token_limit: i64,
    pub context_window: i64,
    pub max_output_tokens: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderConfig {
    /// "openai" (llama.cpp, DeepSeek, OpenAI, OpenRouter, …) or "azure".
    pub kind: String,
    pub base_url: String,
    /// Explicit key (highest priority). May be empty for local servers.
    pub api_key: String,
    /// Env var consulted when `api_key` is empty.
    pub api_key_env: String,
    /// auth file fallback: { "<auth_key>": { "key": "..." } }.
    pub auth_file: String,
    pub auth_key: String,
    /// "bearer" (Authorization) or "api-key" (Azure). Empty = derive from kind.
    pub auth: String,
    /// Azure `api-version` query value.
    pub api_version: String,
    /// Single model/deployment used for every mode (optional override).
    pub model: String,
    /// Per-mode model/deployment names.
    pub models: ModelsConfig,
    /// "max_tokens" (default) or "max_completion_tokens" (some Azure/OpenAI reasoning models).
    pub max_tokens_field: String,
    /// Override the top-level context window for this provider.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<i64>,
    /// Override the top-level max output tokens for this provider.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<i64>,
    /// Override the top-level run token budget for this provider.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_limit: Option<i64>,
    /// Per-model overrides, keyed by model/deployment name. Win over the
    /// provider-level fields above and the top-level values.
    pub model_limits: BTreeMap<String, ModelLimits>,
    /// Send `tool_choice: "auto"` (some endpoints reject it).
    pub send_tool_choice: bool,
    pub extra_headers: BTreeMap<String, String>,
    pub extra_query: BTreeMap<String, String>,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            kind: "openai".into(),
            base_url: String::new(),
            api_key: String::new(),
            api_key_env: String::new(),
            auth_file: String::new(),
            auth_key: String::new(),
            auth: String::new(),
            api_version: String::new(),
            model: String::new(),
            models: ModelsConfig::default(),
            max_tokens_field: "max_tokens".into(),
            context_window: None,
            max_output_tokens: None,
            token_limit: None,
            model_limits: BTreeMap::new(),
            send_tool_choice: true,
            extra_headers: BTreeMap::new(),
            extra_query: BTreeMap::new(),
        }
    }
}

impl ProviderConfig {
    pub fn is_azure(&self) -> bool {
        self.kind.eq_ignore_ascii_case("azure")
    }

    pub fn resolve_api_key(&self) -> String {
        if !self.api_key.is_empty() {
            return self.api_key.clone();
        }
        if !self.api_key_env.is_empty()
            && let Ok(v) = std::env::var(&self.api_key_env)
            && !v.is_empty()
        {
            return v;
        }
        let path = expand_tilde(&self.auth_file);
        if let Ok(text) = std::fs::read_to_string(&path)
            && let Ok(v) = serde_json::from_str::<serde_json::Value>(&text)
            && let Some(key) = v
                .get(&self.auth_key)
                .and_then(|p| p.get("key"))
                .and_then(|k| k.as_str())
        {
            return key.to_string();
        }
        String::new()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub provider: String,
    pub providers: BTreeMap<String, ProviderConfig>,
    pub token_limit: i64,
    pub time_limit_secs: u64,
    pub compact_threshold: f64,
    pub compact_keep_recent: usize,
    pub context_window: i64,
    pub max_output_tokens: i64,
    pub tool_result_max_bytes: usize,
    pub max_tool_iterations: usize,
    pub bash_timeout_secs: u64,
    pub spawn_timeout_secs: u64,
    pub max_subagent_depth: u32,
    #[cfg(feature = "formal")]
    pub max_cycles: usize,
    pub db_path: String,
    #[cfg(feature = "formal")]
    pub requirements_dir: String,
    pub plans_dir: String,
    #[cfg(feature = "formal")]
    pub tickets_dir: String,
    pub skills_dir: String,
    pub tmp_dir: String,
    pub control_socket: String,
    pub control_enabled: bool,
    #[cfg(feature = "formal")]
    pub auto_ingest_requirements: bool,
}

impl Default for Config {
    fn default() -> Self {
        let mut providers = BTreeMap::new();
        let mut model_limits = BTreeMap::new();
        model_limits.insert(
            "qwen3-coder-30b-a3b".into(),
            ModelLimits {
                token_limit: Some(4_000_000),
                context_window: Some(32_768),
                max_output_tokens: Some(8_192),
            },
        );
        providers.insert(
            "local".into(),
            ProviderConfig {
                kind: "openai".into(),
                base_url: "http://127.0.0.1:8080/v1".into(),
                model: "qwen3-coder-30b-a3b".into(),
                model_limits,
                ..Default::default()
            },
        );
        Self {
            provider: "local".into(),
            providers,
            token_limit: 4_000_000,
            time_limit_secs: 1800,
            compact_threshold: 0.70,
            compact_keep_recent: 6,
            context_window: 32_768,
            max_output_tokens: 8_192,
            tool_result_max_bytes: 24_000,
            max_tool_iterations: 80,
            bash_timeout_secs: 120,
            spawn_timeout_secs: 900,
            max_subagent_depth: 2,
            #[cfg(feature = "formal")]
            max_cycles: 30,
            db_path: ".genji/genji.db".into(),
            #[cfg(feature = "formal")]
            requirements_dir: ".genji/requirements".into(),
            plans_dir: ".genji/plans".into(),
            #[cfg(feature = "formal")]
            tickets_dir: ".genji/tickets".into(),
            skills_dir: ".genji/skills".into(),
            tmp_dir: ".genji/tmp".into(),
            control_socket: ".genji/control.sock".into(),
            control_enabled: true,
            #[cfg(feature = "formal")]
            auto_ingest_requirements: true,
        }
    }
}

impl Config {
    pub fn path_in(workspace: &Path) -> PathBuf {
        workspace.join(".genji/config.json")
    }

    pub fn load_or_create(workspace: &Path) -> Result<Self> {
        let path = Self::path_in(workspace);
        if !path.exists() {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            let cfg = Config::default();
            let text = serde_json::to_string_pretty(&cfg)?;
            std::fs::write(&path, format!("{text}\n"))
                .with_context(|| format!("writing default config to {}", path.display()))?;
            eprintln!("[config] created default config at {}", path.display());
            return Ok(cfg);
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let cfg: Config = serde_json::from_str(&text)
            .with_context(|| format!("parsing config {}", path.display()))?;
        Ok(cfg)
    }

    pub fn workspace_path(&self, workspace: &Path, rel: &str) -> PathBuf {
        let p = PathBuf::from(rel);
        if p.is_absolute() {
            p
        } else {
            workspace.join(p)
        }
    }

    pub fn db_file(&self, workspace: &Path) -> PathBuf {
        self.workspace_path(workspace, &self.db_path)
    }

    #[cfg(feature = "formal")]
    pub fn requirements_path(&self, workspace: &Path) -> PathBuf {
        self.workspace_path(workspace, &self.requirements_dir)
    }
    pub fn plans_path(&self, workspace: &Path) -> PathBuf {
        self.workspace_path(workspace, &self.plans_dir)
    }
    #[cfg(feature = "formal")]
    pub fn tickets_path(&self, workspace: &Path) -> PathBuf {
        self.workspace_path(workspace, &self.tickets_dir)
    }
    pub fn skills_path(&self, workspace: &Path) -> PathBuf {
        self.workspace_path(workspace, &self.skills_dir)
    }
    pub fn tmp_path(&self, workspace: &Path) -> PathBuf {
        self.workspace_path(workspace, &self.tmp_dir)
    }
    pub fn control_path(&self, workspace: &Path) -> PathBuf {
        self.workspace_path(workspace, &self.control_socket)
    }

    pub fn resolve_active_provider(&self) -> ProviderConfig {
        let mut provider = self
            .providers
            .get(&self.provider)
            .cloned()
            .unwrap_or_else(|| panic!("configured provider `{}` does not exist", self.provider));
        if provider.auth_key.is_empty() {
            provider.auth_key = self.provider.clone();
        }
        if provider.auth.is_empty() {
            provider.auth = if provider.is_azure() {
                "api-key"
            } else {
                "bearer"
            }
            .into();
        }
        provider
    }

    pub fn model_for_mode(&self, mode: crate::storage::modes::Mode) -> String {
        let p = self.resolve_active_provider();
        if !p.model.trim().is_empty() {
            return p.model;
        }
        let model = p.models.for_mode(mode);
        if model.trim().is_empty() {
            panic!(
                "provider `{}` has no model configured for {}",
                self.provider,
                mode.as_str()
            );
        }
        model.to_string()
    }

    pub fn limits_for_model(&self, model: &str) -> EffectiveLimits {
        let p = self.resolve_active_provider();
        let mut limits = EffectiveLimits {
            token_limit: p.token_limit.unwrap_or(self.token_limit),
            context_window: p.context_window.unwrap_or(self.context_window),
            max_output_tokens: p.max_output_tokens.unwrap_or(self.max_output_tokens),
        };
        if let Some(m) = p.model_limits.get(model) {
            if let Some(v) = m.token_limit {
                limits.token_limit = v;
            }
            if let Some(v) = m.context_window {
                limits.context_window = v;
            }
            if let Some(v) = m.max_output_tokens {
                limits.max_output_tokens = v;
            }
        }
        limits
    }
}

pub fn expand_tilde(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/")
        && let Ok(home) = std::env::var("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(p)
}
