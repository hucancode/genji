use anyhow::{Context, Result};
use std::path::Path;

use crate::config::Config;
use crate::db::Db;
use crate::modes::Mode;

pub fn prompt_file(cfg: &Config, workspace: &Path, mode: Mode) -> std::path::PathBuf {
    cfg.prompts_path(workspace)
        .join(format!("{}.md", mode.as_str()))
}

pub fn write_prompt_file(cfg: &Config, workspace: &Path, mode: Mode, content: &str) -> Result<()> {
    let dir = cfg.prompts_path(workspace);
    std::fs::create_dir_all(&dir)?;
    let path = prompt_file(cfg, workspace, mode);
    std::fs::write(&path, format!("{}\n", content.trim_end()))
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Ensure every extensible mode has an extended prompt file and an active DB
/// version. If the user edited the file, a new version authored `user` is
/// recorded. Modes without an extended prompt (RETRO) are skipped.
pub fn seed_prompts(db: &Db, cfg: &Config, workspace: &Path) -> Result<()> {
    for mode in Mode::all().into_iter().filter(Mode::allows_extended) {
        let path = prompt_file(cfg, workspace, mode);
        let content = if path.exists() {
            std::fs::read_to_string(&path)?
        } else {
            let c = mode.default_extended().to_string();
            std::fs::create_dir_all(cfg.prompts_path(workspace))?;
            std::fs::write(&path, &c)?;
            c
        };
        let active = db.prompt_active(mode.as_str())?;
        match active {
            None => {
                db.prompt_add_version(mode.as_str(), &content, "default", "initial seed")?;
            }
            Some(p) => {
                if p.content.trim() != content.trim() {
                    db.prompt_add_version(mode.as_str(), &content, "user", "file changed")?;
                }
            }
        }
    }
    Ok(())
}

pub fn load_extended(db: &Db, cfg: &Config, workspace: &Path, mode: Mode) -> Result<String> {
    if !mode.allows_extended() {
        return Ok(String::new());
    }
    if let Some(p) = db.prompt_active(mode.as_str())? {
        return Ok(p.content);
    }
    let path = prompt_file(cfg, workspace, mode);
    if path.exists() {
        Ok(std::fs::read_to_string(path)?)
    } else {
        Ok(mode.default_extended().to_string())
    }
}
