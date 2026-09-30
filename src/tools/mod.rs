use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::sync::OnceLock;

use crate::agent::Agent;
use crate::modes::Mode;

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
            ocd_tool("ticket_read", PLAN_BUILD, "Read one ticket by id, or list tickets.", json!({
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
            ocd_tool("ticket_resolve", PLAN_BUILD, "Mark a ticket resolved after the work is done and verified.", json!({
                "type":"object",
                "properties":{"id":{"type":"integer"},"resolution":{"type":"string"}},
                "required":["id"]
            }), tickets::resolve),
            ocd_tool("ticket_close", PLAN_BUILD, "Close a ticket as obsolete/duplicate/won't-fix.", json!({
                "type":"object",
                "properties":{"id":{"type":"integer"},"reason":{"type":"string"}},
                "required":["id"]
            }), tickets::close),
            ocd_tool("ticket_reopen", PLAN_BUILD, "Reopen a resolved/closed ticket as open, clearing its resolution.", json!({
                "type":"object",
                "properties":{"id":{"type":"integer"}},
                "required":["id"]
            }), tickets::reopen),
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
            ocd_tool("requirement_read", PLAN_BUILD, "Read a requirement by id, or list requirements.", json!({
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
            tool("skill_load", ALL_MODES, "Load a skill's instructions by name.", json!({
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

pub fn specs_for(mode: Mode, ocd: bool) -> Vec<ToolSpec> {
    registry()
        .iter()
        .filter(|t| t.modes.contains(&mode))
        .filter(|t| !t.requires_ocd || ocd)
        .map(|t| t.spec())
        .collect()
}

/// Execute a tool. Returns (result_text, is_error). Result is truncated here so
/// every caller gets bounded content.
pub fn dispatch(agent: &mut Agent, name: &str, args: &Value) -> (String, bool) {
    let res: Result<String> = match registry().iter().find(|t| t.name == name) {
        Some(t) => (t.handler)(agent, args),
        None => Err(anyhow!("unknown or unavailable tool `{name}`")),
    };
    match res {
        Ok(s) => (
            crate::llm::truncate(s, agent.cfg.tool_result_max_bytes),
            false,
        ),
        Err(e) => (
            crate::llm::truncate(format!("ERROR: {e:#}"), agent.cfg.tool_result_max_bytes),
            true,
        ),
    }
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
    use super::specs_for;
    use crate::modes::Mode;

    fn names(mode: Mode, ocd: bool) -> Vec<String> {
        specs_for(mode, ocd)
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
        assert!(build.iter().any(|n| n == "ticket_reopen"));
        // Build still cannot create requirements.
        assert!(!build.iter().any(|n| n == "requirement_create"));
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
