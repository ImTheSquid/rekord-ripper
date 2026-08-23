//! Child-process helpers. Every spawn in this crate goes through here.
//!
//! Two rules, both learned the hard way:
//!
//! 1. Never inherit stdio. A `pgrep` PID once leaked straight into a ratatui
//!    frame, so stdout and stderr are always captured or discarded, and stdin
//!    is always null so a child can never steal the terminal's input.
//! 2. `Command` has no timeout. `thread::scope` joins every thread it spawns,
//!    so a backend that hangs on a child process would hang the whole fan-out.
//!    `run_with_deadline` is what makes that impossible.

use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};

/// Poll interval while waiting on a child. Short enough to feel immediate,
/// long enough that a multi-second download costs a negligible number of wakeups.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// How much of a failing child's stderr to keep for the error message.
const STDERR_TAIL_BYTES: usize = 2048;

/// A command whose output we don't want: all three streams discarded, only the
/// exit status is meaningful.
pub fn silent(program: &str) -> Command {
    let mut cmd = Command::new(program);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd
}

/// A command whose output we intend to read. Piped, never inherited.
pub fn capture(program: &str) -> Command {
    let mut cmd = Command::new(program);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

/// True if `program` resolves to something runnable.
///
/// Runs the probe rather than walking `PATH` ourselves, so an absolute path from
/// config and a bare name on `PATH` are checked the same way.
pub fn tool_available(program: &str, version_arg: &str) -> bool {
    silent(program)
        .arg(version_arg)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Wait for `child`, killing it if `deadline` passes first.
///
/// Returns `Ok(None)` on timeout, having killed and reaped the child. The caller
/// decides what a timeout means; this only guarantees no orphan is left behind.
pub fn wait_until(child: &mut Child, deadline: Instant) -> Result<Option<std::process::ExitStatus>> {
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if Instant::now() >= deadline {
            // Kill, then wait: without the wait we leave a zombie behind.
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Run `cmd` to completion, or kill it at `deadline`.
///
/// `cmd` must already have piped stdio (use [`capture`]). Output is read after
/// the child exits, so this is only safe for commands whose output comfortably
/// fits in the pipe buffers — quiet tools invoked with `-v error` and friends.
/// For anything that streams, drive the pipes yourself.
pub fn run_with_deadline(mut cmd: Command, deadline: Instant) -> Result<Output> {
    let started = Instant::now();
    let mut child = cmd.spawn().map_err(|e| spawn_error(&cmd, e))?;

    match wait_until(&mut child, deadline)? {
        Some(status) => {
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            if let Some(mut s) = child.stdout.take() {
                s.read_to_end(&mut stdout)?;
            }
            if let Some(mut s) = child.stderr.take() {
                s.read_to_end(&mut stderr)?;
            }
            Ok(Output {
                status,
                stdout,
                stderr,
            })
        }
        None => bail!(
            "{} timed out after {:?}",
            program_name(&cmd),
            started.elapsed()
        ),
    }
}

/// The last line of a child's stderr, for an error message.
///
/// Tools like yt-dlp print a banner of warnings and then one useful `ERROR:`
/// line; that last line is the part worth showing a user.
pub fn stderr_tail(stderr: &[u8]) -> String {
    let tail_start = stderr.len().saturating_sub(STDERR_TAIL_BYTES);
    let text = String::from_utf8_lossy(&stderr[tail_start..]);
    text.lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("(no output)")
        .trim()
        .to_string()
}

/// Open a URL in the user's browser. The only outward-facing side effect in this
/// module, and it is always the result of an explicit user action.
pub fn open_url(url: &str) -> Result<()> {
    // Refuse anything that isn't plainly an http(s) URL — this string reaches a
    // shell-adjacent OS handler, and a `file://` or custom scheme has no business
    // being launched by an offer listing.
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        bail!("refusing to open a non-http(s) URL: {url}");
    }

    #[cfg(target_os = "macos")]
    let status = silent("open").arg(url).status();
    #[cfg(target_os = "windows")]
    let status = silent("cmd").args(["/C", "start", "", url]).status();

    match status {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => bail!("browser launcher exited with {s}"),
        Err(e) => Err(anyhow!("could not launch a browser: {e}")),
    }
}

fn program_name(cmd: &Command) -> String {
    Path::new(cmd.get_program())
        .file_name()
        .unwrap_or(cmd.get_program())
        .to_string_lossy()
        .into_owned()
}

fn spawn_error(cmd: &Command, e: std::io::Error) -> anyhow::Error {
    let name = program_name(cmd);
    if e.kind() == std::io::ErrorKind::NotFound {
        anyhow!("{name} not found — install it, or set its path in config.toml")
    } else {
        anyhow!("could not run {name}: {e}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stderr_tail_picks_the_last_nonempty_line() {
        let s = b"WARNING: something\nERROR: the real problem\n\n";
        assert_eq!(stderr_tail(s), "ERROR: the real problem");
    }

    #[test]
    fn stderr_tail_handles_empty_output() {
        assert_eq!(stderr_tail(b""), "(no output)");
        assert_eq!(stderr_tail(b"\n  \n"), "(no output)");
    }

    #[test]
    fn run_with_deadline_returns_output_for_a_fast_command() {
        let mut cmd = capture("echo");
        cmd.arg("hello");
        let out = run_with_deadline(cmd, Instant::now() + Duration::from_secs(5)).unwrap();
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hello");
    }

    #[test]
    fn run_with_deadline_kills_a_command_that_overruns() {
        let mut cmd = capture("sleep");
        cmd.arg("30");
        let err = run_with_deadline(cmd, Instant::now() + Duration::from_millis(200))
            .expect_err("a 30s sleep must not satisfy a 200ms deadline");
        assert!(err.to_string().contains("timed out"), "got: {err}");
    }

    #[test]
    fn missing_program_names_itself() {
        let cmd = capture("definitely-not-a-real-binary-xyzzy");
        let err = run_with_deadline(cmd, Instant::now() + Duration::from_secs(5)).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("definitely-not-a-real-binary-xyzzy"), "got: {msg}");
        assert!(msg.contains("not found"), "got: {msg}");
    }

    #[test]
    fn tool_available_agrees_with_reality() {
        assert!(tool_available("echo", "--version") || tool_available("/bin/echo", "--version"));
        assert!(!tool_available("definitely-not-a-real-binary-xyzzy", "--version"));
    }

    #[test]
    fn open_url_refuses_non_http_schemes() {
        for bad in ["file:///etc/passwd", "javascript:alert(1)", "not a url"] {
            assert!(open_url(bad).is_err(), "{bad} should be refused");
        }
    }
}
