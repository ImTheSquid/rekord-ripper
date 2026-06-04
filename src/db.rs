use std::env::var;
use std::io::Read;
use std::path::PathBuf;

use anyhow::{Result, anyhow, bail};
use flate2::read::ZlibDecoder;
use rusqlite::Connection;

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

pub fn rekordbox_app_dir() -> Result<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        var("APPDATA")
            .map(|appdata| PathBuf::from(appdata).join("Pioneer/rekordbox"))
            .map_err(|_| anyhow!("APPDATA env var not found"))
    }
    #[cfg(target_os = "macos")]
    {
        var("HOME")
            .map(|home| PathBuf::from(home).join("Library/Pioneer/rekordbox"))
            .map_err(|_| anyhow!("HOME env var not found"))
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    compile_error!("Rekordbox only runs on macOS and Windows.");
}

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
}
