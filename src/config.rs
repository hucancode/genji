use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::storage::util::{split_frontmatter, valid_slug, write_file};

/// Subcommand names an agent may not take.
pub const RESERVED: [&str; 6] = ["list", "stop", "instruct", "inspect", "reset", "help"];

const BUILTIN_AGENTS: [(&str, &str); 4] = [
    ("plan", include_str!("agents/plan.md")),
    ("build", include_str!("agents/build.md")),
    ("explore", include_str!("agents/explore.md")),
    ("retro", include_str!("agents/retro.md")),
];
const BUILTIN_SKILLS: [(&str, &str); 1] = [("formal", include_str!("skills/formal.md"))];

/// `<workspace>/.genji/<name>`.
pub fn dot(workspace: &Path, name: &str) -> PathBuf {
    workspace.join(".genji").join(name)
}

/// One OpenAI-compatible endpoint plus its limits.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Provider {
    pub base_url: String,
    pub api_key: String,
    /// Env var consulted when `api_key` is empty.
    pub api_key_env: String,
    /// "bearer" (Authorization) or "api-key" (Azure).
    pub auth: String,
    pub model: String,
    /// "max_tokens" or "max_completion_tokens".
    pub max_tokens_field: String,
    pub send_tool_choice: bool,
    pub headers: BTreeMap<String, String>,
    pub context_window: i64,
    pub max_output_tokens: i64,
    /// Max tokens (prompt + completion) per run.
    pub token_limit: i64,
}

impl Default for Provider {
    fn default() -> Self {
        Self {
            base_url: "http://127.0.0.1:8080/v1".into(),
            api_key: String::new(),
            api_key_env: String::new(),
            auth: "bearer".into(),
            model: "qwen3-coder-30b-a3b".into(),
            max_tokens_field: "max_tokens".into(),
            send_tool_choice: true,
            headers: BTreeMap::new(),
            context_window: 32_768,
            max_output_tokens: 8_192,
            token_limit: 4_000_000,
        }
    }
}

impl Provider {
    pub fn api_key(&self) -> String {
        if !self.api_key.is_empty() {
            return self.api_key.clone();
        }
        std::env::var(&self.api_key_env).unwrap_or_default()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub provider: String,
    pub providers: BTreeMap<String, Provider>,
    pub time_limit_secs: u64,
    pub compact_threshold: f64,
    pub compact_keep_recent: usize,
    pub tool_result_max_bytes: usize,
    pub max_tool_iterations: usize,
    pub llm_max_retries: u32,
    pub bash_timeout_secs: u64,
    pub spawn_timeout_secs: u64,
    pub max_subagent_depth: u32,
    pub control_enabled: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            provider: "local".into(),
            providers: BTreeMap::from([("local".into(), Provider::default())]),
            time_limit_secs: 1800,
            compact_threshold: 0.70,
            compact_keep_recent: 6,
            tool_result_max_bytes: 24_000,
            max_tool_iterations: 80,
            llm_max_retries: 6,
            bash_timeout_secs: 120,
            spawn_timeout_secs: 900,
            max_subagent_depth: 2,
            control_enabled: true,
        }
    }
}

impl Config {
    /// Read `.genji/config.json`, writing the defaults first when it is missing.
    pub fn load_or_create(workspace: &Path) -> Result<Self> {
        let path = dot(workspace, "config.json");
        if !path.exists() {
            let cfg = Config::default();
            write_file(&path, format!("{}\n", serde_json::to_string_pretty(&cfg)?))?;
            eprintln!("[config] created default config at {}", path.display());
            return Ok(cfg);
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading config {}", path.display()))?;
        serde_json::from_str(&text).with_context(|| format!("parsing config {}", path.display()))
    }

    pub fn provider(&self) -> Result<Provider> {
        self.providers.get(&self.provider).cloned().ok_or_else(|| {
            anyhow::anyhow!("configured provider `{}` does not exist", self.provider)
        })
    }
}

/// An agent: a system prompt, the tools it may call, and what it may declare when it finishes.
#[derive(Debug, Clone)]
pub struct AgentDef {
    pub name: String,
    pub description: String,
    pub prompt: String,
    pub tools: Vec<String>,
    /// Skills rendered into the system prompt at start.
    pub skills: Vec<String>,
    /// Statuses `finish` may use: done | handoff | blocked.
    pub finish: Vec<String>,
    pub model: Option<String>,
}

fn list(meta: &BTreeMap<String, String>, key: &str) -> Option<Vec<String>> {
    meta.get(key).map(|v| {
        v.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    })
}

fn parse_agent(name: &str, text: &str) -> AgentDef {
    let (meta, prompt) = split_frontmatter(text);
    AgentDef {
        name: name.to_string(),
        description: meta.get("description").cloned().unwrap_or_default(),
        prompt,
        tools: list(&meta, "tools").unwrap_or_default(),
        skills: list(&meta, "skills").unwrap_or_default(),
        finish: list(&meta, "finish")
            .unwrap_or_else(|| ["done", "handoff", "blocked"].map(String::from).into()),
        model: meta.get("model").filter(|m| !m.is_empty()).cloned(),
    }
}

/// The built-in agents, overridden or extended by `.genji/agents/<name>.md`.
/// Reserved or malformed definitions are skipped with a warning.
pub fn load_agents(workspace: &Path) -> BTreeMap<String, AgentDef> {
    let mut agents: BTreeMap<String, AgentDef> = BUILTIN_AGENTS
        .iter()
        .map(|(n, t)| ((*n).to_string(), parse_agent(n, t)))
        .collect();
    let files = std::fs::read_dir(dot(workspace, "agents"))
        .into_iter()
        .flatten()
        .flatten();
    for path in files
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "md"))
    {
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        if RESERVED.contains(&name.as_str()) || !valid_slug(&name) {
            eprintln!(
                "[agents] ignoring {}: `{name}` is not a usable agent name",
                path.display()
            );
            continue;
        }
        let def = parse_agent(&name, &std::fs::read_to_string(&path).unwrap_or_default());
        match crate::tools::check(&def) {
            Ok(()) => agents.insert(name, def),
            Err(e) => {
                eprintln!("[agents] ignoring {}: {e}", path.display());
                continue;
            }
        };
    }
    agents
}

/// Skill names with their descriptions: workspace files first, then built-ins.
pub fn skill_list(workspace: &Path) -> BTreeMap<String, String> {
    let mut skills: BTreeMap<String, String> = BUILTIN_SKILLS
        .iter()
        .map(|(n, t)| {
            (
                (*n).to_string(),
                split_frontmatter(t)
                    .0
                    .remove("description")
                    .unwrap_or_default(),
            )
        })
        .collect();
    let files = std::fs::read_dir(dot(workspace, "skills"))
        .into_iter()
        .flatten()
        .flatten();
    for path in files
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "md"))
    {
        if let Some(name) = path.file_stem().and_then(|s| s.to_str()) {
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            let description = split_frontmatter(&text)
                .0
                .remove("description")
                .unwrap_or_default();
            skills.insert(name.to_string(), description);
        }
    }
    skills
}

/// The skill's text as it appears in a prompt or tool result.
pub fn render_skill(workspace: &Path, name: &str) -> Result<String> {
    let text = valid_slug(name)
        .then(|| std::fs::read_to_string(dot(workspace, "skills").join(format!("{name}.md"))).ok())
        .flatten()
        .or_else(|| {
            BUILTIN_SKILLS
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, t)| (*t).to_string())
        });
    let Some(text) = text else {
        let names: Vec<_> = skill_list(workspace).into_keys().collect();
        anyhow::bail!("skill `{name}` not found. available: {}", names.join(", "));
    };
    let (meta, body) = split_frontmatter(&text);
    let description = meta.get("description").map_or("", String::as_str);
    Ok(format!("# Skill: {name}\n{description}\n\n{body}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::util::temp_dir;

    #[test]
    fn defaults_ship_the_four_agents_and_the_formal_skill() {
        let ws = temp_dir("agents-default");
        let agents = load_agents(&ws);
        assert_eq!(
            agents.keys().map(String::as_str).collect::<Vec<_>>(),
            ["build", "explore", "plan", "retro"]
        );
        assert_eq!(agents["plan"].finish, ["done", "handoff", "blocked"]);
        assert_eq!(agents["build"].finish, ["handoff", "blocked"]);
        assert!(skill_list(&ws).contains_key("formal"));
        assert!(
            render_skill(&ws, "formal")
                .unwrap()
                .starts_with("# Skill: formal")
        );
    }

    #[test]
    fn workspace_agents_override_and_reserved_names_are_skipped() {
        let ws = temp_dir("agents-override");
        let dir = dot(&ws, "agents");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("build.md"),
            "---\ntools: read, ls\nskills: formal\n---\ncustom",
        )
        .unwrap();
        std::fs::write(dir.join("list.md"), "---\ntools: read\n---\nx").unwrap();
        std::fs::write(dir.join("bad.md"), "---\ntools: nope\n---\nx").unwrap();
        std::fs::write(
            dir.join("review.md"),
            "---\ndescription: reviews\ntools: read\n---\ny",
        )
        .unwrap();
        let agents = load_agents(&ws);
        assert_eq!(agents["build"].prompt, "custom");
        assert_eq!(agents["build"].tools, ["read", "ls"]);
        assert_eq!(agents["build"].skills, ["formal"]);
        assert_eq!(agents["review"].description, "reviews");
        assert!(!agents.contains_key("list") && !agents.contains_key("bad"));
    }

    #[test]
    fn workspace_skill_shadows_builtin() {
        let ws = temp_dir("skills");
        let dir = dot(&ws, "skills");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("formal.md"), "---\ndescription: mine\n---\nbody\n").unwrap();
        assert_eq!(skill_list(&ws)["formal"], "mine");
        assert!(render_skill(&ws, "formal").unwrap().contains("body"));
        assert!(
            render_skill(&ws, "../x")
                .unwrap_err()
                .to_string()
                .contains("available")
        );
    }
}
