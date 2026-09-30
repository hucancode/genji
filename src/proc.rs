use anyhow::{Context, Result};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
pub struct ProcResult {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    pub duration_ms: u128,
}

fn tmp_path(dir: &Path, tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    dir.join(format!(".{tag}-{}-{n}.tmp", std::process::id()))
}

fn read_capped(path: &Path, cap: usize) -> String {
    let Ok(f) = File::open(path) else {
        return String::new();
    };
    let mut buf = Vec::new();
    let _ = f.take(cap as u64).read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).to_string()
}

/// Run a program, capturing stdout/stderr to temp files and enforcing a
/// timeout. This avoids pipe-buffer deadlocks and bounds memory.
pub fn run_capture(
    program: &str,
    args: &[String],
    cwd: &Path,
    timeout: Duration,
    stdin_data: Option<&str>,
    max_read_bytes: usize,
) -> Result<ProcResult> {
    let tmpdir = cwd.join(".genji").join("tmp");
    std::fs::create_dir_all(&tmpdir).ok();
    let out_path = tmp_path(&tmpdir, "out");
    let err_path = tmp_path(&tmpdir, "err");
    let out_file = File::create(&out_path).context("creating stdout temp")?;
    let err_file = File::create(&err_path).context("creating stderr temp")?;

    let stdin = match stdin_data {
        Some(data) => {
            let sp = tmp_path(&tmpdir, "in");
            std::fs::write(&sp, data)?;
            Stdio::from(File::open(&sp)?)
        }
        None => Stdio::null(),
    };

    let start = Instant::now();
    let mut child = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .stdin(stdin)
        .stdout(Stdio::from(out_file))
        .stderr(Stdio::from(err_file))
        .spawn()
        .with_context(|| format!("spawning `{program}`"))?;

    let mut timed_out = false;
    let poll = Duration::from_millis(25);
    let code = loop {
        if let Some(status) = child.try_wait()? {
            break status.code();
        }
        if start.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            timed_out = true;
            break None;
        }
        std::thread::sleep(poll);
    };

    let stdout = read_capped(&out_path, max_read_bytes);
    let stderr = read_capped(&err_path, max_read_bytes);
    let _ = std::fs::remove_file(&out_path);
    let _ = std::fs::remove_file(&err_path);

    Ok(ProcResult {
        code,
        stdout,
        stderr,
        timed_out,
        duration_ms: start.elapsed().as_millis(),
    })
}

/// Convenience for running a shell command string via `bash -c`.
pub fn run_bash(
    command: &str,
    cwd: &Path,
    timeout: Duration,
    max_read_bytes: usize,
) -> Result<ProcResult> {
    run_capture(
        "bash",
        &["-c".to_string(), command.to_string()],
        cwd,
        timeout,
        None,
        max_read_bytes,
    )
}
