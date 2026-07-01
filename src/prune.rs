//! Prune old buildah images and containers to free up disk space.
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::{collections::HashMap, process::Stdio};

use crate::{duration::Duration, size::Size};

/// Outcome of attempting to delete an image during pruning.
enum RemoveOutcome {
    /// Every tag was removed (or the image had none), so the image was freed.
    Removed,
    /// The image is still referenced by a container and was left in place.
    InUse,
    /// Removal failed for some other reason.
    Failed,
}

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
    /// If set, only output on error (suitable for cron jobs).
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
    /// The names (tags) attached to the image. May be absent/null when the image is dangling.
    #[serde(default)]
    names: Vec<String>,
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
    /// The names (tags) attached to the image.
    names: Vec<String>,
    /// The size of the image in bytes.
    size: u64,
    /// The creation time of the image as a Unix timestamp (seconds since epoch).
    time: u64,
    /// A list of checkpoint hashes associated with this image, used for caching purposes.
    checkpoints: Vec<String>,
}

/// Run a command, always capturing stdout. In quiet mode, captures stderr and prints it to
/// stderr on failure. In non-quiet mode, streams stderr directly to the terminal.
fn run_command(cmd: &mut std::process::Command, quiet: bool) -> Result<std::process::Output> {
    let output = cmd
        .stdout(Stdio::piped())
        .stderr(if quiet {
            Stdio::piped()
        } else {
            Stdio::inherit()
        })
        .output()?;
    if quiet && !output.status.success() && !output.stderr.is_empty() {
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
    }
    Ok(output)
}

/// Delete an image by removing each of its tags (or by ID if it is untagged).
///
/// Removal is always done by tag rather than by ID: deleting the final tag deletes the
/// image without `--force`, so if a concurrent build has since created a container from
/// or added a new tag to this image ID, the deletion fails gracefully instead of yanking
/// the image out from under the running build.
///
/// An "image is in use by a container" failure is expected (the image is legitimately in
/// use) and is reported as [`RemoveOutcome::InUse`] without printing anything. Any other
/// failure prints buildah's stderr and is reported as [`RemoveOutcome::Failed`].
fn remove_image(info: &ImageInfo2) -> Result<RemoveOutcome> {
    let targets: Vec<&str> = if info.names.is_empty() {
        vec![info.id.as_str()]
    } else {
        info.names.iter().map(String::as_str).collect()
    };
    let mut all_ok = true;
    let mut in_use = false;
    for target in targets {
        let output = std::process::Command::new("buildah")
            .args(["rmi", target])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .with_context(|| format!("Failed to run buildah rmi {}", target))?;
        if output.status.success() {
            continue;
        }
        all_ok = false;
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("image is in use by a container") {
            in_use = true;
        } else if !stderr.is_empty() {
            eprint!("{}", stderr);
        }
    }
    Ok(if all_ok {
        RemoveOutcome::Removed
    } else if in_use {
        RemoveOutcome::InUse
    } else {
        RemoveOutcome::Failed
    })
}

/// Prune old buildah images and containers based on age and cache size limits.
pub fn prune(args: PruneArgs) -> Result<()> {
    // Find and kill stray containers
    // `buildah ps` can hang if buildah/podman is in a bad state, so enforce a timeout.
    let mut child = std::process::Command::new("buildah")
        .args(["ps", "--json"])
        .stderr(if args.quiet {
            Stdio::piped()
        } else {
            Stdio::inherit()
        })
        .stdout(Stdio::piped())
        .spawn()?;
    // Drain stderr in a thread so it doesn't block the pipe buffer while we poll.
    let stderr_rx = args.quiet.then(|| {
        let mut stderr_pipe = child.stderr.take().expect("stderr piped");
        let (tx, rx) = crossbeam::channel::bounded::<Vec<u8>>(1);
        std::thread::spawn(move || {
            use std::io::Read;
            let mut buf = Vec::new();
            let _ = stderr_pipe.read_to_end(&mut buf);
            let _ = tx.send(buf);
        });
        rx
    });
    // Read stdout in a thread so it doesn't block the pipe buffer while we poll.
    let mut stdout_pipe = child.stdout.take().expect("stdout piped");
    let (tx, rx) = crossbeam::channel::bounded::<std::io::Result<Vec<u8>>>(1);
    std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = Vec::new();
        let _ = tx.send(stdout_pipe.read_to_end(&mut buf).map(|_| buf));
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
    let status = loop {
        match child.try_wait().context("Waiting for buildah ps")? {
            Some(status) => break status,
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                bail!("buildah ps timed out after 30s");
            }
            None => std::thread::sleep(std::time::Duration::from_millis(100)),
        }
    };
    if !status.success() {
        if let Some(stderr_rx) = stderr_rx
            && let Ok(buf) = stderr_rx.recv()
            && !buf.is_empty()
        {
            eprint!("{}", String::from_utf8_lossy(&buf));
        }
        bail!("Failed to list containers: {}", status);
    }
    let stdout = rx.recv().unwrap().context("Reading buildah ps output")?;
    let containers: Vec<ContainerPsInfo> =
        serde_json::from_slice::<Option<Vec<ContainerPsInfo>>>(&stdout)
            .context("Listing containers")?
            .unwrap_or_default();

    for container in containers {
        if !container.builder {
            continue;
        }

        // Inspect container to get creation time
        let output = run_command(
            std::process::Command::new("buildah").args(["inspect", &container.id]),
            args.quiet,
        )?;
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

        if !args.quiet {
            println!("Pruning container {} (age: {})", container.id, age);
        }
        let output = run_command(
            std::process::Command::new("buildah").args(["rm", &container.id]),
            args.quiet,
        )?;
        if !output.status.success() {
            bail!(
                "Failed to remove container {}: {}",
                container.id,
                output.status
            );
        }
    }

    // List images known by buildah
    let output = run_command(
        std::process::Command::new("buildah").args(["images", "--no-trunc", "--json"]),
        args.quiet,
    )?;
    if !output.status.success() {
        bail!("Failed to list images: {}", output.status);
    }
    let images: Vec<ImageInfo> = serde_json::from_slice::<Option<Vec<ImageInfo>>>(&output.stdout)
        .context("Listing images")?
        .unwrap_or_default();
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
                names: image.names,
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
    let mut in_use_count = 0;
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
        match remove_image(info)? {
            RemoveOutcome::Removed => {
                freed += info.size;
                removed_count += 1;
            }
            RemoveOutcome::InUse => in_use_count += 1,
            RemoveOutcome::Failed => failure_count += 1,
        }
    }

    if !args.quiet {
        println!(
            "Freed {} out of {}, oldest image age: {}; {} total images, removed: {}, in use: {}, failed: {}",
            Size(freed),
            Size(sum),
            Duration(now - bt),
            images.len(),
            removed_count,
            in_use_count,
            failure_count
        );
    }
    Ok(())
}
