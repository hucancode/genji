use anyhow::Result;

use crate::db::Db;
use crate::modes::Mode;

pub fn seed_prompts(db: &Db) -> Result<()> {
    for mode in Mode::all().into_iter().filter(Mode::allows_extended) {
        if db.prompt_active(mode.as_str())?.is_none() {
            db.prompt_add_version(mode.as_str(), "", "default", "initial seed")?;
        }
    }
    Ok(())
}

/// Load the active extended prompt for a mode from the database. Returns an
/// empty string for modes without an extended prompt (RETRO).
pub fn load_extended(db: &Db, mode: Mode) -> Result<String> {
    if !mode.allows_extended() {
        return Ok(String::new());
    }
    Ok(db
        .prompt_active(mode.as_str())?
        .map(|p| p.content)
        .unwrap_or_default())
}
