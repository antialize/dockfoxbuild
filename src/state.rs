//! State management for the build process, including variable maps, operation types,
//! and overall build state.
use crate::dockerignore::DockerIgnore;
use std::{collections::HashMap, path::PathBuf};

/// A variable map that combines build arguments and environment variables
/// for use in variable substitution during the build process.
#[derive(Debug, Default)]
pub struct VarMap {
    /// Build arguments (ARG in Dockerfile)
    pub args: HashMap<String, String>,
    /// Environment variables (ENV in Dockerfile)
    pub env: HashMap<String, String>,
}

impl VarMap {
    pub fn get(&self, key: &str) -> &str {
        if let Some(val) = self.env.get(key) {
            val.as_str()
        } else if let Some(val) = self.args.get(key) {
            val.as_str()
        } else {
            ""
        }
    }
}

/// An enum representing the different types of operations that can occur during the build process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    Checkpoint,
    From,
    Arg,
    Env,
    Run,
    Workdir,
    Copy,
    Add,
    Label,
    User,
    Entrypoint,
    Cmd,
    Expose,
    Volume,
    StopSignal,
    HealthCheck,
}

/// Determines if two operations can be chunked together for caching purposes. Checkpoint and From
impl Operation {
    pub fn can_chunk(_: Self, rhs: Self) -> bool {
        !matches!(rhs, Operation::Checkpoint | Operation::From)
    }
}

/// An enum representing work that can be performed out of band during the build process.
pub enum OutOfBandWork {
    /// Mark a cache entry as recently used, given its hash.
    LruMarkCache { hash: String },
    /// Push a cache entry to a remote registry, given the Buildah ID and the cache hash.
    PushCache { buildah_id: String, hash: String },
}

/// The overall state of the build process, including the current variable map, context directory,
#[derive(Debug)]
pub struct State {
    /// The Gitignore matcher for the build context, used to determine which files
    /// should be included or excluded from the build context.
    pub ignore: DockerIgnore,
    /// The current container being built, if any.
    pub container: Option<String>,
    /// Global variable map
    pub global: VarMap,
    /// Stage variable map
    pub stage: VarMap,
    /// The current "AS" alias, if any.
    pub cur_as: Option<String>,
    /// A mapping from "AS" aliases to their corresponding image IDs, used for multi-stage builds.
    pub as_images: HashMap<String, String>,
    /// The last used Buildah ID, used for caching and referencing the current container.
    pub last_id: String,
    /// The build context directory, used for resolving file paths during the build process.
    pub context_dir: PathBuf,
    /// The database connection for caching and build metadata storage.
    pub db: rusqlite::Connection,
    /// Whether to disable caching for this build.
    pub no_cache: bool,
    /// A channel for sending out-of-band work to be performed during the build process.
    pub obw_tx: crossbeam::channel::Sender<OutOfBandWork>,
    /// A channel for receiving results from out-of-band work performed during the build process.
    pub oob_ret_rx: crossbeam::channel::Receiver<String>,
    /// Whether to export the cache after the build
    pub cache_to: bool,
    /// Whether to import the cache before the build, and if so, where to import it from.
    pub cache_from: Option<String>,
    /// The output format for the built image, if specified (e.g., "oci", "docker").
    pub format: Option<String>,
    /// The network mode for RUN instructions, if specified.
    pub network: Option<String>,
}
