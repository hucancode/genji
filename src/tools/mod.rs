use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::agent::Agent;
use crate::config::Config;
use crate::modes::Mode;
use std::path::Path;

pub mod basic;
pub mod plans;
pub mod requirements;
pub mod retro;
pub mod skills;
pub mod spawn;
pub mod tickets;

#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub parameters: Value,
}

impl ToolSpec {
    pub fn to_json(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": self.name,
                "description": self.description,
                "parameters": self.parameters,
            }
        })
    }
}

/// A tool is defined exactly once: its name, description, JSON schema, the
/// modes it is available in, and its handler. The model-facing spec and the
/// dispatch table are both derived from this, so they cannot drift apart.
struct Tool {
    name: &'static str,
    description: &'static str,
    parameters: Value,
    modes: &'static [Mode],
    /// When true the tool is only exposed while the OCD flag is enabled. This
    /// is how the requirements/tickets system stays out of the default agent.
    requires_ocd: bool,
    /// When true the tool is only exposed while at least one skill exists. This
    /// keeps `skill_load` out of the schema when there is nothing to load.
    requires_skills: bool,
    handler: fn(&mut Agent, &Value) -> Result<String>,
}

impl Tool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name,
            description: self.description,
            parameters: self.parameters.clone(),
        }
    }
}

const ALL_MODES: &[Mode] = &[Mode::Plan, Mode::Build, Mode::Explore, Mode::Retro];
const PLAN: &[Mode] = &[Mode::Plan];
const PLAN_BUILD: &[Mode] = &[Mode::Plan, Mode::Build];
const PLAN_BUILD_EXPLORE: &[Mode] = &[Mode::Plan, Mode::Build, Mode::Explore];
const RETRO: &[Mode] = &[Mode::Retro];

fn tool(
    name: &'static str,
    modes: &'static [Mode],
    description: &'static str,
    parameters: Value,
    handler: fn(&mut Agent, &Value) -> Result<String>,
) -> Tool {
    Tool {
        name,
        description,
        parameters,
        modes,
        requires_ocd: false,
        requires_skills: false,
        handler,
    }
}

/// Like [`tool`], but only exposed while the OCD flag is enabled.
fn ocd_tool(
    name: &'static str,
    modes: &'static [Mode],
    description: &'static str,
    parameters: Value,
    handler: fn(&mut Agent, &Value) -> Result<String>,
) -> Tool {
    Tool {
        name,
        description,
        parameters,
        modes,
        requires_ocd: true,
        requires_skills: false,
        handler,
    }
}

/// Like [`tool`], but only exposed while at least one skill is available.
fn skill_tool(
    name: &'static str,
    modes: &'static [Mode],
    description: &'static str,
    parameters: Value,
    handler: fn(&mut Agent, &Value) -> Result<String>,
) -> Tool {
    Tool {
        name,
        description,
        parameters,
        modes,
        requires_ocd: false,
        requires_skills: true,
        handler,
    }
}

/// The full registry. Built once, then shared by `specs_for` and `dispatch`.
fn registry() -> &'static [Tool] {
    static TOOLS: OnceLock<Vec<Tool>> = OnceLock::new();
    TOOLS.get_or_init(|| {
        vec![
            tool("read", ALL_MODES, "Read a text file with line numbers. offset is 1-indexed.", json!({
                "type":"object",
                "properties":{
                    "path":{"type":"string","description":"File path"},
                    "offset":{"type":"integer","description":"First line (1-indexed)"},
                    "limit":{"type":"integer","description":"Max lines to read (default 2000)"}
                },
                "required":["path"]
            }), basic::read),
            tool("write", ALL_MODES, "Create or overwrite a file, creating parent directories.", json!({
                "type":"object",
                "properties":{"path":{"type":"string"},"content":{"type":"string"}},
                "required":["path","content"]
            }), basic::write),
            tool("edit", ALL_MODES, "Apply precise text replacements to a file. Each oldText must match uniquely.", json!({
                "type":"object",
                "properties":{
                    "path":{"type":"string"},
                    "edits":{"type":"array","items":{"type":"object","properties":{
                        "oldText":{"type":"string"},"newText":{"type":"string"}
                    },"required":["oldText","newText"]}},
                    "oldText":{"type":"string","description":"Single-edit shorthand"},
                    "newText":{"type":"string","description":"Single-edit shorthand"},
                    "replace_all":{"type":"boolean","description":"Allow replacing a non-unique oldText (single-edit form)"}
                },
                "required":["path"]
            }), basic::edit),
            tool("ls", ALL_MODES, "List files/directories respecting .gitignore.", json!({
                "type":"object",
                "properties":{
                    "path":{"type":"string","description":"Directory (default .)"},
                    "max_depth":{"type":"integer","description":"Recursion depth; Default = 0 = lists only immediate children"},
                    "show_hidden":{"type":"boolean","description":"default false"}
                }
            }), basic::ls),
            tool("bash", ALL_MODES, "Run a shell command via bash -c in the workspace. Returns exit code, stdout, stderr.", json!({
                "type":"object",
                "properties":{
                    "command":{"type":"string"},
                    "cwd":{"type":"string","description":"Working directory (default workspace)"},
                    "timeout_secs":{"type":"integer"}
                },
                "required":["command"]
            }), basic::bash),
            // ---- plans ----
            tool("plan_write", PLAN, "Persist an implementation plan as markdown under the plans directory (default .genji/plans/). Reuse the same title to refine an existing plan.", json!({
                "type":"object",
                "properties":{
                    "title":{"type":"string","description":"Short plan title; drives the file name and default heading"},
                    "content":{"type":"string","description":"Plan body in markdown"},
                    "path":{"type":"string","description":"Optional explicit path (default: plans_dir/<title-slug>.md)"}
                },
                "required":["title","content"]
            }), plans::write),
            // ---- tickets (OCD only) ----
            ocd_tool("ticket_create", PLAN, "Create a work ticket.", json!({
                "type":"object",
                "properties":{
                    "title":{"type":"string"},
                    "description":{"type":"string"},
                    "priority":{"type":"integer","description":"1=high, 2=normal, 3=low"},
                    "parent_id":{"type":"integer"},
                    "requirement_id":{"type":"integer","description":"Requirement this ticket addresses"}
                },
                "required":["title"]
            }), tickets::create),
            ocd_tool("ticket_read", PLAN_BUILD, "Read one ticket by id, or list actionable tickets.", json!({
                "type":"object",
                "properties":{
                    "id":{"type":"integer"},
                    "status":{"type":"string","enum":["open","in_progress","resolved","closed"]},
                    "requirement_id":{"type":"integer"}
                }
            }), tickets::read),
            ocd_tool("ticket_claim", PLAN_BUILD, "Claim the next open ticket (highest priority) or a specific ticket, marking it in_progress.", json!({
                "type":"object",
                "properties":{
                    "id":{"type":"integer","description":"Claim this ticket instead of the next one"},
                    "requirement_id":{"type":"integer","description":"Only consider tickets for this requirement"}
                }
            }), tickets::claim),
            ocd_tool("ticket_update", PLAN_BUILD, "Update a ticket's fields and/or status.", json!({
                "type":"object",
                "properties":{
                    "id":{"type":"integer"},
                    "title":{"type":"string"},
                    "description":{"type":"string"},
                    "priority":{"type":"integer","description":"1=high, 2=normal, 3=low"},
                    "parent_id":{"type":"integer"},
                    "requirement_id":{"type":"integer"},
                    "status":{"type":"string","enum":["open","in_progress","resolved","closed"]},
                    "resolution":{"type":"string"}
                },
                "required":["id"]
            }), tickets::update),
            ocd_tool("ticket_close", PLAN_BUILD, "Close a ticket when its work is done and verified, or it is obsolete/duplicate/won't-fix.", json!({
                "type":"object",
                "properties":{"id":{"type":"integer"},"reason":{"type":"string"}},
                "required":["id"]
            }), tickets::close),
            // ---- requirements (OCD only) ----
            ocd_tool("requirement_create", PLAN, "Create a stakeholder or system requirement.", json!({
                "type":"object",
                "properties":{
                    "level":{"type":"string","enum":["stakeholder","system"]},
                    "title":{"type":"string"},
                    "body":{"type":"string"},
                    "parent_id":{"type":"integer","description":"Parent requirement id"}
                },
                "required":["level","title","body"]
            }), requirements::create),
            ocd_tool("requirement_read", PLAN_BUILD, "Read a requirement by id, or list/filter requirements by level and status when id is omitted.", json!({
                "type":"object",
                "properties":{
                    "id":{"type":"integer"},
                    "level":{"type":"string","enum":["stakeholder","system"]},
                    "status":{"type":"string","enum":["active","met","removed"]}
                }
            }), requirements::read),
            ocd_tool("requirement_tree", PLAN_BUILD, "Show the requirement hierarchy with ticket coverage per requirement.", json!({
                "type":"object",
                "properties":{
                    "status":{"type":"string","enum":["active","met","removed"],"description":"Only show requirements with this status"}
                }
            }), requirements::tree),
            ocd_tool("requirement_update", PLAN, "Update a requirement's title/body/status/level/parent.", json!({
                "type":"object",
                "properties":{
                    "id":{"type":"integer"},
                    "title":{"type":"string"},
                    "body":{"type":"string"},
                    "status":{"type":"string","enum":["active","met","removed"]},
                    "level":{"type":"string","enum":["stakeholder","system"]},
                    "parent_id":{"type":"integer"}
                },
                "required":["id"]
            }), requirements::update),
            ocd_tool("requirement_remove", PLAN, "Remove a requirement (soft by default).", json!({
                "type":"object",
                "properties":{"id":{"type":"integer"},"hard":{"type":"boolean"}},
                "required":["id"]
            }), requirements::remove),
            ocd_tool("requirement_ask", PLAN_BUILD, "Ask the user a clarifying question about a requirement. Recorded in the DB.", json!({
                "type":"object",
                "properties":{
                    "question":{"type":"string"},
                    "requirement_id":{"type":"integer"}
                },
                "required":["question"]
            }), requirements::ask),
            // ---- skills ----
            skill_tool("skill_load", ALL_MODES, "Load a skill's instructions by name.", json!({
                "type":"object",
                "properties":{"name":{"type":"string"}},
                "required":["name"]
            }), skills::load),
            // ---- spawn ----
            tool("spawn", PLAN_BUILD_EXPLORE, "Spawn a subagent in a given mode that only reports back.", json!({
                "type":"object",
                "properties":{
                    "mode":{"type":"string","enum":["plan","build","explore"]},
                    "instructions":{"type":"string","description":"What the subagent should do"},
                    "task":{"type":"string","description":"Optional task label"}
                },
                "required":["mode","instructions"]
            }), spawn::spawn),
            // ---- retro ----
            tool("query_instances", RETRO, "List past agent instances.", json!({
                "type":"object",
                "properties":{
                    "mode":{"type":"string"},
                    "limit":{"type":"integer","description":"Default 20"}
                }
            }), retro::instances),
            tool("query_instance", RETRO, "Get all messages of one instance.", json!({
                "type":"object",
                "properties":{"instance_id":{"type":"string"},"limit":{"type":"integer"}},
                "required":["instance_id"]
            }), retro::instance),
            tool("query_messages", RETRO, "Search recorded messages by text/role/instance.", json!({
                "type":"object",
                "properties":{
                    "instance_id":{"type":"string"},
                    "role":{"type":"string"},
                    "search":{"type":"string"},
                    "limit":{"type":"integer","description":"Default 50"}
                }
            }), retro::messages),
            tool("query_tool_call", RETRO, "Query recorded tool calls (filter by name/errors/instance).", json!({
                "type":"object",
                "properties":{
                    "instance_id":{"type":"string"},
                    "name":{"type":"string"},
                    "errors_only":{"type":"boolean"},
                    "limit":{"type":"integer","description":"Default 50"}
                }
            }), retro::tool_calls),
            tool("query_stats", RETRO, "Aggregate stats: tool usage, error rates, skill loads, token usage.", json!({
                "type":"object","properties":{}
            }), retro::stats),
            tool("list_skills", RETRO, "List all skills with descriptions and use counts.", json!({
                "type":"object","properties":{}
            }), skills::list),
            tool("read_skill", RETRO, "Read a skill, optionally a specific version.", json!({
                "type":"object",
                "properties":{"name":{"type":"string"},"version":{"type":"integer"}},
                "required":["name"]
            }), skills::read),
            tool("write_skill", RETRO, "Create or overwrite a skill (versioned).", json!({
                "type":"object",
                "properties":{
                    "name":{"type":"string"},
                    "description":{"type":"string"},
                    "content":{"type":"string"},
                    "reason":{"type":"string"}
                },
                "required":["name","content"]
            }), skills::write),
            tool("edit_skill", RETRO, "Edit a skill by text replacement (versioned).", json!({
                "type":"object",
                "properties":{
                    "name":{"type":"string"},
                    "oldText":{"type":"string"},
                    "newText":{"type":"string"},
                    "reason":{"type":"string"}
                },
                "required":["name","oldText","newText"]
            }), skills::edit),
            tool("skill_history", RETRO, "List versions of a skill.", json!({
                "type":"object","properties":{"name":{"type":"string"}},"required":["name"]
            }), skills::history),
            tool("skill_rollback", RETRO, "Activate an older version of a skill.", json!({
                "type":"object",
                "properties":{"name":{"type":"string"},"version":{"type":"integer"}},
                "required":["name","version"]
            }), skills::rollback),
            tool("prompt_read", RETRO, "Read the active extended system prompt for a mode. RETRO has no extended prompt.", json!({
                "type":"object","properties":{"mode":{"type":"string","enum":["plan","build","explore"]}},"required":["mode"]
            }), retro::prompt_read),
            tool("prompt_edit", RETRO, "Replace the extended system prompt for a mode (versioned). Core prompt is not editable; RETRO has no extended prompt.", json!({
                "type":"object",
                "properties":{
                    "mode":{"type":"string","enum":["plan","build","explore"]},
                    "content":{"type":"string"},
                    "reason":{"type":"string"}
                },
                "required":["mode","content"]
            }), retro::prompt_edit),
            tool("prompt_history", RETRO, "List versions of a mode's extended prompt. RETRO has no extended prompt.", json!({
                "type":"object","properties":{"mode":{"type":"string","enum":["plan","build","explore"]}},"required":["mode"]
            }), retro::prompt_history),
            tool("prompt_rollback", RETRO, "Activate an older version of a mode's extended prompt. RETRO has no extended prompt.", json!({
                "type":"object",
                "properties":{"mode":{"type":"string","enum":["plan","build","explore"]},"version":{"type":"integer"}},
                "required":["mode","version"]
            }), retro::prompt_rollback),
        ]
    })
}

pub fn specs_for(mode: Mode, ocd: bool, has_skills: bool) -> Vec<ToolSpec> {
    registry()
        .iter()
        .filter(|t| t.modes.contains(&mode))
        .filter(|t| !t.requires_ocd || ocd)
        .filter(|t| !t.requires_skills || has_skills)
        .map(|t| t.spec())
        .collect()
}

/// Execute a tool. Returns (result_text, is_error). Result is bounded here so
/// every caller gets content that fits the context window; oversized results are
/// spilled to `.genji/tmp/*.log` and the returned text points at the file.
pub fn dispatch(agent: &mut Agent, name: &str, args: &Value) -> (String, bool) {
    let res: Result<String> = match registry().iter().find(|t| t.name == name) {
        Some(t) => (t.handler)(agent, args),
        None => Err(anyhow!("unknown or unavailable tool `{name}`")),
    };
    match res {
        Ok(s) => (bounded_result(&agent.cfg, &agent.workspace, name, s), false),
        Err(e) => (
            bounded_result(&agent.cfg, &agent.workspace, name, format!("ERROR: {e:#}")),
            true,
        ),
    }
}

/// Bound a tool result to `tool_result_max_bytes`. When the result is larger it
/// is written verbatim to a log file under `tmp_dir` and the inline text is
/// truncated with a pointer to that file, so the model can page through the
/// full output instead of losing it.
fn bounded_result(cfg: &Config, workspace: &Path, name: &str, text: String) -> String {
    let max = cfg.tool_result_max_bytes;
    let total = text.len();
    if total <= max {
        return text;
    }
    match spill_to_log(cfg, workspace, name, &text) {
        Ok(path) => {
            let shown = path
                .strip_prefix(workspace)
                .unwrap_or(&path)
                .to_string_lossy();
            format!(
                "{}\n[full result ({total} bytes) written to {shown}; read it with the read tool]",
                crate::llm::truncate(text, max),
            )
        }
        Err(_) => crate::llm::truncate(text, max),
    }
}

static SPILL_SEQ: AtomicU64 = AtomicU64::new(0);

/// Write a full tool result to `tmp_dir/tool-<name>-<ts>-<seq>.log`.
fn spill_to_log(cfg: &Config, workspace: &Path, name: &str, text: &str) -> Result<PathBuf> {
    let dir = cfg.tmp_path(workspace);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let seq = SPILL_SEQ.fetch_add(1, Ordering::Relaxed);
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let path = dir.join(format!("tool-{safe}-{ts}-{seq}.log"));
    std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

// ------------------------------------------------------------------ arg helpers

pub fn req_str(args: &Value, key: &str) -> Result<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("missing required string argument `{key}`"))
}

pub fn opt_str(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

pub fn opt_i64(args: &Value, key: &str) -> Option<i64> {
    args.get(key).and_then(|v| v.as_i64())
}

pub fn opt_bool(args: &Value, key: &str) -> Option<bool> {
    args.get(key).and_then(|v| v.as_bool())
}

#[cfg(test)]
mod tests {
    use super::{bounded_result, specs_for};
    use crate::config::Config;
    use crate::modes::Mode;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_workspace(tag: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("genji-tools-{tag}-{nanos}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn small_results_are_returned_verbatim() {
        let ws = temp_workspace("small");
        let cfg = Config::default();
        assert_eq!(bounded_result(&cfg, &ws, "bash", "ok".into()), "ok");
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn large_results_spill_to_tmp_log() {
        let ws = temp_workspace("spill");
        let cfg = Config {
            tool_result_max_bytes: 16,
            ..Default::default()
        };
        let full = "x".repeat(200);
        let out = bounded_result(&cfg, &ws, "bash", full.clone());
        assert!(out.contains("written to .genji/tmp/tool-bash-"), "{out}");
        assert!(out.contains("read it with the read tool"), "{out}");

        let name = out
            .split("written to ")
            .nth(1)
            .unwrap()
            .split(';')
            .next()
            .unwrap();
        let logged = std::fs::read_to_string(ws.join(name)).unwrap();
        assert_eq!(logged, full);
        let _ = std::fs::remove_dir_all(&ws);
    }

    fn names(mode: Mode, ocd: bool) -> Vec<String> {
        specs_for(mode, ocd, true)
            .into_iter()
            .map(|s| s.name.to_string())
            .collect()
    }

    fn names_without_skills(mode: Mode, ocd: bool) -> Vec<String> {
        specs_for(mode, ocd, false)
            .into_iter()
            .map(|s| s.name.to_string())
            .collect()
    }

    fn is_ticket_or_requirement(name: &str) -> bool {
        name.starts_with("ticket_") || name.starts_with("requirement_")
    }

    #[test]
    fn plan_write_is_plan_only() {
        assert!(names(Mode::Plan, false).iter().any(|n| n == "plan_write"));
        for mode in [Mode::Build, Mode::Explore, Mode::Retro] {
            assert!(
                !names(mode, false).iter().any(|n| n == "plan_write"),
                "{mode:?} exposed plan_write"
            );
        }
        // OCD does not change plan tool availability.
        assert!(names(Mode::Plan, true).iter().any(|n| n == "plan_write"));
    }

    #[test]
    fn ticket_tools_are_hidden_without_ocd() {
        for mode in [Mode::Plan, Mode::Build, Mode::Explore, Mode::Retro] {
            assert!(
                names(mode, false)
                    .iter()
                    .all(|n| !is_ticket_or_requirement(n)),
                "{mode:?} exposed a ticket/requirement tool with OCD off"
            );
        }
    }

    #[test]
    fn ticket_tools_appear_with_ocd() {
        let plan = names(Mode::Plan, true);
        assert!(plan.iter().any(|n| n == "ticket_create"));
        assert!(plan.iter().any(|n| n == "requirement_create"));
        assert!(plan.iter().any(|n| n == "requirement_tree"));

        let build = names(Mode::Build, true);
        assert!(build.iter().any(|n| n == "ticket_claim"));
        assert!(build.iter().any(|n| n == "ticket_update"));
        // Build still cannot create requirements.
        assert!(!build.iter().any(|n| n == "requirement_create"));
    }

    #[test]
    fn skill_load_hidden_without_skills() {
        for mode in [Mode::Plan, Mode::Build, Mode::Explore, Mode::Retro] {
            assert!(
                !names_without_skills(mode, false)
                    .iter()
                    .any(|n| n == "skill_load"),
                "{mode:?} exposed skill_load with no skills"
            );
        }
    }

    #[test]
    fn ocd_does_not_leak_into_explore_or_retro() {
        for mode in [Mode::Explore, Mode::Retro] {
            assert!(
                names(mode, true)
                    .iter()
                    .all(|n| !is_ticket_or_requirement(n)),
                "{mode:?} exposed a ticket/requirement tool with OCD on"
            );
        }
    }
}
