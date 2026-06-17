//! Provides functionality to connect to a SQLite database for caching build checkpoints and remote image metadata
//! The database is stored in the user's cache directory following the XDG Base Directory Specification.
use anyhow::{Context, Result};
use std::{env, path::PathBuf};

/// Connects to the SQLite database for caching build checkpoints and remote image metadata.
/// The database is stored in the user's cache directory following the XDG Base Directory Specification.
pub fn connect_db() -> Result<rusqlite::Connection> {
    // 1. Check for XDG_CACHE_HOME, fallback to $HOME/.cache
    let mut cache_path = if let Ok(xdg_cache) = env::var("XDG_CACHE_HOME") {
        PathBuf::from(xdg_cache)
    } else {
        let home = env::var("HOME").context("Neither $XDG_CACHE_HOME nor $HOME is set")?;
        PathBuf::from(home).join(".cache")
    };

    // 2. Append your custom client directory and database file name
    cache_path.push("dockfoxbuild");
    std::fs::create_dir_all(&cache_path).context("Failed to create cache directory")?;
    cache_path.push("cache.db");
    let conn = rusqlite::Connection::open(cache_path)?;
    conn.execute_batch(
        "
        PRAGMA journal_mode=WAL;
        PRAGMA synchronous=NORMAL;

        CREATE TABLE IF NOT EXISTS checkpoint_cache (
            checkpoint_hash TEXT PRIMARY KEY,
            buildah_id TEXT NOT NULL,
            created_at INTEGER DEFAULT (unixepoch()),
            last_used_at INTEGER DEFAULT (unixepoch())
        ) STRICT;

        CREATE TABLE IF NOT EXISTS remote_images (
            buildah_id TEXT PRIMARY KEY,
            last_used_at INTEGER DEFAULT (unixepoch())
        ) STRICT;

        CREATE INDEX IF NOT EXISTS idx_last_used ON checkpoint_cache(last_used_at);
        ",
    )?;
    Ok(conn)
}

/// Generates a cache image name based on the given hash, following the format "localhost/dockfoxbuild_cache:{hash}".
pub fn cache_image_name(hash: &str) -> String {
    format!("localhost/dockfoxbuild_cache:{}", hash)
}
