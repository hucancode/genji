//! Small shared filesystem helpers.

use anyhow::{Context, Result};
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Write `text` to `path`, creating missing parent directories.
pub fn write_file(path: &Path, text: impl AsRef<[u8]>) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))
}

/// A unique path under `dir` for a scratch file named `<prefix>-<pid>-<n>.<ext>`.
pub fn tmp_file(dir: &Path, prefix: &str, ext: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    dir.join(format!("{prefix}-{}-{n}.{ext}", std::process::id()))
}

/// A scratch file path removed on drop.
pub struct TempPath(pub PathBuf);

impl Drop for TempPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// A short random-looking instance id (6 hex digits).
pub fn new_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut x = (unix_millis() ^ (u64::from(std::process::id()) << 21) ^ n)
        .wrapping_mul(0x9e37_79b9_7f4a_7c15);
    x ^= x >> 32;
    format!("{:06x}", x & 0x00ff_ffff)
}

pub fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Expand a leading `~/`, then resolve relative paths against `workspace`.
pub fn resolve_path(workspace: &Path, path: &str) -> PathBuf {
    let expanded = match (path.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(path),
    };
    if expanded.is_absolute() {
        expanded
    } else {
        workspace.join(expanded)
    }
}

pub fn relative_path(workspace: &Path, path: &Path) -> String {
    path.strip_prefix(workspace)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

/// Split `---\nkey: value\n---\nbody` into its keys and body.
pub fn split_frontmatter(text: &str) -> (BTreeMap<String, String>, String) {
    let mut meta = BTreeMap::new();
    if let Some(rest) = text.strip_prefix("---\n")
        && let Some(index) = rest.find("\n---")
    {
        for line in rest[..index].lines() {
            if let Some((key, value)) = line.split_once(':') {
                meta.insert(
                    key.trim().to_owned(),
                    value.trim().trim_matches('"').to_owned(),
                );
            }
        }
        let body = &rest[index + 4..];
        return (meta, body.strip_prefix('\n').unwrap_or(body).to_owned());
    }
    (meta, text.to_owned())
}

/// `id` reduced to letters, digits, `-` and `_` (at most 40 chars), safe in a file name.
pub fn sanitize(id: &str) -> String {
    let s: String = id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .take(40)
        .collect();
    if s.is_empty() { "call".into() } else { s }
}

/// A bare file-name slug: letters, digits, `-` and `_`, at most 64 chars.
pub fn valid_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug.len() <= 64
        && slug
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

pub fn truncate(s: &str, max: usize) -> Cow<'_, str> {
    if s.len() <= max {
        return Cow::Borrowed(s);
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    Cow::Owned(format!(
        "{}… [{} bytes truncated]",
        &s[..end],
        s.len() - end
    ))
}

/// Join the kept head and tail of a text, noting how many bytes were dropped between them.
pub fn join_head_tail(head: &str, tail: &str, omitted: usize) -> String {
    format!("{head}\n[... {omitted} bytes omitted ...]\n{tail}")
}

/// Keep the first two thirds and the last third of `s`, cut on char boundaries; command
/// output often puts the failure or summary at the end.
pub fn head_and_tail(s: &str, max: usize) -> Cow<'_, str> {
    if s.len() <= max {
        return Cow::Borrowed(s);
    }
    let (mut head, mut tail) = (max * 2 / 3, max / 3);
    while head > 0 && !s.is_char_boundary(head) {
        head -= 1;
    }
    tail = s.len().saturating_sub(tail);
    while tail < s.len() && !s.is_char_boundary(tail) {
        tail += 1;
    }
    Cow::Owned(join_head_tail(&s[..head], &s[tail..], tail - head))
}

/// A fresh, empty scratch directory for a test.
#[cfg(test)]
pub fn temp_dir(tag: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("genji-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_on_char_boundary() {
        let s = "é".repeat(50);
        let out = truncate(&s, 11);
        assert!(out.contains("truncated"));
        assert!(out.len() < 50 * 2 + 40);
        assert_eq!(truncate("hi", 10), "hi");
    }

    #[test]
    fn head_and_tail_keeps_both_ends() {
        let s = format!("{}MID{}", "a".repeat(100), "z".repeat(100));
        let out = head_and_tail(&s, 30);
        assert!(out.starts_with("aaaa") && out.ends_with("zzzz") && out.contains("omitted"));
        assert!(!out.contains("MID") && out.len() < 100);
        assert_eq!(head_and_tail("short", 30), "short");
    }

    #[test]
    fn resolves_workspace_paths() {
        let ws = Path::new("/workspace");
        assert_eq!(resolve_path(ws, "src/main.rs"), ws.join("src/main.rs"));
        assert_eq!(resolve_path(ws, "/tmp/file"), Path::new("/tmp/file"));
    }

    #[test]
    fn call_tags_are_file_safe() {
        assert_eq!(sanitize("call_abc/../1"), "call_abc1");
        assert_eq!(sanitize("///"), "call");
    }

    #[test]
    fn slugs_are_safe_file_names() {
        assert!(valid_slug("rate-limiting") && valid_slug("Plan_2"));
        assert!(
            !valid_slug("")
                && !valid_slug("../escape")
                && !valid_slug("a/b")
                && !valid_slug(&"x".repeat(65))
        );
    }

    #[test]
    fn frontmatter_parsing() {
        let (meta, body) = split_frontmatter("---\ntools: read, ls\nname: \"x\"\n---\nbody\n");
        assert_eq!(meta["tools"], "read, ls");
        assert_eq!(meta["name"], "x");
        assert_eq!(body, "body\n");
        assert_eq!(split_frontmatter("plain").1, "plain");
    }
}
