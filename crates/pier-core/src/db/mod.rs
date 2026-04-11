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

    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA foreign_keys = ON;
         PRAGMA busy_timeout = 5000;
         PRAGMA synchronous = NORMAL;",
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
