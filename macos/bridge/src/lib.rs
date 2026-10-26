use rekord_ripper::db::MasterDb;
use rekord_ripper::library::load_rows;

#[swift_bridge::bridge]
mod ffi {
    extern "Rust" {
        type Session;

        #[swift_bridge(associated_to = Session)]
        fn open() -> Result<Session, String>;
        fn row_count(&self) -> usize;
    }
}

pub struct Session {
    rows: usize,
}

impl Session {
    fn open() -> Result<Session, String> {
        let db = MasterDb::open().map_err(|e| format!("{e:#}"))?;
        let rows = load_rows(&db).map_err(|e| format!("{e:#}"))?.len();
        Ok(Session { rows })
    }

    fn row_count(&self) -> usize {
        self.rows
    }
}
