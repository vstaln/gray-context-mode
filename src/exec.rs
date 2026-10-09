//! Sandboxed command execution.
//!
//! The command runs with stdout/stderr redirected to capture files in a
//! scratch dir — nothing streams back into context. With `bwrap` on PATH
//! (and a working userns) the sandbox is a read-only host root + tmpfs /tmp
//! with only the work dir and capture dir bound writable; without it we
//! degrade to a plain child process confined to the work dir by convention.
//! `timeout -k` inside the sandbox bounds wall-clock and kills the whole
//! process group; a watchdog polls as a backstop.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use std::os::unix::process::ExitStatusExt;

/// Default wall-clock limit and the hard cap on the `timeout_ms` arg.
pub const DEFAULT_TIMEOUT_MS: u64 = 120_000;
pub const MAX_TIMEOUT_MS: u64 = 600_000;
/// Grace period after the inner `timeout` should have fired before the
/// watchdog kills the child itself.
const KILL_GRACE: Duration = Duration::from_secs(10);

/// Env vars stripped from the child — startup/module injection surfaces.
/// Subset of the context-mode denylist covering the shells/runtimes an
/// agent actually spawns.
const ENV_DENY: &[&str] = &[
    "BASH_ENV", "ENV", "PROMPT_COMMAND", "PS4", "SHELLOPTS", "BASHOPTS",
    "CDPATH", "INPUTRC", "BASH_XTRACEFD", "LD_PRELOAD", "DYLD_INSERT_LIBRARIES",
    "NODE_OPTIONS", "NODE_PATH", "PYTHONSTARTUP", "PYTHONHOME", "PYTHONINSPECT",
    "PYTHONWARNINGS", "PYTHONBREAKPOINT", "RUBYOPT", "RUBYLIB", "PERL5OPT",
    "PERL5LIB", "PERLLIB", "PERL5DB", "GOFLAGS", "CGO_CFLAGS", "CGO_LDFLAGS",
    "RUSTC", "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER", "CARGO_BUILD_RUSTC",
    "CARGO_BUILD_RUSTC_WRAPPER", "RUSTFLAGS", "OPENSSL_CONF", "OPENSSL_ENGINES",
    "CC", "CXX", "AR", "GIT_SSH", "GIT_SSH_COMMAND", "GIT_ASKPASS",
    "GIT_EXEC_PATH", "GIT_CONFIG_GLOBAL", "GIT_CONFIG_SYSTEM",
    "GIT_TEMPLATE_DIR",
];

pub struct ExecResult {
    /// Scratch dir holding `stdout.log`/`stderr.log`. Callers index the
    /// files, then `cleanup()` removes the dir.
    pub capture_dir: PathBuf,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub duration_ms: u64,
    /// "bwrap" or "cwd" — surfaced in the summary so the agent knows how
    /// strong the isolation was.
    pub sandbox: &'static str,
    pub workdir: PathBuf,
    /// True when `workdir` was our own scratch dir (safe to delete).
    pub scratch_workdir: bool,
}

impl ExecResult {
    pub fn stdout_path(&self) -> PathBuf {
        self.capture_dir.join("stdout.log")
    }
    pub fn stderr_path(&self) -> PathBuf {
        self.capture_dir.join("stderr.log")
    }
    pub fn cleanup(&self) {
        let _ = std::fs::remove_dir_all(&self.capture_dir);
        if self.scratch_workdir && self.workdir != self.capture_dir {
            let _ = std::fs::remove_dir_all(&self.workdir);
        }
    }
}

fn scratch_dir(tag: &str) -> Result<PathBuf, String> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("gray-ctx-{tag}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    Ok(dir)
}

/// Cached `bwrap --version` + preflight result. Preflight runs the real
/// bind shape so broken-userns hosts fall back to "cwd" instead of failing
/// every exec.
fn bwrap_ok() -> bool {
    static OK: OnceLock<bool> = OnceLock::new();
    *OK.get_or_init(|| {
        let probe = Command::new("bwrap")
            .args([
                "--die-with-parent",
                "--ro-bind", "/", "/",
                "--dev", "/dev",
                "--proc", "/proc",
                "--tmpfs", "/tmp",
                "--", "true",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        matches!(probe, Ok(s) if s.success())
    })
}

fn timeout_ok() -> bool {
    static OK: OnceLock<bool> = OnceLock::new();
    *OK.get_or_init(|| {
        Command::new("timeout")
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

fn scrub_env(cmd: &mut Command) {
    for key in ENV_DENY {
        cmd.env_remove(key);
    }
    for (key, _) in std::env::vars() {
        if key.starts_with("BASH_FUNC_") || key.to_ascii_uppercase().starts_with("COMPLUS_") {
            cmd.env_remove(&key);
        }
    }
    cmd.env("NO_COLOR", "1");
}

/// Run `command` through bash with stdout/stderr captured to files.
/// `cwd`: caller-requested work dir (must exist); None → fresh scratch dir.
pub fn run(
    command: &str,
    cwd: Option<&Path>,
    timeout_ms: u64,
) -> Result<ExecResult, String> {
    let timeout_ms = timeout_ms.clamp(1_000, MAX_TIMEOUT_MS);
    let capture_dir = scratch_dir("cap")?;
    let (workdir, scratch_workdir) = match cwd {
        Some(dir) => {
            let dir = dir
                .canonicalize()
                .map_err(|e| format!("cwd {}: {e}", dir.display()))?;
            if !dir.is_dir() {
                return Err(format!("cwd {} is not a directory", dir.display()));
            }
            (dir, false)
        }
        None => (scratch_dir("work")?, true),
    };
    let stdout_file = std::fs::File::create(capture_dir.join("stdout.log"))
        .map_err(|e| format!("capture stdout: {e}"))?;
    let stderr_file = std::fs::File::create(capture_dir.join("stderr.log"))
        .map_err(|e| format!("capture stderr: {e}"))?;

    let secs = format!("{}", (timeout_ms as f64 / 1000.0).ceil() as u64);
    let (sandbox, mut cmd) = if bwrap_ok() {
        let mut c = Command::new("bwrap");
        c.args([
            "--die-with-parent",
            "--ro-bind", "/", "/",
            "--dev", "/dev",
            "--proc", "/proc",
            "--tmpfs", "/tmp",
        ]);
        c.arg("--bind").arg(&workdir).arg(&workdir);
        if capture_dir != workdir {
            c.arg("--bind").arg(&capture_dir).arg(&capture_dir);
        }
        c.arg("--setenv").arg("TMPDIR").arg(&capture_dir);
        c.arg("--chdir").arg(&workdir).arg("--");
        if timeout_ok() {
            c.args(["timeout", "-k", "5", &secs, "bash", "-c", command]);
        } else {
            c.args(["bash", "-c", command]);
        }
        ("bwrap", c)
    } else {
        let mut c = if timeout_ok() {
            let mut t = Command::new("timeout");
            t.args(["-k", "5", &secs, "bash", "-c", command]);
            t
        } else {
            let mut t = Command::new("bash");
            t.args(["-c", command]);
            t
        };
        c.current_dir(&workdir);
        c.env("TMPDIR", &capture_dir);
        ("cwd", c)
    };
    scrub_env(&mut cmd);
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file))
        .spawn()
        .map_err(|e| format!("spawn: {e}"))?;

    let start = Instant::now();
    let deadline = start + Duration::from_millis(timeout_ms) + KILL_GRACE;
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() > deadline {
                    let _ = child.kill();
                    timed_out = true;
                    break child.wait().unwrap_or_else(|_| {
                        std::process::ExitStatus::from_raw(1 << 8)
                    });
                }
                std::thread::sleep(Duration::from_millis(15));
            }
            Err(e) => return Err(format!("wait: {e}")),
        }
    };
    // `timeout` reports 124; the watchdog sets the flag itself.
    timed_out = timed_out || status.code() == Some(124);
    Ok(ExecResult {
        capture_dir,
        exit_code: status.code(),
        timed_out,
        duration_ms: start.elapsed().as_millis() as u64,
        sandbox,
        workdir,
        scratch_workdir,
    })
}
