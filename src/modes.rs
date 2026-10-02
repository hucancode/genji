use anyhow::{Result, bail};

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
            Mode::Plan => include_str!("prompts/plan.md"),
            Mode::Build => include_str!("prompts/build.md"),
            Mode::Explore => include_str!("prompts/explore.md"),
            Mode::Retro => include_str!("prompts/retro.md"),
        }
    }

    /// Whether this mode has a user-editable extended prompt. RETRO is
    /// intentionally fixed: it must not be able to extend or rewrite its own
    /// instructions, directly or via a spawned agent.
    pub fn allows_extended(&self) -> bool {
        !matches!(self, Mode::Retro)
    }

    /// Extra system-prompt guidance appended when the Formal flag is on. Empty for
    /// modes that never touch the requirements/tickets system.
    pub fn formal_guidance(&self) -> &'static str {
        match self {
            Mode::Plan => include_str!("prompts/formal-plan.md"),
            Mode::Build => include_str!("prompts/formal-build.md"),
            _ => "",
        }
    }

    pub fn all() -> [Mode; 4] {
        [Mode::Plan, Mode::Build, Mode::Explore, Mode::Retro]
    }
}

pub fn shared_preamble() -> &'static str {
    include_str!("prompts/shared.md")
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
