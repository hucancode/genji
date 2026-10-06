use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::storage::util::{split_frontmatter, tmp_file, valid_slug, write_file};

/// Subcommand names an agent may not take.
pub const RESERVED: [&str; 2] = ["init", "help"];

/// Statuses `finish` accepts when an agent's `finish:` line does not narrow them.
pub const DEFAULT_FINISH: [&str; 3] = ["done", "handoff", "blocked"];

/// The agent definitions `genji init` writes, with their review prompts; they are not read at run time.
const DEFAULT_AGENTS: [(&str, &str, Option<&str>); 4] = [
    (
        "plan",
        include_str!("agents/plan.md"),
        Some(include_str!("agents/review/plan.md")),
    ),
    (
        "build",
        include_str!("agents/build.md"),
        Some(include_str!("agents/review/build.md")),
    ),
    ("explore", include_str!("agents/explore.md"), None),
    ("retro", include_str!("agents/retro.md"), None),
];

/// `<workspace>/.genji/<name>`.
pub fn dot(workspace: &Path, name: &str) -> PathBuf {
    workspace.join(".genji").join(name)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Provider {
    pub base_url: String,
    pub api_key: String,
    /// Env var consulted when `api_key` is empty.
    pub api_key_env: String,
    /// "bearer" (Authorization) or "api-key" (Azure). The anthropic api always sends `x-api-key`.
    pub auth: String,
    pub model: String,
    /// Wire format: "chat" (OpenAI chat-completions) or "anthropic" (Messages API).
    pub api: String,
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
            api: "chat".into(),
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
    /// Project cap on the context size in tokens (0 = none); the effective window is the
    /// smallest of this, the provider's `context_window` and what the server reports.
    pub preferred_context_size: i64,
    pub compact_keep_recent: usize,
    /// A work pass is reviewed only when its largest prompt reached this fraction of the
    /// effective context window (0 = always review).
    pub review_threshold: f64,
    /// Messages kept verbatim by the periodic prune of old tool output and payloads.
    pub prune_keep_recent: usize,
    pub tool_result_max_bytes: usize,
    pub max_tool_iterations: usize,
    pub llm_max_retries: u32,
    pub bash_timeout_secs: u64,
    pub spawn_timeout_secs: u64,
    /// How long `ask` waits for a human answer before using the recommended option.
    pub ask_timeout_secs: u64,
    pub max_subagent_depth: u32,
    /// Max tokens (prompt + completion) per run; overrides the provider's when above 0.
    pub token_limit: i64,
    /// Where session files live; `--sessions-dir`, defaulting to `.genji/sessions`.
    pub sessions_dir: Option<PathBuf>,
    /// Where agent definitions live; `--agents-dir`, defaulting to `.genji/agents`.
    pub agents_dir: Option<PathBuf>,
    /// Where skills live, defaulting to `.agents/skills`.
    pub skills_dir: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            provider: "local".into(),
            providers: BTreeMap::from([("local".into(), Provider::default())]),
            time_limit_secs: 1800,
            preferred_context_size: 0,
            compact_keep_recent: 6,
            review_threshold: 0.4,
            prune_keep_recent: 24,
            tool_result_max_bytes: 24_000,
            max_tool_iterations: 200,
            llm_max_retries: 6,
            bash_timeout_secs: 120,
            spawn_timeout_secs: 900,
            ask_timeout_secs: 600,
            max_subagent_depth: 2,
            token_limit: 0,
            sessions_dir: None,
            agents_dir: None,
            skills_dir: None,
        }
    }
}

impl Config {
    /// The config given inline by `--config-json`, nothing read or written on disk.
    pub fn from_json(text: &str) -> Result<Self> {
        serde_json::from_str(text).context("parsing --config-json")
    }

    /// Read `path`, or `.genji/config.json` when `path` is None. A missing default file
    /// yields the built-in defaults; a missing explicit file is an error.
    pub fn load(workspace: &Path, path: Option<&Path>) -> Result<Self> {
        let default = dot(workspace, "config.json");
        let path = path.unwrap_or(&default);
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if path == default && e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Config::default());
            }
            Err(e) => {
                return Err(e).with_context(|| format!("reading config {}", path.display()));
            }
        };
        serde_json::from_str(&text).with_context(|| format!("parsing config {}", path.display()))
    }

    /// Like [`Config::load`], but writes the defaults first when the default file is missing.
    pub fn load_or_create(workspace: &Path, path: Option<&Path>) -> Result<Self> {
        let default = dot(workspace, "config.json");
        let path = path.unwrap_or(&default);
        if path == default && !path.exists() {
            let cfg = Config::default();
            write_file(path, format!("{}\n", serde_json::to_string_pretty(&cfg)?))?;
            return Ok(cfg);
        }
        Self::load(workspace, Some(path))
    }

    pub fn sessions(&self, workspace: &Path) -> PathBuf {
        self.sessions_dir
            .clone()
            .unwrap_or_else(|| dot(workspace, "sessions"))
    }

    /// Where agent definitions live: `agents_dir`, else `<workspace>/.genji/agents`.
    pub fn agents(&self, workspace: &Path) -> PathBuf {
        self.agents_dir
            .clone()
            .unwrap_or_else(|| agents_dir(workspace))
    }

    /// Where skills live: `skills_dir`, else `<workspace>/.agents/skills`.
    pub fn skills(&self, workspace: &Path) -> PathBuf {
        self.skills_dir
            .clone()
            .unwrap_or_else(|| workspace.join(".agents").join("skills"))
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
    /// Workspace files (or `dir/` listings) put in front of a fresh instance's task.
    pub context: Vec<String>,
    /// Statuses `finish` may use: done | handoff | blocked.
    pub finish: Vec<String>,
    /// Agents `spawn` may start; `None` allows every agent.
    pub spawns: Option<Vec<String>>,
    pub model: Option<String>,
    /// Reached only through `hand_off`/`spawn`; front ends do not offer it for a new session.
    pub internal: bool,
    /// With `review: true`, the prompt of the review pass, from `<agents dir>/review/<name>.md`.
    pub review: Option<String>,
}

/// Name suffix of an agent's review pass.
pub const REVIEW_SUFFIX: &str = ":review";

impl AgentDef {
    /// The review pass of a `review: true` agent: the review prompt, the work tools without
    /// `hand_off`/`finish`, and `verdict` to end it.
    pub fn reviewer(&self) -> Option<AgentDef> {
        let prompt = self.review.clone()?;
        let mut tools: Vec<String> = self
            .tools
            .iter()
            .filter(|t| !matches!(t.as_str(), "hand_off" | "finish"))
            .cloned()
            .collect();
        tools.push("verdict".into());
        Some(AgentDef {
            name: format!("{}{REVIEW_SUFFIX}", self.name),
            prompt,
            tools,
            finish: Vec::new(),
            internal: true,
            review: None,
            ..self.clone()
        })
    }

    /// Whether `spawn` may start `agent`.
    pub fn may_spawn(&self, agent: &str) -> bool {
        self.spawns
            .as_ref()
            .is_none_or(|l| l.iter().any(|n| n == agent))
    }

    /// The agent whose work this pass belongs to: itself, or for a review pass its worker.
    pub fn worker(&self) -> &str {
        self.name.strip_suffix(REVIEW_SUFFIX).unwrap_or(&self.name)
    }

    /// The summary `genji help agent --json` prints.
    pub fn info(&self) -> Value {
        json!({
            "name": self.name, "description": self.description, "tools": self.tools,
            "skills": self.skills, "finish": self.finish, "model": self.model,
            "internal": self.internal, "review": self.review.is_some()
        })
    }
}

fn list(meta: &BTreeMap<String, String>, key: &str) -> Option<Vec<String>> {
    meta.get(key).map(|v| {
        v.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    })
}

/// Parses `<name>.md`; the flag is whether its frontmatter asks for a review pass.
fn parse_agent(name: &str, text: &str) -> (AgentDef, bool) {
    let (meta, prompt) = split_frontmatter(text);
    let review = meta.get("review").is_some_and(|v| v == "true");
    let def = AgentDef {
        name: name.to_string(),
        description: meta.get("description").cloned().unwrap_or_default(),
        prompt,
        tools: list(&meta, "tools").unwrap_or_default(),
        skills: list(&meta, "skills").unwrap_or_default(),
        context: list(&meta, "context").unwrap_or_default(),
        finish: list(&meta, "finish").unwrap_or_else(|| DEFAULT_FINISH.map(String::from).into()),
        spawns: list(&meta, "spawns"),
        model: meta.get("model").filter(|m| !m.is_empty()).cloned(),
        internal: meta.get("internal").is_some_and(|v| v == "true"),
        review: None,
    };
    (def, review)
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
/// unless `force`; `only` limits the agents considered (all defaults when empty).
pub fn init_agents(workspace: &Path, force: bool, only: &[String]) -> Result<InitReport> {
    if let Some(n) = only
        .iter()
        .find(|n| !DEFAULT_AGENTS.iter().any(|(d, ..)| d == n))
    {
        anyhow::bail!("no default agent named `{n}`");
    }
    // `(file stem relative to the agents dir, text)`: each agent, then its review prompt.
    let chosen: Vec<(String, &str)> = DEFAULT_AGENTS
        .iter()
        .filter(|(n, ..)| only.is_empty() || only.iter().any(|o| o == n))
        .flat_map(|(n, text, review)| {
            std::iter::once((n.to_string(), *text))
                .chain(review.map(|r| (format!("review/{n}"), r)))
        })
        .collect();
    let dir = agents_dir(workspace);
    std::fs::create_dir_all(&dir)?;
    let mut report = InitReport::default();
    for (n, text) in chosen {
        let path = dir.join(format!("{n}.md"));
        if path.exists() && !force {
            report.skipped.push(n);
            continue;
        }
        // Write to a scratch file and rename, so a crash never leaves a half-written definition.
        let tmp = tmp_file(&dir, &n, "tmp");
        write_file(&tmp, text)?;
        std::fs::rename(&tmp, &path)?;
        report.written.push(n);
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
#[cfg(test)]
pub fn load_agents(workspace: &Path) -> BTreeMap<String, AgentDef> {
    load_agents_from(&agents_dir(workspace))
}

/// The agents defined by `<dir>/<name>.md`, and nothing else.
pub fn load_agents_from(dir: &Path) -> BTreeMap<String, AgentDef> {
    let mut agents: BTreeMap<String, AgentDef> = BTreeMap::new();
    let files = std::fs::read_dir(dir).into_iter().flatten().flatten();
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
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let (mut def, review) = parse_agent(&name, &text);
        if review {
            let file = dir.join("review").join(format!("{name}.md"));
            match std::fs::read_to_string(&file) {
                Ok(prompt) => def.review = Some(prompt),
                Err(_) => {
                    eprintln!(
                        "[agents] ignoring {}: `review: true` needs {}",
                        path.display(),
                        file.display()
                    );
                    continue;
                }
            }
        }
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
    /// Workspace files the skill maintains (`metadata: context:`), put in front of a fresh
    /// instance's task for agents that force the skill.
    pub context: Vec<String>,
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
        context: list(&meta, "context").unwrap_or_default(),
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

/// Skills from `dir` (found recursively; the first of a name wins).
/// Malformed skills are skipped with a warning.
pub fn load_skills(dir: &Path) -> BTreeMap<String, Skill> {
    let mut skills = BTreeMap::new();
    let mut files = Vec::new();
    find_skill_files(dir, &mut files);
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
        assert_eq!(
            r.written,
            [
                "plan",
                "review/plan",
                "build",
                "review/build",
                "explore",
                "retro"
            ]
        );
        assert!(r.skipped.is_empty());
        let agents = load_agents(&ws);
        assert_eq!(
            agents.keys().map(String::as_str).collect::<Vec<_>>(),
            ["build", "explore", "plan", "retro"]
        );
        assert_eq!(agents["plan"].finish, ["done", "blocked"]);
        assert_eq!(agents["build"].finish, ["done", "blocked"]);
        assert!(agents["plan"].review.is_some() && agents["build"].review.is_some());
        assert!(agents["explore"].review.is_none() && agents["retro"].review.is_none());
        assert!(agents["build"].context.is_empty());
        assert!(load_skills(&Config::default().skills(&ws)).is_empty());
        assert!(!Config::default().skills(&ws).exists());
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
        assert_eq!(
            r.skipped,
            ["plan", "review/plan", "build", "review/build", "explore"]
        );
        assert_eq!(load_agents(&ws)["build"].prompt, "mine");
        let r = init_agents(&ws, true, &["build".to_string()]).unwrap();
        assert_eq!(r.written, ["build", "review/build"]);
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
        std::fs::write(dir.join("help.md"), "---\ntools: read\n---\nx").unwrap();
        std::fs::write(dir.join("bad.md"), "---\ntools: nope\n---\nx").unwrap();
        std::fs::write(
            dir.join("check.md"),
            "---\ndescription: checks\ntools: read\nreview: true\n---\ny",
        )
        .unwrap();
        std::fs::write(
            dir.join("lone.md"),
            "---\ntools: read\nreview: true\n---\nz",
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("review")).unwrap();
        std::fs::write(dir.join("review/check.md"), "judge").unwrap();
        let agents = load_agents(&ws);
        assert_eq!(agents["build"].prompt, "custom");
        assert_eq!(agents["build"].tools, ["read", "ls"]);
        assert_eq!(agents["build"].skills, ["formal"]);
        assert!(agents["build"].review.is_none());
        assert_eq!(agents["check"].description, "checks");
        assert_eq!(agents["check"].review.as_deref(), Some("judge"));
        assert!(!agents.contains_key("help") && !agents.contains_key("bad"));
        assert!(!agents.contains_key("lone") && !agents.contains_key("review"));
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
                (
                    "explore",
                    "---\ntools: read\nfinish: blocked\n---\nreplaced",
                ),
                ("retro", "---\ntools: read\nfinish: blocked\n---\nreplaced"),
                (
                    "alpha",
                    "---\ntools: read, finish\nfinish: handoff, done\n---\na",
                ),
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
        let path = Config::default().skills(ws).join(rel).join("SKILL.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn workspace_skills_are_found_recursively() {
        let ws = temp_dir("skills");
        skill(
            &ws,
            "formal",
            "---\nname: formal\ndescription: mine\n---\nbody\n",
        );
        skill(
            &ws,
            "group/pdf-tools",
            "---\nname: pdf-tools\ndescription: pdfs\nmetadata:\n  context: docs/a.md, docs/adr/\n---\nx\n",
        );
        skill(
            &ws,
            "hidden",
            "---\ndescription: manual only\ndisable-model-invocation: true\n---\nx\n",
        );
        skill(&ws, "Bad_Name", "---\ndescription: d\n---\nx\n");
        skill(&ws, "nodesc", "---\nname: nodesc\n---\nx\n");
        let skills = load_skills(&Config::default().skills(&ws));
        assert_eq!(
            skills.keys().map(String::as_str).collect::<Vec<_>>(),
            ["formal", "hidden", "pdf-tools"]
        );
        assert_eq!(skills["formal"].description, "mine");
        assert!(
            skills["formal"]
                .path
                .ends_with(".agents/skills/formal/SKILL.md")
        );
        assert!(skills["hidden"].disable_model_invocation);
        assert_eq!(skills["pdf-tools"].context, ["docs/a.md", "docs/adr/"]);
        assert!(skills["formal"].context.is_empty());
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
