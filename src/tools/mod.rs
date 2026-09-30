use anyhow::{anyhow, Result};
use serde_json::{json, Value};

use crate::agent::Agent;
use crate::modes::Mode;

pub mod basic;
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

fn t(name: &'static str, description: &'static str, parameters: Value) -> ToolSpec {
    ToolSpec {
        name,
        description,
        parameters,
    }
}

pub fn all_specs() -> Vec<ToolSpec> {
    vec![
        t("read", "Read a text file with line numbers. offset is 1-indexed.", json!({
            "type":"object",
            "properties":{
                "path":{"type":"string","description":"File path"},
                "offset":{"type":"integer","description":"First line (1-indexed)"},
                "limit":{"type":"integer","description":"Max lines to read (default 2000)"}
            },
            "required":["path"]
        })),
        t("write", "Create or overwrite a file, creating parent directories.", json!({
            "type":"object",
            "properties":{"path":{"type":"string"},"content":{"type":"string"}},
            "required":["path","content"]
        })),
        t("edit", "Apply precise text replacements to a file. Each oldText must match uniquely.", json!({
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
        })),
        t("ls", "List files/directories respecting .gitignore. Non-recursive unless recursive=true.", json!({
            "type":"object",
            "properties":{
                "path":{"type":"string","description":"Directory (default .)"},
                "recursive":{"type":"boolean"},
                "max_depth":{"type":"integer"},
                "show_hidden":{"type":"boolean"}
            }
        })),
        t("bash", "Run a shell command via bash -c in the workspace. Returns exit code, stdout, stderr.", json!({
            "type":"object",
            "properties":{
                "command":{"type":"string"},
                "cwd":{"type":"string","description":"Working directory (default workspace)"},
                "timeout_secs":{"type":"integer"}
            },
            "required":["command"]
        })),
        // ---- tickets ----
        t("ticket_create", "Create a work ticket.", json!({
            "type":"object",
            "properties":{
                "title":{"type":"string"},
                "description":{"type":"string"},
                "priority":{"type":"integer","description":"1=high, 2=normal, 3=low"},
                "parent_id":{"type":"integer"},
                "requirement_id":{"type":"integer","description":"Requirement this ticket addresses"}
            },
            "required":["title"]
        })),
        t("ticket_read", "Read one ticket by id, or list tickets.", json!({
            "type":"object",
            "properties":{
                "id":{"type":"integer"},
                "status":{"type":"string","enum":["open","in_progress","resolved","closed"]},
                "requirement_id":{"type":"integer"}
            }
        })),
        t("ticket_resolve", "Mark a ticket resolved after the work is done and verified.", json!({
            "type":"object",
            "properties":{"id":{"type":"integer"},"resolution":{"type":"string"}},
            "required":["id"]
        })),
        t("ticket_close", "Close a ticket as obsolete/duplicate/won't-fix.", json!({
            "type":"object",
            "properties":{"id":{"type":"integer"},"reason":{"type":"string"}},
            "required":["id"]
        })),
        // ---- requirements ----
        t("requirement_create", "Create a stakeholder or system requirement.", json!({
            "type":"object",
            "properties":{
                "level":{"type":"string","enum":["stakeholder","system"]},
                "title":{"type":"string"},
                "body":{"type":"string"},
                "parent_id":{"type":"integer","description":"Parent requirement id"}
            },
            "required":["level","title","body"]
        })),
        t("requirement_read", "Read a requirement by id, or list requirements.", json!({
            "type":"object",
            "properties":{
                "id":{"type":"integer"},
                "level":{"type":"string","enum":["stakeholder","system"]},
                "status":{"type":"string","enum":["active","met","removed"]}
            }
        })),
        t("requirement_update", "Update a requirement's title/body/status/level/parent.", json!({
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
        })),
        t("requirement_remove", "Remove a requirement (soft by default).", json!({
            "type":"object",
            "properties":{"id":{"type":"integer"},"hard":{"type":"boolean"}},
            "required":["id"]
        })),
        t("requirement_ask", "Ask the user a clarifying question about a requirement. Recorded in the DB.", json!({
            "type":"object",
            "properties":{
                "question":{"type":"string"},
                "requirement_id":{"type":"integer"}
            },
            "required":["question"]
        })),
        // ---- skills ----
        t("skill_load", "Load a skill's instructions by name.", json!({
            "type":"object",
            "properties":{"name":{"type":"string"}},
            "required":["name"]
        })),
        // ---- spawn ----
        t("spawn", "Spawn a subagent in a given mode that only reports back.", json!({
            "type":"object",
            "properties":{
                "mode":{"type":"string","enum":["plan","build","explore"]},
                "instructions":{"type":"string","description":"What the subagent should do"},
                "task":{"type":"string","description":"Optional task label"}
            },
            "required":["mode","instructions"]
        })),
        // ---- retro ----
        t("query_instances", "List past agent instances.", json!({
            "type":"object",
            "properties":{
                "mode":{"type":"string"},
                "limit":{"type":"integer","description":"Default 20"}
            }
        })),
        t("query_instance", "Get all messages of one instance.", json!({
            "type":"object",
            "properties":{"instance_id":{"type":"string"},"limit":{"type":"integer"}},
            "required":["instance_id"]
        })),
        t("query_messages", "Search recorded messages by text/role/instance.", json!({
            "type":"object",
            "properties":{
                "instance_id":{"type":"string"},
                "role":{"type":"string"},
                "search":{"type":"string"},
                "limit":{"type":"integer","description":"Default 50"}
            }
        })),
        t("query_tool_call", "Query recorded tool calls (filter by name/errors/instance).", json!({
            "type":"object",
            "properties":{
                "instance_id":{"type":"string"},
                "name":{"type":"string"},
                "errors_only":{"type":"boolean"},
                "limit":{"type":"integer","description":"Default 50"}
            }
        })),
        t("query_stats", "Aggregate stats: tool usage, error rates, skill loads, token usage.", json!({
            "type":"object","properties":{}
        })),
        t("list_skills", "List all skills with descriptions and use counts.", json!({
            "type":"object","properties":{}
        })),
        t("read_skill", "Read a skill, optionally a specific version.", json!({
            "type":"object",
            "properties":{"name":{"type":"string"},"version":{"type":"integer"}},
            "required":["name"]
        })),
        t("write_skill", "Create or overwrite a skill (versioned).", json!({
            "type":"object",
            "properties":{
                "name":{"type":"string"},
                "description":{"type":"string"},
                "content":{"type":"string"},
                "reason":{"type":"string"}
            },
            "required":["name","content"]
        })),
        t("edit_skill", "Edit a skill by text replacement (versioned).", json!({
            "type":"object",
            "properties":{
                "name":{"type":"string"},
                "oldText":{"type":"string"},
                "newText":{"type":"string"},
                "reason":{"type":"string"}
            },
            "required":["name","oldText","newText"]
        })),
        t("skill_history", "List versions of a skill.", json!({
            "type":"object","properties":{"name":{"type":"string"}},"required":["name"]
        })),
        t("skill_rollback", "Activate an older version of a skill.", json!({
            "type":"object",
            "properties":{"name":{"type":"string"},"version":{"type":"integer"}},
            "required":["name","version"]
        })),
        t("prompt_read", "Read the active extended system prompt for a mode. RETRO has no extended prompt.", json!({
            "type":"object","properties":{"mode":{"type":"string","enum":["plan","build","explore"]}},"required":["mode"]
        })),
        t("prompt_edit", "Replace the extended system prompt for a mode (versioned). Core prompt is not editable; RETRO has no extended prompt.", json!({
            "type":"object",
            "properties":{
                "mode":{"type":"string","enum":["plan","build","explore"]},
                "content":{"type":"string"},
                "reason":{"type":"string"}
            },
            "required":["mode","content"]
        })),
        t("prompt_history", "List versions of a mode's extended prompt. RETRO has no extended prompt.", json!({
            "type":"object","properties":{"mode":{"type":"string","enum":["plan","build","explore"]}},"required":["mode"]
        })),
        t("prompt_rollback", "Activate an older version of a mode's extended prompt. RETRO has no extended prompt.", json!({
            "type":"object",
            "properties":{"mode":{"type":"string","enum":["plan","build","explore"]},"version":{"type":"integer"}},
            "required":["mode","version"]
        })),
    ]
}

pub fn specs_for(mode: Mode) -> Vec<ToolSpec> {
    let all = all_specs();
    mode.tool_names()
        .iter()
        .filter_map(|n| all.iter().find(|s| s.name == *n).cloned())
        .collect()
}

/// Execute a tool. Returns (result_text, is_error). Result is truncated here so
/// every caller gets bounded content.
pub fn dispatch(agent: &mut Agent, name: &str, args: &Value) -> (String, bool) {
    let res: Result<String> = match name {
        "read" => basic::read(agent, args),
        "write" => basic::write(agent, args),
        "edit" => basic::edit(agent, args),
        "ls" => basic::ls(agent, args),
        "bash" => basic::bash(agent, args),
        "ticket_create" => tickets::create(agent, args),
        "ticket_read" => tickets::read(agent, args),
        "ticket_resolve" => tickets::resolve(agent, args),
        "ticket_close" => tickets::close(agent, args),
        "requirement_create" => requirements::create(agent, args),
        "requirement_read" => requirements::read(agent, args),
        "requirement_update" => requirements::update(agent, args),
        "requirement_remove" => requirements::remove(agent, args),
        "requirement_ask" => requirements::ask(agent, args),
        "skill_load" => skills::load(agent, args),
        "spawn" => spawn::spawn(agent, args),
        "query_instances" => retro::instances(agent, args),
        "query_instance" => retro::instance(agent, args),
        "query_messages" => retro::messages(agent, args),
        "query_tool_call" => retro::tool_calls(agent, args),
        "query_stats" => retro::stats(agent, args),
        "list_skills" => skills::list(agent, args),
        "read_skill" => skills::read(agent, args),
        "write_skill" => skills::write(agent, args),
        "edit_skill" => skills::edit(agent, args),
        "skill_history" => skills::history(agent, args),
        "skill_rollback" => skills::rollback(agent, args),
        "prompt_read" => retro::prompt_read(agent, args),
        "prompt_edit" => retro::prompt_edit(agent, args),
        "prompt_history" => retro::prompt_history(agent, args),
        "prompt_rollback" => retro::prompt_rollback(agent, args),
        other => Err(anyhow!("unknown or unavailable tool `{other}`")),
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
