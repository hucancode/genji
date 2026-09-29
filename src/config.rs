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

/// A named endpoint profile. Switching between a local llama.cpp server and a
/// deployed Azure model is a `provider` change — the rest of the agent (tools,
/// modes, loop) is untouched because every target speaks the OpenAI
/// chat-completions protocol.
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
    /// pi-compatible auth file fallback: { "<auth_key>": { "key": "..." } }.
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
    pub context_window: Option<i64>,
    /// Override the top-level max output tokens for this provider.
    pub max_output_tokens: Option<i64>,
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

    /// Explicit key → env var → pi auth file. Empty for local servers with no auth.
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

/// Runtime configuration. Everything the user is expected to tune lives here.
/// Loaded from `agent.config.json` in the workspace (created with defaults on
/// first run). Paths are resolved relative to the workspace root.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    // ---- provider selection ----
    /// Active provider profile name (a key of `providers`). For a single-endpoint
    /// setup this also names the entry looked up in the auth file.
    pub provider: String,
    /// Named endpoint profiles. `--provider` / `GENJI_PROVIDER` selects one.
    pub providers: BTreeMap<String, ProviderConfig>,

    // ---- legacy / fallback endpoint fields (used when no profile matches) ----
    pub base_url: String,
    pub api_key: String,
    pub api_key_env: String,
    pub auth_file: String,
    pub default_model: String,
    pub models: ModelsConfig,

    // ---- budgets ----
    pub token_limit: i64,
    pub time_limit_secs: u64,
    pub compact_threshold: f64,
    pub compact_keep_recent: usize,

    // ---- model limits ----
    pub context_window: i64,
    pub max_output_tokens: i64,

    // ---- tools ----
    pub tool_result_max_bytes: usize,
    pub max_tool_iterations: usize,
    pub bash_timeout_secs: u64,
    pub spawn_timeout_secs: u64,
    pub max_subagent_depth: u32,

    // ---- cycling ----
    pub max_cycles: usize,

    // ---- paths ----
    pub db_path: String,
    pub requirements_dir: String,
    pub skills_dir: String,
    pub prompts_dir: String,

    // ---- control socket ----
    pub control_socket: String,
    pub control_enabled: bool,

    // ---- behaviour ----
    pub auto_ingest_requirements: bool,
    pub interactive: bool,
    pub verbose: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            provider: "deepseek".into(),
            providers: BTreeMap::new(),
            base_url: "https://api.deepseek.com".into(),
            api_key: String::new(),
            api_key_env: "DEEPSEEK_API_KEY".into(),
            auth_file: "~/.pi/agent/auth.json".into(),
            default_model: "deepseek-flash".into(),
            models: ModelsConfig {
                plan: "deepseek-flash".into(),
                build: "deepseek-flash".into(),
                explore: "deepseek-flash".into(),
                retro: "deepseek-v4-pro".into(),
            },
            token_limit: 2_000_000,
            time_limit_secs: 1800,
            compact_threshold: 0.70,
            compact_keep_recent: 6,
            context_window: 1_000_000,
            max_output_tokens: 16_000,
            tool_result_max_bytes: 24_000,
            max_tool_iterations: 80,
            bash_timeout_secs: 120,
            spawn_timeout_secs: 900,
            max_subagent_depth: 2,
            max_cycles: 30,
            db_path: ".genji/genji.db".into(),
            requirements_dir: "requirements".into(),
            skills_dir: "skills".into(),
            prompts_dir: "prompts".into(),
            control_socket: ".genji/control.sock".into(),
            control_enabled: true,
            auto_ingest_requirements: true,
            interactive: false,
            verbose: false,
        }
    }
}

impl Config {
    pub fn path_in(workspace: &Path) -> PathBuf {
        workspace.join("agent.config.json")
    }

    /// Load config from disk, creating it with defaults if missing.
    pub fn load_or_create(workspace: &Path) -> Result<Self> {
        let path = Self::path_in(workspace);
        if !path.exists() {
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
    pub fn skills_path(&self, workspace: &Path) -> PathBuf {
        self.workspace_path(workspace, &self.skills_dir)
    }
    pub fn prompts_path(&self, workspace: &Path) -> PathBuf {
        self.workspace_path(workspace, &self.prompts_dir)
    }
    pub fn control_path(&self, workspace: &Path) -> PathBuf {
        self.workspace_path(workspace, &self.control_socket)
    }

    /// Resolve the active profile, filling gaps from the legacy top-level
    /// fields so old configs and partial profiles both work.
    pub fn resolve_active_provider(&self) -> ProviderConfig {
        match self.providers.get(&self.provider) {
            Some(p) => p.clone().filled_from(self, &self.provider),
            None => ProviderConfig::from_legacy(self).filled_from(self, &self.provider),
        }
    }

    /// Per-mode model/deployment: profile single model → profile per-mode →
    /// top-level per-mode → `default_model`.
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
}

pub fn expand_tilde(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modes::Mode;

    #[test]
    fn legacy_flat_config_still_resolves() {
        let mut cfg = Config::default();
        cfg.base_url = "http://127.0.0.1:8080/v1".into();
        cfg.api_key = "sk-legacy".into();
        let p = cfg.resolve_active_provider();
        assert_eq!(p.kind, "openai");
        assert_eq!(p.base_url, "http://127.0.0.1:8080/v1");
        assert_eq!(p.auth, "bearer");
        assert_eq!(p.resolve_api_key(), "sk-legacy");
        assert_eq!(cfg.model_for_mode(Mode::Plan), "deepseek-flash");
    }

    #[test]
    fn profile_overrides_and_derives_auth() {
        let mut cfg = Config::default();
        cfg.provider = "local".into();
        cfg.providers.insert(
            "local".into(),
            ProviderConfig {
                base_url: "http://127.0.0.1:8080/v1".into(),
                api_key: "sk-local".into(),
                models: ModelsConfig {
                    plan: "qwen".into(),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        let p = cfg.resolve_active_provider();
        assert_eq!(p.base_url, "http://127.0.0.1:8080/v1");
        assert_eq!(p.auth, "bearer");
        assert_eq!(cfg.model_for_mode(Mode::Plan), "qwen");
    }

    #[test]
    fn azure_profile_derives_api_key_auth_and_single_model() {
        let mut cfg = Config::default();
        cfg.provider = "azure".into();
        cfg.providers.insert(
            "azure".into(),
            ProviderConfig {
                kind: "azure".into(),
                base_url: "https://res.openai.azure.com".into(),
                api_version: "2024-10-21".into(),
                api_key: "azkey".into(),
                model: "gpt-4o".into(),
                max_tokens_field: "max_completion_tokens".into(),
                ..Default::default()
            },
        );
        let p = cfg.resolve_active_provider();
        assert!(p.is_azure());
        assert_eq!(p.auth, "api-key");
        assert_eq!(p.api_version, "2024-10-21");
        // A single `model` overrides every mode (deployment name).
        for m in Mode::all() {
            assert_eq!(cfg.model_for_mode(m), "gpt-4o");
        }
    }

    #[test]
    fn empty_profile_inherits_legacy_base_url() {
        let mut cfg = Config::default();
        cfg.provider = "partial".into();
        cfg.providers
            .insert("partial".into(), ProviderConfig::default());
        let p = cfg.resolve_active_provider();
        assert_eq!(p.base_url, cfg.base_url);
    }
}
