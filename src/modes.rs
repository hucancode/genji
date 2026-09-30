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

    /// Extra system-prompt guidance appended when the OCD flag is on. Empty for
    /// modes that never touch the requirements/tickets system.
    pub fn ocd_guidance(&self) -> &'static str {
        match self {
            Mode::Plan => OCD_PLAN_GUIDANCE,
            Mode::Build => OCD_BUILD_GUIDANCE,
            _ => "",
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
You produce a clear, actionable plan before work begins.

Workflow:
1. Investigate the workspace and the request enough to understand the current state (`ls`, `read`, `bash`).
2. Break the work into concrete, ordered, verifiable steps.
3. Report the plan: the steps, the files likely to change, and how you will verify the result.

You plan and specify. Prefer inspecting reality over speculation. Do not make changes unless asked.
"#;

const CORE_BUILD: &str = r#"
You implement the requested changes and verify them.

Workflow:
1. Inspect the relevant files and understand the task (`read`, `ls`, `bash`).
2. Make focused changes with `write`/`edit`.
3. Verify your work: build, test, and run what you changed.
4. Report what changed, how you verified it, and anything still open.

Prefer small, correct changes over broad rewrites.
"#;

// OCD is an opt-in flag (not a mode): when it is on, plan/build gain the
// requirements/tickets surface and the run auto-cycles between them. These
// guidance blocks are appended to the matching core prompt only when OCD is on.
const OCD_PLAN_GUIDANCE: &str = r#"
## OCD: requirements and tickets

OCD is enabled, so the requirements/tickets system is your source of truth.
Requirements are markdown files under `.genji/requirements/`; tickets live in
the workspace database. Workflow:
1. Read the stakeholder and system requirements (`requirement_read`).
2. Explore the workspace enough to understand the current state (`ls`, `read`, `bash`).
3. Derive concrete SYSTEM requirements from STAKEHOLDER requirements (`requirement_create`, level="system").
4. Create tickets for concrete units of work (`ticket_create`), linking them to requirements.
5. Check coverage with `requirement_tree` so every active requirement has a path to being met.
6. Mark a requirement `met` (`requirement_update` status="met") only when you are confident current artifacts satisfy it; otherwise leave it active.
7. Stop with a brief summary once the plan is current.
"#;

const OCD_BUILD_GUIDANCE: &str = r#"
## OCD: requirements and tickets

OCD is enabled. Open tickets are your work queue and the requirements are the
success criteria. Workflow:
1. Read the requirements and open tickets (`requirement_read`, `ticket_read`).
2. Claim the highest-value open ticket with `ticket_claim` (or read a specific one).
3. Do the work with `read`/`write`/`edit`/`bash`, then verify it (build, test, run).
4. Resolve the ticket (`ticket_resolve`) when verified, or `ticket_close` if it is obsolete/duplicate. Use `ticket_update` to refine details and `ticket_reopen` if a resolved ticket turns out to be incomplete.
5. Repeat until no actionable tickets remain, then stop with a brief report.

Do not create requirements in this mode. If you discover missing work, report it.
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

pub fn shared_preamble() -> &'static str {
    SHARED
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retro_has_no_extended_prompt() {
        assert!(!Mode::Retro.allows_extended());
    }

    #[test]
    fn other_modes_are_extensible() {
        for mode in [Mode::Plan, Mode::Build, Mode::Explore] {
            assert!(mode.allows_extended());
        }
    }
}
