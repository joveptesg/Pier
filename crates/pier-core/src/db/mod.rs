pub mod models;
pub mod ports;
pub mod schema;

use anyhow::Result;
use rusqlite::Connection;
use std::path::Path;

/// Open SQLite database, configure pragmas, run migrations.
pub fn init_db(path: &Path) -> Result<Connection> {
    // Ensure parent directory exists
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let conn = Connection::open(path)?;

    // `PRAGMA journal_mode` reports the mode it actually ended up in as a
    // result row, and silently keeps the old one if the switch fails —
    // execute_batch would throw that row away and we would never know.
    let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        tracing::warn!("SQLite refused WAL mode, running in '{mode}' instead");
    }

    conn.execute_batch(
        // synchronous = FULL, not NORMAL.
        //
        // In WAL mode NORMAL does not fsync on commit: the WAL is only synced
        // before a checkpoint, and with the default autocheckpoint of 1000
        // pages (~4 MB) a control-plane database this small can go hours
        // between checkpoints. Everything committed in that window sits in the
        // page cache with no durability guarantee, and an unclean host event
        // can leave a WAL whose header does not validate — which SQLite then
        // discards in full, silently, rolling the database back to the last
        // checkpoint. FULL costs one fsync per commit; for a database that
        // handles deploys rather than traffic that is not a meaningful price.
        "PRAGMA foreign_keys = ON;
         PRAGMA busy_timeout = 5000;
         PRAGMA synchronous = FULL;
         PRAGMA wal_autocheckpoint = 256;",
    )?;

    schema::run_migrations(&conn)?;

    // Ensure default network exists (fallback for partial migration)
    let _ = conn.execute(
        "INSERT OR IGNORE INTO networks (id, name, description, driver, is_default)
         VALUES ('default-pier-net', 'pier-net', 'Default network for all services', 'bridge', 1)",
        [],
    );

    // Assign existing services without network to default
    let _ = conn.execute(
        "UPDATE services SET network_id = 'default-pier-net' WHERE network_id IS NULL",
        [],
    );

    tracing::info!("Database initialized at {}", path.display());
    Ok(conn)
}

/// Count total users in the database.
pub fn user_count(conn: &Connection) -> Result<u32> {
    let count: u32 = conn.query_row("SELECT COUNT(*) FROM users", [], |row| row.get(0))?;
    Ok(count)
}
