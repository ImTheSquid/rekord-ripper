use std::env::var;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Result, anyhow, bail};
use flate2::read::ZlibDecoder;
use rusqlite::{Connection, params};

// Obfuscated SQLCipher key for rekordbox 6.x and 7.x `master.db`, byte-identical
// to the constants pyrekordbox ships. Deobfuscation: base85-decode -> XOR with
// BLOB_KEY (cycled) -> zlib-decompress. The plaintext is a 64-char hex string.
//
// Pyrekordbox refs:
//   pyrekordbox/db6/database.py:41   (BLOB)
//   pyrekordbox/utils.py:18,179      (BLOB_KEY, deobfuscate)
const BLOB: &str = "PN_Pq^*N>(JYe*u^8;Yg76HuZ<mR13S?=>)b9;DpoTXV(6ItkU`}8*m6tx_I{Solh_N#dfe{v=";
const BLOB_KEY: &[u8] = b"657f48f84c437cc1";

fn deobfuscate() -> Result<String> {
    let data = base85::decode(BLOB).map_err(|e| anyhow!("base85 decode failed: {e:?}"))?;
    let xored: Vec<u8> = data
        .iter()
        .enumerate()
        .map(|(i, b)| b ^ BLOB_KEY[i % BLOB_KEY.len()])
        .collect();
    let mut decoder = ZlibDecoder::new(&xored[..]);
    let mut out = String::new();
    decoder.read_to_string(&mut out)?;
    Ok(out)
}

fn resolve_key() -> Result<String> {
    if let Ok(k) = var("REKORDBOX_KEY") {
        let k = k.trim().to_owned();
        if !k.is_empty() {
            return Ok(k);
        }
    }
    deobfuscate()
}

pub use crate::paths::rekordbox_app_dir;

pub struct MasterDb {
    pub conn: Connection,
    pub app_dir: PathBuf,
}

impl MasterDb {
    pub fn open() -> Result<Self> {
        let app_dir = rekordbox_app_dir()?;
        let db_path = app_dir.join("master.db");
        if !db_path.exists() {
            bail!("Rekordbox master.db not found at {}", db_path.display());
        }

        let conn = Connection::open(&db_path)?;
        let key = resolve_key()?;
        conn.execute_batch(&format!("PRAGMA key = '{key}';"))?;

        // Force a read to verify decryption succeeded — PRAGMA key itself
        // returns OK even if the key is wrong; the first real read is where
        // SQLCipher fails.
        conn.query_row::<i64, _, _>("SELECT count(*) FROM sqlite_master", [], |r| r.get(0))
            .map_err(|e| {
                anyhow!(
                    "failed to decrypt master.db (key may be stale for this rekordbox version): {e}"
                )
            })?;

        Ok(Self { conn, app_dir })
    }

    /// Resolve a `djmdContent.AnalysisDataPath` value (e.g.
    /// `/PIONEER/USBANLZ/b1f/ed0f0-…/ANLZ0000.DAT`) to an absolute path on disk.
    pub fn resolve_analysis_path(&self, rel: &str) -> PathBuf {
        let stripped = rel.trim_start_matches('/');
        self.app_dir.join("share").join(stripped)
    }

    /// Read the global USN counter from `agentRegistry`.
    pub fn read_local_usn(&self) -> Result<i64> {
        self.conn
            .query_row(
                "SELECT int_1 FROM agentRegistry WHERE registry_id = 'localUpdateCount'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .map_err(|e| anyhow!("read localUpdateCount: {e}"))
    }

    /// Write the global USN counter into `agentRegistry`. Must be inside the
    /// caller's transaction.
    pub fn write_local_usn(&self, usn: i64) -> Result<()> {
        let ts = now_db_string();
        let n = self.conn.execute(
            "UPDATE agentRegistry SET int_1 = ?1, updated_at = ?2
             WHERE registry_id = 'localUpdateCount'",
            params![usn, ts],
        )?;
        if n != 1 {
            bail!("localUpdateCount row missing or updated {n} rows");
        }
        Ok(())
    }

    /// Copy `master.db` (+ wal/shm sidecars if present) into our data dir.
    /// Returns the path of the primary backup file.
    pub fn backup(&self) -> Result<PathBuf> {
        let backup_dir = backup_dir()?;
        std::fs::create_dir_all(&backup_dir)?;
        let stamp = unique_stamp(&backup_dir);

        let live = self.app_dir.join("master.db");
        let target = backup_dir.join(format!("master.db.{stamp}.bak"));
        std::fs::copy(&live, &target)?;

        for sidecar in ["master.db-wal", "master.db-shm"] {
            let src = self.app_dir.join(sidecar);
            if src.exists() {
                let dst = backup_dir.join(format!("{sidecar}.{stamp}.bak"));
                std::fs::copy(&src, &dst)?;
            }
        }
        Ok(target)
    }
}

use crate::paths::backup_dir;

/// A stamp no existing backup already uses.
///
/// Seconds were not enough. `apply_plan` backs up once per plan, so applying a
/// batch wrote several backups inside the same second, and `fs::copy` truncates
/// — leaving one file that claimed to be the pre-change copy but held the state
/// after every change but the last. Milliseconds make that unlikely; the counter
/// makes it impossible.
fn unique_stamp(dir: &std::path::Path) -> String {
    let base = chrono::Utc::now().format("%Y%m%dT%H%M%S%3fZ").to_string();
    let taken = |s: &str| dir.join(format!("master.db.{s}.bak")).exists();
    if !taken(&base) {
        return base;
    }
    (1..)
        .map(|n| format!("{base}-{n}"))
        .find(|s| !taken(s))
        .unwrap_or(base)
}

/// True if a `rekordbox` process is currently running.
pub fn rekordbox_running() -> bool {
    #[cfg(target_os = "macos")]
    {
        // pgrep prints matching PIDs to stdout; with status() that inherits the
        // terminal and leaks the PID into the TUI. We only need the exit status.
        Command::new("pgrep")
            .args(["-x", "rekordbox"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
    #[cfg(target_os = "windows")]
    {
        // tasklist prints a header even with /NH unless filter matches nothing.
        let out = Command::new("tasklist")
            .args(["/FI", "IMAGENAME eq rekordbox.exe", "/NH"])
            .output();
        match out {
            Ok(o) => String::from_utf8_lossy(&o.stdout).contains("rekordbox.exe"),
            Err(_) => false,
        }
    }
}

#[derive(Clone, Copy, Default)]
pub struct SafetyOpts {
    /// Bypass the "rekordbox is running" hard refuse. Mapped to the explicit
    /// `--i-know-rekordbox-is-open-and-may-corrupt-my-data` flag.
    pub bypass_rekordbox_check: bool,
}

/// Pre-flight checks every mutating run must pass.
pub fn safety_preflight(opts: SafetyOpts) -> Result<()> {
    if rekordbox_running() && !opts.bypass_rekordbox_check {
        bail!(
            "rekordbox is running — refusing to write to master.db. \
             Close rekordbox, or pass \
             --i-know-rekordbox-is-open-and-may-corrupt-my-data to proceed."
        );
    }
    Ok(())
}

/// Timestamp string in the format master.db already uses, e.g.
/// `2026-06-03 01:02:54.026 +00:00`.
pub fn now_db_string() -> String {
    chrono::Utc::now()
        .format("%Y-%m-%d %H:%M:%S%.3f +00:00")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn back_to_back_backups_never_share_a_name() {
        // `apply_plan` backs up once per plan, so a batch of transfers took
        // several backups inside one second. With a seconds-only stamp they all
        // resolved to one filename and `fs::copy` truncated, leaving a single
        // file that claimed to predate changes it actually contained.
        let dir = std::env::temp_dir().join(format!("rr-stamp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let mut stamps = Vec::new();
        for _ in 0..5 {
            let s = unique_stamp(&dir);
            // What `backup` does next, and what makes the stamp taken.
            std::fs::write(dir.join(format!("master.db.{s}.bak")), b"x").unwrap();
            stamps.push(s);
        }

        let mut unique = stamps.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), stamps.len(), "collided: {stamps:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
