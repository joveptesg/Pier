pub mod executor;
pub mod restore;
pub mod scheduler;

/// A database dump above this size is flagged as large: DB backups are held
/// in the panel's memory while they upload, so a base that keeps growing
/// past this point risks a failed backup. The UI warns about such backups
/// (exposed to templates as `large_backup_bytes`).
pub const LARGE_BACKUP_BYTES: u64 = 300 * 1024 * 1024;

/// Log a warning when a finished dump crosses [`LARGE_BACKUP_BYTES`].
pub fn warn_if_large(service: &str, database: Option<&str>, size: u64) {
    if size > LARGE_BACKUP_BYTES {
        tracing::warn!(
            service,
            database = database.unwrap_or("<all>"),
            size_mb = size / (1024 * 1024),
            "large database backup: dumps are held in memory while uploading, a growing database may make the backup fail"
        );
    }
}
