//! Helpers shared by the `genji-eval` and `genji-drive` binaries.

use anyhow::{Context, Result};
use std::fs;
use std::path::Path;

/// Recursively copy the directory `from` into `to`, creating `to`.
pub fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to)?;
    for e in fs::read_dir(from).with_context(|| format!("read {}", from.display()))? {
        let e = e?;
        let dest = to.join(e.file_name());
        if e.file_type()?.is_dir() {
            copy_dir(&e.path(), &dest)?;
        } else {
            fs::copy(e.path(), dest)?;
        }
    }
    Ok(())
}

/// Like [`copy_dir`], but a missing source is not an error.
pub fn copy_dir_if_exists(from: &Path, to: &Path) -> Result<()> {
    if from.exists() {
        copy_dir(from, to)
    } else {
        Ok(())
    }
}
