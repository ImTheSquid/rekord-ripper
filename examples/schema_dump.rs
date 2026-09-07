// Scratch: read-only survey of djmdContent, to write the insert against reality
// rather than against notes. Opens master.db and only ever SELECTs.
use anyhow::Result;
use rekord_ripper::db::MasterDb;

fn main() -> Result<()> {
    let db = MasterDb::open()?;
    let what = std::env::args().nth(1).unwrap_or_else(|| "cols".into());

    match what.as_str() {
        // Column list with types, nullability and defaults.
        "cols" => {
            let table = std::env::args()
                .nth(2)
                .unwrap_or_else(|| "djmdContent".into());
            let mut stmt = db.conn.prepare(&format!("PRAGMA table_info({table})"))?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, i64>("cid")?,
                    r.get::<_, String>("name")?,
                    r.get::<_, String>("type")?,
                    r.get::<_, i64>("notnull")?,
                    r.get::<_, Option<String>>("dflt_value")?,
                    r.get::<_, i64>("pk")?,
                ))
            })?;
            println!(
                "{:<4} {:<26} {:<14} {:<8} {:<12} pk",
                "cid", "name", "type", "notnull", "default"
            );
            for row in rows {
                let (cid, name, ty, nn, dflt, pk) = row?;
                println!(
                    "{cid:<4} {name:<26} {ty:<14} {nn:<8} {:<12} {pk}",
                    dflt.unwrap_or_else(|| "-".into())
                );
            }
        }

        // What a real locally-created row actually contains.
        "sample" => {
            let id: String = std::env::args().nth(2).unwrap_or_default();
            let sql = if id.is_empty() {
                // This device only, so the row reflects what rekordbox writes here
                // rather than what another machine synced in.
                "SELECT c.* FROM djmdContent c, djmdProperty p
                 WHERE c.DeviceID = p.DeviceID AND c.ServiceID = 0
                   AND c.rb_local_deleted = 0 AND c.FolderPath LIKE '/%'
                 ORDER BY c.created_at DESC LIMIT 1"
                    .to_string()
            } else {
                format!("SELECT * FROM djmdContent WHERE ID = '{id}'")
            };
            let mut stmt = db.conn.prepare(&sql)?;
            let names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
            let mut rows = stmt.query([])?;
            if let Some(row) = rows.next()? {
                for (i, n) in names.iter().enumerate() {
                    let v = row.get_ref(i)?;
                    let shown = match v {
                        rusqlite::types::ValueRef::Null => "NULL".to_string(),
                        rusqlite::types::ValueRef::Integer(x) => x.to_string(),
                        rusqlite::types::ValueRef::Real(x) => x.to_string(),
                        rusqlite::types::ValueRef::Text(t) => {
                            format!("{:?}", String::from_utf8_lossy(t))
                        }
                        rusqlite::types::ValueRef::Blob(b) => format!("<blob {} bytes>", b.len()),
                    };
                    println!("  {n:<26} = {shown}");
                }
            } else {
                println!("no row");
            }
        }

        // Distribution of a column, to tell defaults from real variety.
        "dist" => {
            let col = std::env::args().nth(2).expect("dist <column>");
            let sql = format!(
                "SELECT {col} AS v, COUNT(*) AS n FROM djmdContent
                 WHERE rb_local_deleted = 0 GROUP BY {col} ORDER BY n DESC LIMIT 12"
            );
            let mut stmt = db.conn.prepare(&sql)?;
            let mut rows = stmt.query([])?;
            while let Some(r) = rows.next()? {
                let v = r.get_ref(0)?;
                let n: i64 = r.get(1)?;
                println!("  {n:>6}  {v:?}");
            }
        }

        // Cloud-sync posture.
        "cloud" => {
            let mut stmt = db.conn.prepare(
                "SELECT registry_id, id_1, str_1, date_1, int_1 FROM agentRegistry
                 WHERE registry_id IN ('cloudBackupState','localUpdateCount','lastSyncTime',
                                       'agentCredentials','masterDbId')",
            )?;
            let mut rows = stmt.query([])?;
            while let Some(r) = rows.next()? {
                let id: String = r.get(0)?;
                let s: Option<String> = r.get(2)?;
                let d: Option<String> = r.get(3)?;
                let i: Option<i64> = r.get(4)?;
                println!(
                    "  {id:<20} str={} date={} int={}",
                    s.map(|v| format!("{} chars", v.len()))
                        .unwrap_or("-".into()),
                    d.unwrap_or("-".into()),
                    i.map(|v| v.to_string()).unwrap_or("-".into())
                );
            }
            for (label, sql) in [
                ("devices", "SELECT COUNT(*) FROM djmdDevice"),
                ("contentFile", "SELECT COUNT(*) FROM contentFile"),
                (
                    "serviceid2",
                    "SELECT COUNT(*) FROM djmdContent WHERE ServiceID = 2",
                ),
            ] {
                let n: i64 = db.conn.query_row(sql, [], |r| r.get(0))?;
                println!("  {label:<20} {n}");
            }
            let (dbid, devid): (Option<String>, Option<String>) =
                db.conn
                    .query_row("SELECT DBID, DeviceID FROM djmdProperty LIMIT 1", [], |r| {
                        Ok((r.get(0)?, r.get(1)?))
                    })?;
            println!("  djmdProperty         DBID={dbid:?} DeviceID={devid:?}");
        }
        // Artwork: which tables exist, what a row looks like, how many rows on
        // djmdContent actually carry an ArtworkID.
        "art" => {
            let mut stmt = db.conn.prepare(
                "SELECT name, sql FROM sqlite_master WHERE type = 'table'
                 AND (name LIKE '%rtwork%' OR name LIKE '%mage%' OR name LIKE '%Jacket%')
                 ORDER BY name",
            )?;
            let mut rows = stmt.query([])?;
            while let Some(r) = rows.next()? {
                println!(
                    "== {}\n{}\n",
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?
                );
            }
            let mut stmt = db.conn.prepare("PRAGMA table_info(djmdContent)")?;
            let cols: Vec<String> = stmt
                .query_map([], |r| r.get::<_, String>("name"))?
                .collect::<rusqlite::Result<_>>()?;
            println!(
                "  djmdContent art-ish columns: {:?}",
                cols.iter()
                    .filter(|c| {
                        let l = c.to_lowercase();
                        l.contains("art") || l.contains("image") || l.contains("jacket")
                    })
                    .collect::<Vec<_>>()
            );
            for (label, sql) in [
                ("imageFile rows", "SELECT COUNT(*) FROM imageFile"),
                (
                    "djmdAlbum with art",
                    "SELECT COUNT(ImagePath) FROM djmdAlbum",
                ),
            ] {
                match db.conn.query_row(sql, [], |r| r.get::<_, i64>(0)) {
                    Ok(n) => println!("  {label:<22} {n}"),
                    Err(e) => println!("  {label:<22} err: {e}"),
                }
            }
            let mut stmt = db.conn.prepare(
                "SELECT c.ID, c.UUID, c.ImagePath, c.AlbumID, a.UUID, a.ImagePath, c.ServiceID
                 FROM djmdContent c LEFT JOIN djmdAlbum a ON a.ID = c.AlbumID
                 WHERE c.ImagePath IS NOT NULL AND c.ImagePath != ''
                   AND c.FolderPath LIKE ?1
                 ORDER BY c.created_at DESC LIMIT 6",
            )?;
            let pattern = std::env::args().nth(2).unwrap_or_else(|| "/Users/%".into());
            let mut rows = stmt.query([&pattern])?;
            while let Some(r) = rows.next()? {
                println!("  --");
                println!("  content ID    {}", r.get::<_, String>(0)?);
                println!("  content UUID  {}", r.get::<_, String>(1)?);
                println!("  ImagePath     {}", r.get::<_, String>(2)?);
                println!("  AlbumID       {:?}", r.get::<_, Option<String>>(3)?);
                println!("  album UUID    {:?}", r.get::<_, Option<String>>(4)?);
                println!("  album Image   {:?}", r.get::<_, Option<String>>(5)?);
                println!("  ServiceID     {:?}", r.get::<_, Option<i64>>(6)?);
            }
        }
        // Why the artwork backfill finds what it finds: each condition of its
        // scan query, counted separately.
        "artscan" => {
            for (label, sql) in [
                ("local rows", "ServiceID = 0 AND FolderPath LIKE '/%'"),
                (
                    "  of those, no art",
                    "ServiceID = 0 AND FolderPath LIKE '/%'
                     AND (ImagePath IS NULL OR ImagePath = '')",
                ),
                (
                    "  of those, have art",
                    "ServiceID = 0 AND FolderPath LIKE '/%'
                     AND ImagePath IS NOT NULL AND ImagePath != ''",
                ),
            ] {
                let n: i64 = db.conn.query_row(
                    &format!(
                        "SELECT COUNT(*) FROM djmdContent
                         WHERE (rb_local_deleted = 0 OR rb_local_deleted IS NULL) AND {sql}"
                    ),
                    [],
                    |r| r.get(0),
                )?;
                println!("  {label:<22} {n}");
            }
            // The condition the scan applies in Rust, not SQL.
            let mut stmt = db.conn.prepare(
                "SELECT FolderPath FROM djmdContent
                 WHERE (rb_local_deleted = 0 OR rb_local_deleted IS NULL)
                   AND ServiceID = 0 AND FolderPath LIKE '/%'
                   AND (ImagePath IS NULL OR ImagePath = '')",
            )?;
            let paths: Vec<String> = stmt
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            let present = paths
                .iter()
                .filter(|p| std::path::Path::new(p).is_file())
                .count();
            println!("  of those, file on disk  {present} / {}", paths.len());
            for p in paths.iter().filter(|p| std::path::Path::new(p).is_file()) {
                println!("    {p}");
            }
        }
        other => println!("unknown mode {other}"),
    }
    Ok(())
}
