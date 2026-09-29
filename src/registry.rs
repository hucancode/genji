//! A lightweight registry of running genji instances.
//!
//! Every top-level run that opens a control socket writes a small JSON record
//! to a per-user directory. `genji list` / `stop` / `instruct` / `inspect` read
//! those records to find and steer instances, even from another workspace.
//!
//! Records are best-effort: a crashed process may leave a stale file behind,
//! which is detected by probing the control socket and cleaned up on the next
//! listing.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Instance {
    pub id: String,
    pub pid: u32,
    pub workspace: String,
    pub control_socket: String,
    #[serde(default)]
    pub label: String,
    pub started_at: u64,
}

impl Instance {
    pub fn path(&self) -> PathBuf {
        dir().join(format!("{}.json", self.id))
    }

    pub fn save(&self) -> Result<()> {
        let d = dir();
        std::fs::create_dir_all(&d)
            .with_context(|| format!("creating instance registry {}", d.display()))?;
        let p = self.path();
        let text = serde_json::to_string_pretty(self)?;
        std::fs::write(&p, format!("{text}\n"))
            .with_context(|| format!("writing instance record {}", p.display()))?;
        Ok(())
    }

    pub fn uptime_secs(&self) -> u64 {
        now_secs().saturating_sub(self.started_at)
    }

    /// True while the instance's control socket still accepts connections.
    pub fn is_live(&self) -> bool {
        crate::control::send(Path::new(&self.control_socket), "/ping")
            .map(|r| !r.trim().is_empty())
            .unwrap_or(false)
    }
}

/// Directory holding instance records. `GENJI_REGISTRY_DIR` overrides it
/// (handy for tests); otherwise it lives under the user's home directory.
pub fn dir() -> PathBuf {
    if let Ok(d) = std::env::var("GENJI_REGISTRY_DIR") {
        if !d.trim().is_empty() {
            return PathBuf::from(d);
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        if !home.trim().is_empty() {
            return PathBuf::from(home).join(".genji").join("instances");
        }
    }
    std::env::temp_dir().join("genji-instances")
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn candidate(seed: u64) -> u64 {
    let mut x = seed;
    if x == 0 {
        x = 0x9e37_79b9_7f4a_7c15;
    }
    // mix so small pid/time deltas spread across the id space
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
    x ^= x >> 33;
    x
}

/// A short, human-friendly instance id, unique among currently-registered ids.
pub fn new_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut x = candidate(nanos ^ ((std::process::id() as u64) << 21));
    let d = dir();
    for _ in 0..64 {
        let id = format!("{:06x}", x & 0x00ff_ffff);
        if !d.join(format!("{id}.json")).exists() {
            return id;
        }
        x = candidate(x);
    }
    format!("{:08x}", (nanos as u32) ^ std::process::id())
}

/// Parse every valid record on disk (invalid files are dropped). Does not probe
/// liveness.
pub fn load_all() -> Vec<Instance> {
    let d = dir();
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(&d) else {
        return out;
    };
    for entry in rd.flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&p) else {
            continue;
        };
        match serde_json::from_str::<Instance>(&text) {
            Ok(inst) => out.push(inst),
            Err(_) => {
                let _ = std::fs::remove_file(&p);
            }
        }
    }
    out.sort_by_key(|i| (i.started_at, i.id.clone()));
    out
}

/// Live instances, pruning stale records as a side effect.
pub fn list_live() -> Vec<Instance> {
    let mut out = Vec::new();
    for inst in load_all() {
        if inst.is_live() {
            out.push(inst);
        } else {
            remove(&inst.id);
        }
    }
    out
}

/// Resolve an instance id against the live pool, leniently.
///
/// An exact id always wins. Otherwise a unique id prefix is accepted, so
/// `genji instruct abc` resolves `abc123` when it is the only live instance
/// starting with `abc`. An ambiguous prefix reports the candidates instead of
/// guessing, and a prefix that matches nothing is treated as unknown.
pub fn find(id: &str) -> Result<Instance> {
    let id = id.trim();

    // Prune stale records while we gather live instances.
    let mut live = Vec::new();
    for inst in load_all() {
        if inst.is_live() {
            live.push(inst);
        } else {
            remove(&inst.id);
        }
    }

    if let Some(inst) = live.iter().find(|i| i.id == id) {
        return Ok(inst.clone());
    }

    if id.is_empty() {
        bail!("missing instance id (see `genji list`)");
    }

    let matches: Vec<&Instance> = live.iter().filter(|i| i.id.starts_with(id)).collect();
    match matches.as_slice() {
        [inst] => Ok((*inst).clone()),
        [] => bail!("no running genji instance with id `{id}` (see `genji list`)"),
        many => {
            let ids: Vec<&str> = many.iter().map(|i| i.id.as_str()).collect();
            bail!(
                "instance id `{id}` is ambiguous; matches: {} (use a longer prefix)",
                ids.join(", ")
            )
        }
    }
}

pub fn remove(id: &str) {
    let _ = std::fs::remove_file(dir().join(format!("{id}.json")));
}
