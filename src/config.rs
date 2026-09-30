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
    pub fn for_mode(&self, mode: crate::modes::Mode) -> &str {
        use crate::modes::Mode::*;
        match mode {
            Plan => &self.plan,
            Build => &self.build,
            Explore => &self.explore,
            Retro => &self.retro,
        }
    }
}

/// Per-model limits. Keyed by model/deployment name so that switching models
/// (per mode, per run) carries the right token budget and context size.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelLimits {
    /// Max cumulative tokens for a run on this model.
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
    fn from_legacy(cfg: &Config) -> Self {
        ProviderConfig {
            api_key: cfg.api_key.clone(),
            api_key_env: cfg.api_key_env.clone(),
            auth_file: cfg.auth_file.clone(),
            auth_key: cfg.provider.clone(),
            models: cfg.models.clone(),
            base_url: cfg.base_url.clone(),
            ..Default::default()
        }
    }

    /// Inherit any empty fields from the legacy top-level config, and derive
    /// `auth` from `kind` when unset.
    fn filled_from(mut self, cfg: &Config, name: &str) -> Self {
        if self.base_url.is_empty() {
            self.base_url = cfg.base_url.clone();
        }
        if self.api_key.is_empty() {
            self.api_key = cfg.api_key.clone();
        }
        if self.api_key_env.is_empty() {
            self.api_key_env = cfg.api_key_env.clone();
        }
        if self.auth_file.is_empty() {
            self.auth_file = cfg.auth_file.clone();
        }
        if self.auth_key.is_empty() {
            self.auth_key = if name.is_empty() {
                cfg.provider.clone()
            } else {
                name.to_string()
            };
        }
        if self.kind.is_empty() {
            self.kind = "openai".into();
        }
        if self.auth.is_empty() {
            self.auth = if self.kind == "azure" {
                "api-key".into()
            } else {
                "bearer".into()
            };
        }
        if self.max_tokens_field.is_empty() {
            self.max_tokens_field = "max_tokens".into();
        }
        self
    }

    pub fn is_azure(&self) -> bool {
        self.kind.eq_ignore_ascii_case("azure")
    }

    /// Explicit key → env var → auth file. Empty for local servers with no auth.
    pub fn resolve_api_key(&self) -> String {
        if !self.api_key.is_empty() {
            return self.api_key.clone();
        }
        if !self.api_key_env.is_empty() {
            if let Ok(v) = std::env::var(&self.api_key_env) {
                if !v.is_empty() {
                    return v;
                }
            }
        }
        let path = expand_tilde(&self.auth_file);
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                if let Some(key) = v
                    .get(&self.auth_key)
                    .and_then(|p| p.get("key"))
                    .and_then(|k| k.as_str())
                {
                    return key.to_string();
                }
            }
        }
        String::new()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub provider: String,
    pub providers: BTreeMap<String, ProviderConfig>,
    pub base_url: String,
    pub api_key: String,
    pub api_key_env: String,
    pub auth_file: String,
    pub default_model: String,
    pub models: ModelsConfig,
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
    pub max_cycles: usize,
    pub db_path: String,
    pub requirements_dir: String,
    pub plans_dir: String,
    pub tickets_dir: String,
    pub skills_dir: String,
    pub tmp_dir: String,
    pub control_socket: String,
    pub control_enabled: bool,
    pub auto_ingest_requirements: bool,
}

impl Default for Config {
    fn default() -> Self {
        let mut providers = BTreeMap::new();
        let mut model_limits = BTreeMap::new();
        model_limits.insert(
            "qwen2.5-coder-7b".into(),
            ModelLimits {
                token_limit: Some(2_000_000),
                context_window: Some(32_768),
                max_output_tokens: Some(4_096),
            },
        );
        providers.insert(
            "local".into(),
            ProviderConfig {
                kind: "openai".into(),
                base_url: "http://127.0.0.1:8080/v1".into(),
                model: "qwen2.5-coder-7b".into(),
                model_limits,
                ..Default::default()
            },
        );
        Self {
            provider: "local".into(),
            providers,
            base_url: "http://127.0.0.1:8080/v1".into(),
            api_key: String::new(),
            api_key_env: String::new(),
            auth_file: String::new(),
            default_model: "qwen2.5-coder-7b".into(),
            models: ModelsConfig {
                plan: "qwen2.5-coder-7b".into(),
                build: "qwen2.5-coder-7b".into(),
                explore: "qwen2.5-coder-7b".into(),
                retro: "qwen2.5-coder-7b".into(),
            },
            token_limit: 2_000_000,
            time_limit_secs: 1800,
            compact_threshold: 0.70,
            compact_keep_recent: 6,
            context_window: 32_768,
            max_output_tokens: 4_096,
            tool_result_max_bytes: 24_000,
            max_tool_iterations: 80,
            bash_timeout_secs: 120,
            spawn_timeout_secs: 900,
            max_subagent_depth: 2,
            max_cycles: 30,
            db_path: ".genji/genji.db".into(),
            requirements_dir: ".genji/requirements".into(),
            plans_dir: ".genji/plans".into(),
            tickets_dir: ".genji/tickets".into(),
            skills_dir: ".genji/skills".into(),
            tmp_dir: ".genji/tmp".into(),
            control_socket: ".genji/control.sock".into(),
            control_enabled: true,
            auto_ingest_requirements: true,
        }
    }
}

impl Config {
    pub fn path_in(workspace: &Path) -> PathBuf {
        workspace.join(".genji/config.json")
    }

    /// Load config from disk, creating it with defaults if missing.
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

    pub fn requirements_path(&self, workspace: &Path) -> PathBuf {
        self.workspace_path(workspace, &self.requirements_dir)
    }
    pub fn plans_path(&self, workspace: &Path) -> PathBuf {
        self.workspace_path(workspace, &self.plans_dir)
    }
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
        match self.providers.get(&self.provider) {
            Some(p) => p.clone().filled_from(self, &self.provider),
            None => ProviderConfig::from_legacy(self).filled_from(self, &self.provider),
        }
    }

    pub fn model_for_mode(&self, mode: crate::modes::Mode) -> String {
        let p = self.resolve_active_provider();
        if !p.model.trim().is_empty() {
            return p.model.clone();
        }
        let m = p.models.for_mode(mode);
        if !m.trim().is_empty() {
            return m.to_string();
        }
        let top = self.models.for_mode(mode);
        if !top.trim().is_empty() {
            return top.to_string();
        }
        self.default_model.clone()
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
    if let Some(rest) = p.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(p)
}
