use anyhow::{bail, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Plan,
    Build,
    Explore,
    Retro,
}

impl Mode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Mode::Plan => "plan",
            Mode::Build => "build",
            Mode::Explore => "explore",
            Mode::Retro => "retro",
        }
    }

    pub fn parse(s: &str) -> Result<Mode> {
        match s.to_ascii_lowercase().as_str() {
            "plan" => Ok(Mode::Plan),
            "build" => Ok(Mode::Build),
            "explore" => Ok(Mode::Explore),
            "retro" => Ok(Mode::Retro),
            other => bail!("unknown mode `{other}` (expected plan|build|explore|retro)"),
        }
    }

    /// Tools available in this mode. Basic tools are shared; ticket/requirement
    /// tools are progressively restricted, explore has none at all.
    pub fn tool_names(&self) -> Vec<&'static str> {
        let mut t: Vec<&'static str> = vec!["read", "write", "edit", "ls", "bash"];
        match self {
            Mode::Plan => {
                t.extend([
                    "ticket_create",
                    "ticket_read",
                    "ticket_resolve",
                    "ticket_close",
                    "requirement_create",
                    "requirement_read",
                    "requirement_update",
                    "requirement_remove",
                    "requirement_ask",
                    "skill_load",
                    "spawn",
                ]);
            }
            Mode::Build => {
                // Reduced ticket/requirement surface for implementation work.
                t.extend([
                    "ticket_read",
                    "ticket_resolve",
                    "ticket_close",
                    "requirement_read",
                    "requirement_ask",
                    "skill_load",
                    "spawn",
                ]);
            }
            Mode::Explore => {
                // No ticket or requirement tools. Read/inspect and report.
                t.extend(["skill_load", "spawn"]);
            }
            Mode::Retro => {
                t.extend([
                    "skill_load",
                    "query_instances",
                    "query_instance",
                    "query_messages",
                    "query_tool_call",
                    "query_stats",
                    "list_skills",
                    "read_skill",
                    "write_skill",
                    "edit_skill",
                    "skill_history",
                    "skill_rollback",
                    "prompt_read",
                    "prompt_edit",
                    "prompt_history",
                    "prompt_rollback",
                ]);
            }
        }
        t
    }

    /// Minimal, non-editable core system prompt for the mode.
    pub fn core_prompt(&self) -> &'static str {
        match self {
            Mode::Plan => CORE_PLAN,
            Mode::Build => CORE_BUILD,
            Mode::Explore => CORE_EXPLORE,
            Mode::Retro => CORE_RETRO,
        }
    }

    /// Whether this mode has a user-editable extended prompt. RETRO is
    /// intentionally fixed: it must not be able to extend or rewrite its own
    /// instructions, directly or via a spawned agent.
    pub fn allows_extended(&self) -> bool {
        !matches!(self, Mode::Retro)
    }

    /// Starter text for the user-editable extended prompt. Empty for every
    /// mode: extended prompts are stored in the database, not seeded with
    /// default content, and modes that do not support one
    /// (see [`Mode::allows_extended`]) are also empty.
    pub fn default_extended(&self) -> &'static str {
        match self {
            Mode::Plan => DEFAULT_EXT_PLAN,
            Mode::Build => DEFAULT_EXT_BUILD,
            Mode::Explore => DEFAULT_EXT_EXPLORE,
            Mode::Retro => "",
        }
    }

    pub fn all() -> [Mode; 4] {
        [Mode::Plan, Mode::Build, Mode::Explore, Mode::Retro]
    }
}

const SHARED: &str = r#"You are genji, a coding agent.
Be concise and act. Prefer doing over explaining. Use tools to inspect reality;
Tool results may be truncated when large; if so, narrow your query.
Call tools using the provided function interface. When the task is complete, reply with a short final report and no tool calls.
"#;

const CORE_PLAN: &str = r#"
You convert stakeholder intent into an actionable, verifiable plan.

Workflow:
1. Read the stakeholder and system requirements (`requirement_read`).
2. Explore the workspace enough to understand the current state (`ls`, `read`, `bash`).
3. Derive concrete SYSTEM requirements from STAKEHOLDER requirements (`requirement_create`, level="system").
4. Create tickets for concrete units of work (`ticket_create`), linking them to requirements.
5. Mark a requirement as met (`requirement_update` status="met") only when you are confident it is satisfied by current artifacts; otherwise leave it active.
6. When the plan is complete, stop with a brief summary.

You plan and specify.
"#;

const CORE_BUILD: &str = r#"
You implement the plan. Open tickets are your work queue.

Workflow:
1. Read the requirements and inspect open tickets (`ticket_read`, `requirement_read`).
2. Pick the highest-value open ticket.
3. Do the work with `read`/`write`/`edit`/`bash`. Verify your changes (build, test, run).
4. Resolve the ticket (`ticket_resolve`) when the work is verified, or `ticket_close` if it is obsolete/duplicate.
5. Repeat until no actionable tickets remain, then stop with a brief report.

You may not create requirements or tickets. If you discover missing work, report it.
"#;

const CORE_EXPLORE: &str = r#"
You investigate and report.

Workflow:
1. Explore the workspace to answer the question you were given.
2. Read files, run read-only commands, gather evidence.
3. Report findings concisely: what you found, where, and what it implies. Cite file paths.

Do not make changes. Avoid destructive commands.
"#;

const CORE_RETRO: &str = r#"
You improve the agent itself by studying its recorded history.

You have read/write/edit/ls/bash plus tools to query the instance database:
query_instances, query_instance, query_messages, query_tool_call, query_stats,
list_skills, read_skill, write_skill, edit_skill, skill_history, skill_rollback,
prompt_read, prompt_edit, prompt_history, prompt_rollback.

Workflow:
1. Gather evidence: `query_stats` first, then drill into failing tool calls, repeated loops, and loaded skills.
2. Identify concrete, generalizable improvements (better prompts, better skills).
3. Apply them:
   - `prompt_edit` changes the user-editable extended prompt for a mode (versioned; you may only edit the extended part).
   - `write_skill`/`edit_skill` create or improve skills (versioned).
   - Use `prompt_history`/`prompt_rollback` and `skill_history`/`skill_rollback` to inspect or revert.
4. Record why you made each change in the `reason` field.
5. Stop with a concise report of changes and evidence.

Every change is versioned in the database and can be rolled back.
"#;

// Extended prompts start empty and live only in the database. Retro mode
// populates them via `prompt_edit`; nothing is written to disk.
const DEFAULT_EXT_PLAN: &str = "";
const DEFAULT_EXT_BUILD: &str = "";
const DEFAULT_EXT_EXPLORE: &str = "";

pub fn shared_preamble() -> &'static str {
    SHARED
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retro_has_no_extended_prompt() {
        assert!(!Mode::Retro.allows_extended());
        assert!(Mode::Retro.default_extended().is_empty());
    }

    #[test]
    fn other_modes_are_extensible() {
        for mode in [Mode::Plan, Mode::Build, Mode::Explore] {
            assert!(mode.allows_extended());
            assert!(mode.default_extended().is_empty());
        }
    }
}
