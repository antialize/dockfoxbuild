//! Advisory locks that mark buildah containers and images as owned by a running build.
//!
//! Builds and `prune` share a single buildah storage, so prune needs to tell a resource
//! belonging to a live build from one left behind by a build that crashed. Age is not a
//! usable signal: `buildah inspect` reports the *base image's* creation time for a builder
//! container, so a freshly created container can look arbitrarily old.
//!
//! Instead a build holds an exclusive lock on a file named after each resource it depends
//! on, for as long as it depends on it. The kernel releases the lock when the owning
//! process dies, so prune can simply try to take the lock to learn whether the owner is
//! still alive, with no PID tracking and nothing to reap after a crash.
use anyhow::{Context, Result};
use std::{
    collections::HashSet,
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

/// Prefix identifying containers created by this tool.
pub const CONTAINER_PREFIX: &str = "dockfoxbuild-";

/// Path to the directory where lock files are stored, creating it if necessary.
pub fn lock_dir() -> Result<PathBuf> {
    let dir = crate::db::cache_dir()?.join("locks");
    std::fs::create_dir_all(&dir).context("Failed to create lock directory")?;
    Ok(dir)
}

fn container_lock_path(lock_dir: &Path, name: &str) -> Result<PathBuf> {
    Ok(lock_dir.join(format!("container_{}.lock", name)))
}

fn image_lock_path(lock_dir: &Path, id: &str) -> Result<PathBuf> {
    Ok(lock_dir.join(format!("image_{}.lock", id)))
}

/// Build a container name that no concurrent build can collide with.
///
/// buildah derives a default name from the image reference, so every build using the same
/// base image contends for one name in the shared storage namespace. Naming containers
/// ourselves also lets prune recognise which containers are ours.
fn new_container_name() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!(
        "{}{}-{}-{}",
        CONTAINER_PREFIX,
        std::process::id(),
        nanos,
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// An exclusive or shared lock marking a container or image as in use. Releasing it is what tells
/// prune the resource may be collected, so it is held until the build is genuinely done
/// with it.
#[derive(Debug)]
pub struct Lock {
    name: String,
    path: PathBuf,
    /// `None` once released; dropping the file is what unlocks it.
    file: Option<File>,
    shared: bool,
}

impl Lock {
    fn open(path: &Path) -> Result<File> {
        OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)
            .with_context(|| format!("Failed to open lock file {}", path.display()))
    }

    /// Claim a container. Must be called *before* the container is created, so prune can
    /// never observe an unowned container that is about to be used.
    ///
    /// Container names are unique per build, so nothing else contends for this lock and it
    /// is taken exclusively.
    pub fn new_container(lock_dir: &Path) -> Result<Self> {
        let name = new_container_name();
        let path = container_lock_path(lock_dir, &name)?;
        let file = Self::open(&path)?;
        // Blocking rather than try_lock: prune takes this lock briefly to test ownership,
        // and failing the build over that momentary overlap would be a spurious error.
        file.lock()
            .with_context(|| format!("Failed to lock {}", path.display()))?;
        Ok(Self {
            name,
            path,
            file: Some(file),
            shared: false,
        })
    }

    /// Claim `name` if no live build owns it, for prune to remove the container under.
    ///
    /// The lock must be *held* across the removal: testing it and releasing would let a
    /// build claim the name and create its container in the gap, and prune would then
    /// delete that container, and its lock file, out from under the running build.
    pub fn try_container(lock_dir: &Path, name: &str) -> Result<Option<Self>> {
        let path = container_lock_path(lock_dir, name)?;
        let file = Self::open(&path)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self {
                name: name.to_string(),
                path,
                file: Some(file),
                shared: false,
            })),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(e)) => {
                Err(e).with_context(|| format!("Failed to test lock {}", path.display()))
            }
        }
    }

    /// Claim an image. Must be called *before* the build relies on the image existing, so a
    /// stale cache entry is detected as a miss rather than vanishing mid-build.
    ///
    /// Held as a shared lock: any number of concurrent builds may legitimately depend on the
    /// same cached image, and all of them need to keep prune away from it.
    pub fn image(lock_dir: &Path, id: &str) -> Result<Self> {
        let path = image_lock_path(lock_dir, id)?;
        let file = Self::open(&path)?;
        file.lock_shared()
            .with_context(|| format!("Failed to lock {}", path.display()))?;
        Ok(Self {
            name: id.to_string(),
            path,
            file: Some(file),
            shared: true,
        })
    }

    /// Claim image `id` if no live build depends on it, for prune to remove the image under.
    ///
    /// Taken exclusively, so it also excludes the shared locks builds hold, and must be
    /// *held* across the removal: testing it and releasing would let a build take a cache
    /// hit on the image in the gap, and prune would then delete it mid-build.
    pub fn try_image(lock_dir: &Path, id: &str) -> Result<Option<Self>> {
        let path = image_lock_path(lock_dir, id)?;
        let file = Self::open(&path)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self {
                name: id.to_string(),
                path,
                file: Some(file),
                shared: false,
            })),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(e)) => {
                Err(e).with_context(|| format!("Failed to test lock {}", path.display()))
            }
        }
    }

    /// Return the name of the container or image this lock protects.
    pub fn name(&self) -> &str {
        &self.name
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let Some(file) = self.file.take() else {
            return;
        };
        if self.shared {
            let _ = file.unlock();
            if file.try_lock().is_ok() {
                let _ = std::fs::remove_file(&self.path);
                let _ = file.unlock();
            }
        } else {
            let _ = std::fs::remove_file(&self.path);
            let _ = file.unlock();
        }
    }
}

/// Delete `path` unless a live process holds its lock.
///
/// The unlink happens while the exclusive lock is held, so no build can take the lock in
/// between the test and the removal and be left owning a file prune has already unlinked.
fn remove_if_unlocked(path: &Path) {
    let Ok(file) = File::open(path) else {
        return;
    };
    if file.try_lock().is_ok() {
        let _ = std::fs::remove_file(path);
        let _ = file.unlock();
    }
}

/// Delete lock files for images that no longer exist, so crashed builds do not leak them
/// forever. An image absent from storage cannot be one a build is about to depend on, so
/// this cannot race a build that is mid-acquisition.
pub fn sweep_image_locks(lock_dir: &Path, existing_ids: &HashSet<String>) {
    let Ok(entries) = std::fs::read_dir(lock_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(id) = path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.strip_prefix("image_"))
        else {
            continue;
        };
        if existing_ids.contains(id) {
            continue;
        }
        remove_if_unlocked(&path);
    }
}
