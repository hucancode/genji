use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::storage::util::{split_frontmatter, tmp_file, valid_slug, write_file};

/// Subcommand names an agent may not take.
pub const RESERVED: [&str; 7] = ["init", "list", "stop", "instruct", "inspect", "reset", "help"];

/// The agent definitions `genji init` writes; they are not read at run time.
const DEFAULT_AGENTS: [(&str, &str); 4] = [
    ("plan", include_str!("agents/plan.md")),
    ("build", include_str!("agents/build.md")),
    ("explore", include_str!("agents/explore.md")),
    ("retro", include_str!("agents/retro.md")),
];

/// `<workspace>/.genji/<name>`.
pub fn dot(workspace: &Path, name: &str) -> PathBuf {
    workspace.join(".genji").join(name)
}

pub fn skills_dir(workspace: &Path) -> PathBuf {
    workspace.join(".agents").join("skills")
}

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
    /// Max tokens (prompt + completion) per run; overrides the provider's when above 0.
    pub token_limit: i64,
    /// Where session files live; `--sessions-dir`, defaulting to `.genji/sessions`.
    #[serde(skip)]
    pub sessions_dir: Option<PathBuf>,
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
            token_limit: 0,
            sessions_dir: None,
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

    pub fn sessions(&self, workspace: &Path) -> PathBuf {
        self.sessions_dir
            .clone()
            .unwrap_or_else(|| dot(workspace, "sessions"))
    }

    /// The per-run token budget: the top-level `token_limit` when set, else the provider's.
    pub fn token_limit(&self, provider: &Provider) -> i64 {
        if self.token_limit > 0 {
            self.token_limit
        } else {
            provider.token_limit
        }
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

pub fn agents_dir(workspace: &Path) -> PathBuf {
    dot(workspace, "agents")
}

/// Result of [`init_agents`]: the files written and the ones left as they were.
#[derive(Debug, Default, PartialEq)]
pub struct InitReport {
    pub written: Vec<String>,
    pub skipped: Vec<String>,
}

/// Writes the default agent definitions to `.genji/agents/<name>.md`. Existing files are kept
/// unless `force`; `only` limits the agents considered (all defaults when empty). When the
/// directory does not exist yet, it appears complete or not at all.
pub fn init_agents(workspace: &Path, force: bool, only: &[String]) -> Result<InitReport> {
    if let Some(n) = only.iter().find(|n| !DEFAULT_AGENTS.iter().any(|(d, _)| d == n)) {
        anyhow::bail!("no default agent named `{n}`");
    }
    let chosen: Vec<&(&str, &str)> = DEFAULT_AGENTS
        .iter()
        .filter(|(n, _)| only.is_empty() || only.iter().any(|o| o == n))
        .collect();
    let dir = agents_dir(workspace);
    let mut report = InitReport::default();
    if !dir.exists() {
        let parent = dir.parent().unwrap_or(workspace);
        std::fs::create_dir_all(parent)?;
        let tmp = tmp_file(parent, "agents", "d");
        std::fs::create_dir_all(&tmp)?;
        for (n, text) in &chosen {
            write_file(&tmp.join(format!("{n}.md")), text)?;
        }
        if std::fs::rename(&tmp, &dir).is_ok() {
            report.written = chosen.iter().map(|(n, _)| (*n).to_string()).collect();
            return Ok(report);
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }
    std::fs::create_dir_all(&dir)?;
    for (n, text) in chosen {
        let path = dir.join(format!("{n}.md"));
        let tmp = tmp_file(&dir, n, "tmp");
        write_file(&tmp, text)?;
        let placed = if force {
            std::fs::rename(&tmp, &path).is_ok()
        } else {
            let linked = std::fs::hard_link(&tmp, &path).is_ok();
            let _ = std::fs::remove_file(&tmp);
            linked
        };
        let list = if placed { &mut report.written } else { &mut report.skipped };
        list.push((*n).to_string());
    }
    Ok(report)
}

/// Runs `init` when the workspace has no agents directory yet.
pub fn ensure_agents(workspace: &Path) -> Result<()> {
    if !agents_dir(workspace).exists() {
        init_agents(workspace, false, &[])?;
    }
    Ok(())
}

/// The agents defined by `.genji/agents/<name>.md`, and nothing else. Reserved or malformed
/// definitions are skipped with a warning.
pub fn load_agents(workspace: &Path) -> BTreeMap<String, AgentDef> {
    let mut agents: BTreeMap<String, AgentDef> = BTreeMap::new();
    let files = std::fs::read_dir(agents_dir(workspace))
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

/// A skill in the Agent Skills format: a directory holding `SKILL.md`.
#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// Absolute path of the skill's `SKILL.md`.
    pub path: PathBuf,
    pub body: String,
    /// Left out of the skills list in the system prompt; still usable through `skills:`.
    pub disable_model_invocation: bool,
}

/// A skill name: lowercase letters, digits and single hyphens, at most 64 chars.
pub fn valid_skill_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--")
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn parse_skill(path: PathBuf, text: &str) -> Result<Skill, String> {
    let (meta, body) = split_frontmatter(text);
    let dir_name = path
        .parent()
        .and_then(Path::file_name)
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    let name = meta
        .get("name")
        .filter(|n| !n.is_empty())
        .map_or(dir_name, String::as_str)
        .to_string();
    if !valid_skill_name(&name) {
        return Err(format!("`{name}` is not a valid skill name"));
    }
    let description = meta.get("description").cloned().unwrap_or_default();
    if description.is_empty() {
        return Err("missing `description`".into());
    }
    Ok(Skill {
        name,
        description,
        path,
        body,
        disable_model_invocation: meta
            .get("disable-model-invocation")
            .is_some_and(|v| v == "true"),
    })
}

/// Every `SKILL.md` below `dir`, in path order.
fn find_skill_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            find_skill_files(&p, out);
        } else if p.file_name().is_some_and(|n| n == "SKILL.md") {
            out.push(p);
        }
    }
}

/// Skills from `.agents/skills/` (found recursively; the first of a name wins).
/// Malformed skills are skipped with a warning.
pub fn load_skills(workspace: &Path) -> BTreeMap<String, Skill> {
    let mut skills = BTreeMap::new();
    let mut files = Vec::new();
    find_skill_files(&skills_dir(workspace), &mut files);
    for path in files {
        let path = std::fs::canonicalize(&path).unwrap_or(path);
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        match parse_skill(path.clone(), &text) {
            Ok(skill) if skills.contains_key(&skill.name) => eprintln!(
                "[skills] ignoring {}: skill `{}` is already defined",
                path.display(),
                skill.name
            ),
            Ok(skill) => {
                skills.insert(skill.name.clone(), skill);
            }
            Err(e) => eprintln!("[skills] ignoring {}: {e}", path.display()),
        }
    }
    skills
}

/// A skill's full text as inlined into a system prompt.
pub fn render_skill(skill: &Skill) -> String {
    let dir = skill.path.parent().unwrap_or(Path::new("."));
    format!(
        "# Skill: {}\n{}\nSkill directory: {} (paths in this skill are relative to it)\n\n{}",
        skill.name,
        skill.description,
        dir.display(),
        skill.body
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::storage::util::temp_dir;

    #[test]
    fn init_writes_the_defaults_that_become_the_only_agents() {
        let ws = temp_dir("agents-default");
        assert!(load_agents(&ws).is_empty());
        let r = init_agents(&ws, false, &[]).unwrap();
        assert_eq!(r.written, ["plan", "build", "explore", "retro"]);
        assert!(r.skipped.is_empty());
        let agents = load_agents(&ws);
        assert_eq!(
            agents.keys().map(String::as_str).collect::<Vec<_>>(),
            ["build", "explore", "plan", "retro"]
        );
        assert_eq!(agents["plan"].finish, ["done", "handoff", "blocked"]);
        assert_eq!(agents["build"].finish, ["handoff", "blocked"]);
        assert!(load_skills(&ws).is_empty());
        assert!(!skills_dir(&ws).exists());
    }

    #[test]
    fn init_keeps_existing_files_and_force_restores_them() {
        let ws = temp_dir("agents-init");
        init_agents(&ws, false, &[]).unwrap();
        let dir = agents_dir(&ws);
        std::fs::write(dir.join("build.md"), "---\ntools: read\n---\nmine").unwrap();
        std::fs::remove_file(dir.join("retro.md")).unwrap();
        let r = init_agents(&ws, false, &[]).unwrap();
        assert_eq!(r.written, ["retro"]);
        assert_eq!(r.skipped, ["plan", "build", "explore"]);
        assert_eq!(load_agents(&ws)["build"].prompt, "mine");
        let r = init_agents(&ws, true, &["build".to_string()]).unwrap();
        assert_eq!(r.written, ["build"]);
        assert!(r.skipped.is_empty());
        assert_ne!(load_agents(&ws)["build"].prompt, "mine");
        assert!(init_agents(&ws, true, &["nope".to_string()]).is_err());
    }

    #[test]
    fn an_existing_agents_dir_is_used_as_is() {
        let ws = temp_dir("agents-as-is");
        init_agents(&ws, false, &[]).unwrap();
        std::fs::remove_file(agents_dir(&ws).join("plan.md")).unwrap();
        ensure_agents(&ws).unwrap();
        assert!(!load_agents(&ws).contains_key("plan"));
        let empty = temp_dir("agents-empty");
        std::fs::create_dir_all(agents_dir(&empty)).unwrap();
        ensure_agents(&empty).unwrap();
        assert!(load_agents(&empty).is_empty());
    }

    #[test]
    fn ensure_agents_initialises_a_fresh_workspace() {
        let ws = temp_dir("agents-ensure");
        ensure_agents(&ws).unwrap();
        assert_eq!(load_agents(&ws).len(), 4);
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

    /// Writes workspace agent files (`(name, file text)`).
    pub(crate) fn write_agents(ws: &Path, defs: &[(&str, &str)]) {
        let dir = dot(ws, "agents");
        std::fs::create_dir_all(&dir).unwrap();
        for (name, text) in defs {
            std::fs::write(dir.join(format!("{name}.md")), text).unwrap();
        }
    }

    #[test]
    fn a_workspace_may_replace_every_default_and_add_its_own() {
        let ws = temp_dir("agents-replaced");
        write_agents(
            &ws,
            &[
                ("plan", "---\ntools: read\nfinish: blocked\n---\nreplaced"),
                ("build", "---\ntools: read\nfinish: blocked\n---\nreplaced"),
                ("explore", "---\ntools: read\nfinish: blocked\n---\nreplaced"),
                ("retro", "---\ntools: read\nfinish: blocked\n---\nreplaced"),
                ("alpha", "---\ntools: read, finish\nfinish: handoff, done\n---\na"),
            ],
        );
        let agents = load_agents(&ws);
        for n in ["plan", "build", "explore", "retro"] {
            assert_eq!(agents[n].prompt, "replaced");
            assert_eq!(agents[n].finish, ["blocked"]);
        }
        assert_eq!(agents["alpha"].finish, ["handoff", "done"]);
    }

    fn skill(ws: &Path, rel: &str, text: &str) {
        let path = skills_dir(ws).join(rel).join("SKILL.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn workspace_skills_are_found_recursively() {
        let ws = temp_dir("skills");
        skill(&ws, "formal", "---\nname: formal\ndescription: mine\n---\nbody\n");
        skill(&ws, "group/pdf-tools", "---\nname: pdf-tools\ndescription: pdfs\n---\nx\n");
        skill(&ws, "hidden", "---\ndescription: manual only\ndisable-model-invocation: true\n---\nx\n");
        skill(&ws, "Bad_Name", "---\ndescription: d\n---\nx\n");
        skill(&ws, "nodesc", "---\nname: nodesc\n---\nx\n");
        let skills = load_skills(&ws);
        assert_eq!(
            skills.keys().map(String::as_str).collect::<Vec<_>>(),
            ["formal", "hidden", "pdf-tools"]
        );
        assert_eq!(skills["formal"].description, "mine");
        assert!(skills["formal"].path.ends_with(".agents/skills/formal/SKILL.md"));
        assert!(skills["hidden"].disable_model_invocation);
        assert!(render_skill(&skills["formal"]).contains("body"));
    }

    #[test]
    fn skill_names_follow_the_spec() {
        assert!(valid_skill_name("pdf-tools") && valid_skill_name("a1"));
        for bad in ["", "-a", "a-", "a--b", "A", "a_b", &"a".repeat(65)] {
            assert!(!valid_skill_name(bad), "{bad}");
        }
    }

    #[test]
    fn top_level_token_limit_overrides_the_provider() {
        let mut cfg = Config::default();
        let p = Provider::default();
        assert_eq!(cfg.token_limit(&p), p.token_limit);
        cfg.token_limit = 10;
        assert_eq!(cfg.token_limit(&p), 10);
    }
}
