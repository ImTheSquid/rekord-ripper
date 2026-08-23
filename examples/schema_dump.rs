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
        other => println!("unknown mode {other}"),
    }
    Ok(())
}
