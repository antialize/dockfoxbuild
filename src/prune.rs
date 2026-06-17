//! Prune old buildah images and containers to free up disk space.
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::{collections::HashMap, process::Stdio};

use crate::{duration::Duration, size::Size};

/// Arguments for the prune command, including age thresholds and cache size limits.
#[derive(clap::Parser)]
pub struct PruneArgs {
    /// Minimum age of images before they may be pruned (e.g. "6h", "2d"). Images newer than this are never pruned.
    #[arg(long, default_value = "6h")]
    min_age: Duration,
    /// Maximum age of images to keep (e.g. "2w", "1m"). Images older than this are pruned regardless of cache size.
    #[arg(long, default_value = "2w")]
    max_age: Duration,
    /// Maximum cache size (e.g. "100GB", "512MB"). Older images are pruned to maintain this limit.
    #[arg(long, default_value = "100GB")]
    max_cache_size: Size,
    /// If set, the prune command will run in quiet mode and only output summary information.
    #[arg(long, short)]
    quiet: bool,
}

/// Information about a container from `buildah ps --json`.
#[derive(Debug, Deserialize)]
struct ContainerPsInfo {
    /// The container ID.
    id: String,
    /// Whether this container is a builder container (i.e., created by `buildah from`).
    builder: bool,
}

/// Information about a container from `buildah inspect --json`.
#[derive(Debug, Deserialize)]
struct ContainerOCIV1 {
    /// The creation time of the container, in RFC3339 format.
    created: String,
}

/// Information about a container from `buildah inspect --json`.
#[derive(Debug, Deserialize)]
struct ContainerInspect {
    /// The OCIv1-specific information about the container, including creation time.
    #[serde(rename = "OCIv1")]
    ociv1: ContainerOCIV1,
}

/// Information about an image from `buildah images --json`, including ID, size, and creation time.
#[derive(Debug, Deserialize)]
struct ImageInfo {
    /// The image ID, typically a sha256 hash.
    id: String,
    /// The size of the image as a human-readable string (e.g., "123 MB").
    size: Size,
    /// The creation time of the image as a raw string in RFC3339 format.
    createdatraw: String,
}

/// A simplified struct representing image information, including ID, size in bytes, creation time as a timestamp, and associated checkpoints.
#[derive(Debug)]
struct ImageInfo2 {
    /// The image ID, typically a sha256 hash without the "sha256:" prefix.
    id: String,
    /// The size of the image in bytes.
    size: u64,
    /// The creation time of the image as a Unix timestamp (seconds since epoch).
    time: u64,
    /// A list of checkpoint hashes associated with this image, used for caching purposes.
    checkpoints: Vec<String>,
}

/// Prune old buildah images and containers based on age and cache size limits.
pub fn prune(args: PruneArgs) -> Result<()> {
    // Find and kill stray containers
    let output = std::process::Command::new("buildah")
        .args(["ps", "--json"])
        .stderr(Stdio::inherit())
        .output()?;
    if !output.status.success() {
        bail!("Failed to list containers: {}", output.status);
    }
    let containers: Vec<ContainerPsInfo> =
        serde_json::from_slice(&output.stdout).context("Listing containers")?;

    for container in containers {
        if !container.builder {
            continue;
        }

        // Inspect container to get creation time
        let output = std::process::Command::new("buildah")
            .args(["inspect", &container.id])
            .stderr(Stdio::inherit())
            .output()?;
        if !output.status.success() {
            bail!(
                "Failed to inspect container {}: {}",
                container.id,
                output.status
            );
        }
        let inspect: ContainerInspect =
            serde_json::from_slice(&output.stdout).context("Inspecting container")?;
        let created = chrono::DateTime::parse_from_rfc3339(&inspect.ociv1.created)
            .context("Parsing container creation time")?;
        let age = chrono::Utc::now().signed_duration_since(created);
        if age < chrono::Duration::hours(1) {
            continue;
        }

        println!("Pruning container {} (age: {})", container.id, age);
        let status = std::process::Command::new("buildah")
            .args(["rm", &container.id])
            .stderr(Stdio::inherit())
            .status()?;
        if !status.success() {
            bail!("Failed to remove container {}: {}", container.id, status);
        }
    }

    // List images known by buildah
    let output = std::process::Command::new("buildah")
        .args(["images", "--no-trunc", "--json"])
        .stderr(Stdio::inherit())
        .output()?;
    if !output.status.success() {
        bail!("Failed to list images: {}", output.status);
    }
    let images: Vec<ImageInfo> =
        serde_json::from_slice(&output.stdout).context("Listing images")?;
    let mut images_by_id = HashMap::new();
    for image in images {
        let time = chrono::DateTime::parse_from_rfc3339(&image.createdatraw)
            .context("Parsing image creation time")?
            .timestamp() as u64;
        let size = image.size.0;
        let id = image
            .id
            .strip_prefix("sha256:")
            .context("Missing sha256")?
            .to_string();
        images_by_id.insert(
            id.clone(),
            ImageInfo2 {
                id,
                size,
                time,
                checkpoints: Vec::new(),
            },
        );
    }

    let db = crate::db::connect_db()?;
    let mut stmt = db.prepare("SELECT buildah_id, last_used_at FROM remote_images")?;

    let remote_images = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?
        .filter_map(|res| res.ok())
        .collect::<Vec<_>>();

    for (id, last_used) in remote_images {
        if let Some(info) = images_by_id.get_mut(&id) {
            info.time = u64::max(info.time, last_used as u64);
        } else {
            db.execute(
                "DELETE FROM remote_images WHERE buildah_id = ?1",
                rusqlite::params![id],
            )?;
        }
    }

    let mut stmt =
        db.prepare("SELECT checkpoint_hash, buildah_id, last_used_at FROM checkpoint_cache")?;

    let checkpoints = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?
        .filter_map(|res| res.ok())
        .collect::<Vec<_>>();

    for (checkpoint_hash, buildah_id, last_used_at) in checkpoints {
        if let Some(info) = images_by_id.get_mut(&buildah_id) {
            info.time = u64::max(info.time, last_used_at as u64);
            info.checkpoints.push(checkpoint_hash.clone());
        } else {
            db.execute(
                "DELETE FROM checkpoint_cache WHERE checkpoint_hash = ?1",
                rusqlite::params![checkpoint_hash],
            )?;
        }
    }

    let mut images: Vec<_> = images_by_id.values().collect();
    images.sort_by_key(|info| info.time);
    let sum = images.iter().map(|info| info.size).sum::<u64>();
    let mut freed = 0;

    let now = chrono::Utc::now().timestamp() as u64;
    let mut removed_count = 0;
    let mut failure_count = 0;
    let mut bt = now;
    for info in &images {
        let age = now.saturating_sub(info.time);
        // Never prune images younger than min_age. Since images are sorted oldest
        // first, every remaining image is also younger, so we can stop entirely.
        if age < args.min_age.0 {
            bt = info.time;
            break;
        }
        // Images older than max_age are always pruned. Otherwise only keep pruning
        // while we are still over the max cache size.
        let over_max_age = age > args.max_age.0;
        let over_size = (sum - freed) > args.max_cache_size.0;
        if !over_max_age && !over_size {
            bt = info.time;
            break;
        }
        let s = std::process::Command::new("buildah")
            .args(["rmi", &info.id])
            .stderr(Stdio::null())
            .stdout(Stdio::null())
            .status()
            .with_context(|| format!("Failed to remove image {}", info.id))?;
        if s.success() {
            freed += info.size;
            removed_count += 1;
        } else {
            failure_count += 1;
        }
    }

    if !args.quiet {
        println!(
            "Freed {} out of {}, oldest image age: {}; {} total images, removed: {}, failed: {}",
            Size(freed),
            Size(sum),
            Duration(now - bt),
            images.len(),
            removed_count,
            failure_count
        );
    }
    Ok(())
}
