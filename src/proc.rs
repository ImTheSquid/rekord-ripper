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

use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{OnceLock, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};

/// Poll interval while waiting on a child. Short enough to feel immediate,
/// long enough that a multi-second download costs a negligible number of wakeups.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// How much of a failing child's stderr to keep for the error message.
const STDERR_TAIL_BYTES: usize = 2048;

/// `PATH` with the usual package-manager folders after it, for finding a tool
/// and for whatever that tool runs in turn.
///
/// An app opened from the Finder inherits launchd's bare `PATH`, which finds
/// none of ffmpeg, ffprobe or yt-dlp. Appended, so the shell's order still wins.
fn search_path() -> &'static OsStr {
    static PATH: OnceLock<OsString> = OnceLock::new();
    PATH.get_or_init(|| {
        with_usual_dirs(
            std::env::var_os("PATH"),
            std::env::var_os("HOME").map(PathBuf::from),
            std::env::var("USER").ok(),
        )
    })
}

fn with_usual_dirs(
    path: Option<OsString>,
    home: Option<PathBuf>,
    user: Option<String>,
) -> OsString {
    let mut dirs: Vec<PathBuf> = path
        .as_deref()
        .map(|p| std::env::split_paths(p).collect())
        .unwrap_or_default();
    if cfg!(unix) {
        let mut usual: Vec<PathBuf> = [
            "/opt/homebrew/bin",
            "/usr/local/bin",
            "/opt/local/bin",
            "/run/current-system/sw/bin",
            "/nix/var/nix/profiles/default/bin",
        ]
        .map(PathBuf::from)
        .into();
        if let Some(user) = user {
            usual.push(format!("/etc/profiles/per-user/{user}/bin").into());
        }
        if let Some(home) = home {
            usual.push(home.join(".nix-profile/bin"));
            usual.push(home.join(".local/bin"));
        }
        for dir in usual {
            if !dirs.contains(&dir) {
                dirs.push(dir);
            }
        }
    }
    std::env::join_paths(dirs).unwrap_or_else(|_| path.unwrap_or_default())
}

/// Configured paths for tools spawned by bare name.
static TOOLS: RwLock<Vec<(&'static str, PathBuf)>> = RwLock::new(Vec::new());

/// Spawns of `ffmpeg` and `ffprobe` from here on use these paths.
pub fn use_tools(tools: &crate::config::Tools) {
    if let Ok(mut set) = TOOLS.write() {
        *set = configured(tools);
    }
}

fn configured(tools: &crate::config::Tools) -> Vec<(&'static str, PathBuf)> {
    let mut set = Vec::new();
    for (name, path) in [
        ("ffmpeg", &tools.ffmpeg_path),
        ("ffprobe", &tools.ffprobe_path),
    ] {
        let path = path.trim();
        if !path.is_empty()
            && path != name
            && let Ok(p) = crate::paths::expand_tilde(path)
        {
            set.push((name, p));
        }
    }
    set
}

fn resolve(program: &str) -> OsString {
    TOOLS
        .read()
        .ok()
        .and_then(|set| resolve_in(&set, program))
        .unwrap_or_else(|| program.into())
}

fn resolve_in(set: &[(&str, PathBuf)], program: &str) -> Option<OsString> {
    set.iter()
        .find(|(n, _)| *n == program)
        .map(|(_, p)| p.clone().into_os_string())
}

/// A command whose output we don't want: all three streams discarded, only the
/// exit status is meaningful.
pub fn silent(program: &str) -> Command {
    let mut cmd = Command::new(resolve(program));
    cmd.env("PATH", search_path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd
}

/// A command whose output we intend to read. Piped, never inherited.
pub fn capture(program: &str) -> Command {
    let mut cmd = Command::new(resolve(program));
    cmd.env("PATH", search_path())
        .stdin(Stdio::null())
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
pub fn wait_until(
    child: &mut Child,
    deadline: Instant,
) -> Result<Option<std::process::ExitStatus>> {
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

/// Names a configured path in full, since that is what was wrong.
pub fn not_found(cmd: &Command) -> anyhow::Error {
    let program = Path::new(cmd.get_program());
    if program.components().count() > 1 {
        anyhow!(
            "{} not found — check its path in config.toml",
            program.display()
        )
    } else {
        anyhow!(
            "{} not found — install it, or set its path in config.toml",
            program.display()
        )
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
        not_found(cmd)
    } else {
        anyhow!("could not run {name}: {e}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_configured_tool_path_replaces_the_name_and_is_named_when_missing() {
        let set = configured(&crate::config::Tools {
            ffmpeg_path: "/nonexistent/bin/ffmpeg".into(),
            ffprobe_path: "ffprobe".into(),
        });
        assert_eq!(
            resolve_in(&set, "ffmpeg").unwrap(),
            "/nonexistent/bin/ffmpeg"
        );
        assert_eq!(
            resolve_in(&set, "ffprobe"),
            None,
            "the default name is not an override"
        );

        let cmd = Command::new("/nonexistent/bin/ffmpeg");
        let err = run_with_deadline(cmd, Instant::now() + Duration::from_secs(5)).unwrap_err();
        assert!(
            format!("{err:#}").contains("/nonexistent/bin/ffmpeg not found — check its path"),
            "{err:#}"
        );
    }

    #[test]
    fn the_usual_dirs_follow_the_inherited_path() {
        let path = with_usual_dirs(
            Some("/usr/bin:/bin:/opt/homebrew/bin".into()),
            Some("/Users/me".into()),
            Some("me".into()),
        );
        let dirs: Vec<PathBuf> = std::env::split_paths(&path).collect();
        assert_eq!(
            dirs[..3],
            [
                PathBuf::from("/usr/bin"),
                "/bin".into(),
                "/opt/homebrew/bin".into()
            ]
        );
        assert_eq!(
            dirs.iter().filter(|d| d.ends_with("homebrew/bin")).count(),
            1
        );
        assert!(dirs.contains(&"/Users/me/.nix-profile/bin".into()));
        assert!(dirs.contains(&"/etc/profiles/per-user/me/bin".into()));
    }

    #[test]
    fn a_tool_is_found_on_the_path_given_to_the_command() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("rr-path-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let tool = dir.join("rr-path-probe");
        std::fs::write(&tool, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        let ran = Command::new("rr-path-probe").env("PATH", &dir).status();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(ran.unwrap().success());
    }

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
        assert!(
            msg.contains("definitely-not-a-real-binary-xyzzy"),
            "got: {msg}"
        );
        assert!(msg.contains("not found"), "got: {msg}");
    }

    #[test]
    fn tool_available_agrees_with_reality() {
        assert!(tool_available("echo", "--version") || tool_available("/bin/echo", "--version"));
        assert!(!tool_available(
            "definitely-not-a-real-binary-xyzzy",
            "--version"
        ));
    }

    #[test]
    fn open_url_refuses_non_http_schemes() {
        for bad in ["file:///etc/passwd", "javascript:alert(1)", "not a url"] {
            assert!(open_url(bad).is_err(), "{bad} should be refused");
        }
    }
}
