/// Implements the main build logic, which is responsible for executing the parsed Dockerfile operations in order,
/// managing the build state, and handling caching of intermediate layers. It also includes an out-of-band worker for handling cache pushes
/// and LRU marking without blocking the main build process.
use anyhow::{Context, Result};
use clap::Parser as _;
use rusqlite::OptionalExtension;
use std::{io::Read, path::PathBuf, process::Stdio, str::FromStr};

use crate::{
    copy::{add_can_hash, copy_can_hash, execute_add, execute_copy, hash_sources},
    db::{cache_image_name, connect_db},
    dockerignore::load_gitignore,
    parse::parse_dockerfile,
    state::{Operation, OutOfBandWork, State},
    substitute::substitute,
};

/// Kvp is a simple key-value pair struct used for parsing ARG and ENV instructions in the Dockerfile.
#[derive(Debug, Clone)]
struct Kvp {
    key: String,
    value: String,
}

impl FromStr for Kvp {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (key, value) = s
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("Expected KEY=VALUE, got: {}", s))?;
        Ok(Kvp {
            key: key.to_string(),
            value: value.to_string(),
        })
    }
}

/// Command-line arguments for the build process, parsed using `clap`.
#[derive(clap::Parser)]
pub struct BuildArgs {
    /// Path to the Dockerfile to build, defaults to "Dockerfile" in the context directory
    #[clap(short, long)]
    file: Option<PathBuf>,

    /// Build arguments as KEY=VALUE or just KEY (inherits value from the caller's environment).
    #[clap(long)]
    build_arg: Vec<String>,

    /// Path to the build context, defaults to the current directory
    #[clap(default_value = ".")]
    context: PathBuf,

    /// Do not use cache when building the image
    #[clap(long)]
    no_cache: bool,

    /// Tag the resulting image with these tags, e.g. "myimage:latest" or "localhost:5000/myimage:tag"
    #[clap(long, short)]
    tag: Vec<String>,

    /// This flag is for compatibility with docker build, it has no effect in this build system as we always pull.
    #[clap(long)]
    pull: bool,

    /// This flag is for for compatibility with docker build, we always build layers
    #[clap(long)]
    layers: bool,

    /// Format the output image in a specific way, e.g. "oci" or "docker".
    #[clap(long)]
    format: Option<String>,

    /// Cache from these images, e.g. "alpine:latest" or "localhost:5000/myimage:tag"
    #[clap(long)]
    cache_from: Option<String>,

    ///// Push cache to these images, e.g. "localhost:5000/myimage:tag"
    #[clap(long)]
    cache_to: Option<String>,

    /// Set the network mode for RUN instructions, e.g. "none", "host", or "container:<id>".
    #[clap(long)]
    network: Option<String>,
}

/// Execute from instruction by creating a new container from the specified image and setting it as the current container in the build state.
fn execute_from(line: &str, state: &mut State) -> Result<()> {
    let from = line
        .split_once(" AS ")
        .map(|(a, _)| a.trim())
        .unwrap_or(line.as_ref());
    let from = state
        .as_images
        .get(from)
        .map(|v| v.as_str())
        .unwrap_or(from);

    let mut cmd = std::process::Command::new("buildah");
    cmd.arg("from");
    if let Some(format) = &state.format {
        cmd.args(["--format", format]);
    }
    let out = cmd
        .arg(from)
        .stderr(Stdio::inherit())
        .output()
        .with_context(|| format!("Failed to pull image: {}", from))?;
    out.status
        .success()
        .then_some(())
        .ok_or_else(|| anyhow::anyhow!("Failed to pull image: {}", from))?;
    let container = String::from_utf8(out.stdout)?.trim().to_string();

    state.container = Some(container);
    Ok(())
}

/// Execute checkpoint instruction by creating a new container from the last checkpoint and setting it as the current container in the build state.
fn execute_checkpoint(_line: &str, state: &mut State) -> Result<()> {
    println!("\x1b[34mCHECKPOINT\x1b[0m");
    let out = std::process::Command::new("buildah")
        .args(["from", state.last_id.as_ref()])
        .stderr(Stdio::inherit())
        .output()
        .context("Failed to create checkpoint container")?;
    if !out.status.success() {
        anyhow::bail!("Failed to create checkpoint container: {:?}", out.status);
    }
    let container = String::from_utf8(out.stdout)?.trim().to_string();
    state.container = Some(container);
    Ok(())
}

/// Execute ENV instruction by setting the specified environment variables in the current container using buildah config.
fn execute_env(line: &str, state: &mut State) -> Result<()> {
    println!("\x1b[34mENV {}\x1b[0m", line);
    let mut cmd = std::process::Command::new("buildah");
    let cmd = cmd.arg("config");
    for part in shlex::split(line).context("Invalid")? {
        let kvp: Kvp = part.parse()?;
        state.stage.env.insert(kvp.key.clone(), kvp.value.clone());
        cmd.arg("--env");
        cmd.arg(part);
    }
    cmd.arg(
        state.container.as_ref().ok_or_else(|| {
            anyhow::anyhow!("ENV instruction must be used after a FROM instruction")
        })?,
    );
    let out = cmd
        .status()
        .with_context(|| format!("Failed to set environment variables: {}", line))?;
    if !out.success() {
        anyhow::bail!("Failed to set environment variables: {}", line);
    }
    Ok(())
}

fn hash_run(line: &str, args: &[(String, String)], hasher: &mut blake3::Hasher) {
    hasher.update("\0RUN\0".as_bytes());
    for (arg_key, arg_value) in args {
        hasher.update(&[1]);
        hasher.update(arg_key.as_bytes());
        hasher.update("=".as_bytes());
        hasher.update(arg_value.as_bytes());
    }
    hasher.update("\0".as_bytes());
    hasher.update(line.as_bytes());
}

/// Execute a run instruction by invoking buildah run command with the appropriate arguments.
pub fn execute_run(line: &str, state: &mut State, args: &[(String, String)]) -> Result<()> {
    println!("\x1b[34mRUN {}\x1b[0m", line);
    let mut cmd = std::process::Command::new("buildah");
    cmd.arg("run");
    if let Some(network) = &state.network {
        cmd.args(["--network", network]);
    }
    for (key, value) in args {
        cmd.arg("--env");
        cmd.arg(format!("{}={}", key, value));
    }
    cmd.arg(
        state.container.as_ref().ok_or_else(|| {
            anyhow::anyhow!("RUN instruction must be used after a FROM instruction")
        })?,
    );
    cmd.args(["--", "sh", "-c", line]);
    let out = cmd
        .status()
        .with_context(|| format!("Failed to run command: {}", line))?;
    if !out.success() {
        anyhow::bail!("Failed to run command: {}", line);
    }
    Ok(())
}

/// Execute a workdir instruction by invoking buildah config command with the appropriate arguments
/// to set the working directory in the current container.
fn execute_workdir(line: &str, state: &mut State) -> Result<()> {
    println!("\x1b[34mWORKDIR {}\x1b[0m", line);
    let out = std::process::Command::new("buildah")
        .args([
            "config",
            "--workingdir",
            line,
            state.container.as_ref().ok_or_else(|| {
                anyhow::anyhow!("WORKDIR instruction must be used after a FROM instruction")
            })?,
        ])
        .status()
        .with_context(|| format!("Failed to set working directory: {}", line))?;
    if !out.success() {
        anyhow::bail!("Failed to set working directory: {}", line);
    }
    Ok(())
}

/// Execute a label instruction by invoking buildah config command with the appropriate arguments
/// to set the labels in the current container.
fn execute_label(line: &str, state: &mut State) -> Result<()> {
    println!("\x1b[34mLABEL {}\x1b[0m", line);
    let mut cmd = std::process::Command::new("buildah");
    let cmd = cmd.arg("config");
    for part in shlex::split(line).context("Invalid")? {
        cmd.arg("--label");
        cmd.arg(part);
    }
    cmd.arg(state.container.as_ref().ok_or_else(|| {
        anyhow::anyhow!("LABEL instruction must be used after a FROM instruction")
    })?);
    let out = cmd
        .status()
        .with_context(|| format!("Failed to set labels: {}", line))?;
    if !out.success() {
        anyhow::bail!("Failed to set labels: {}", line);
    }
    Ok(())
}

/// Execute a user instruction by setting the user in the current container.
fn execute_user(line: &str, state: &mut State) -> Result<()> {
    println!("\x1b[34mUSER {}\x1b[0m", line);
    let out = std::process::Command::new("buildah")
        .args([
            "config",
            "--user",
            line.trim(),
            state.container.as_ref().ok_or_else(|| {
                anyhow::anyhow!("USER instruction must be used after a FROM instruction")
            })?,
        ])
        .status()
        .with_context(|| format!("Failed to set user: {}", line))?;
    if !out.success() {
        anyhow::bail!("Failed to set user: {}", line);
    }
    Ok(())
}

/// Execute an entrypoint instruction. Exec form (starting with `[`) is passed as a JSON array;
/// shell form is wrapped in `/bin/sh -c`.
fn execute_entrypoint(line: &str, state: &mut State) -> Result<()> {
    println!("\x1b[34mENTRYPOINT {}\x1b[0m", line);
    let container = state.container.as_ref().ok_or_else(|| {
        anyhow::anyhow!("ENTRYPOINT instruction must be used after a FROM instruction")
    })?;
    let entrypoint = if line.trim_start().starts_with('[') {
        line.trim().to_string()
    } else {
        serde_json::to_string(&["/bin/sh", "-c", line.trim()])
            .context("Failed to serialize ENTRYPOINT")?
    };
    let out = std::process::Command::new("buildah")
        .args(["config", "--entrypoint", &entrypoint, container])
        .status()
        .with_context(|| format!("Failed to set entrypoint: {}", line))?;
    if !out.success() {
        anyhow::bail!("Failed to set entrypoint: {}", line);
    }
    Ok(())
}

/// Execute a cmd instruction. Exec form (starting with `[`) is passed as a JSON array;
/// shell form is wrapped in `/bin/sh -c`.
fn execute_cmd(line: &str, state: &mut State) -> Result<()> {
    println!("\x1b[34mCMD {}\x1b[0m", line);
    let container = state
        .container
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("CMD instruction must be used after a FROM instruction"))?;
    let cmd_val = if line.trim_start().starts_with('[') {
        line.trim().to_string()
    } else {
        serde_json::to_string(&["/bin/sh", "-c", line.trim()]).context("Failed to serialize CMD")?
    };
    let out = std::process::Command::new("buildah")
        .args(["config", "--cmd", &cmd_val, container])
        .status()
        .with_context(|| format!("Failed to set cmd: {}", line))?;
    if !out.success() {
        anyhow::bail!("Failed to set cmd: {}", line);
    }
    Ok(())
}

/// Execute an expose instruction by declaring one or more ports on the current container.
fn execute_expose(line: &str, state: &mut State) -> Result<()> {
    println!("\x1b[34mEXPOSE {}\x1b[0m", line);
    let container = state.container.as_ref().ok_or_else(|| {
        anyhow::anyhow!("EXPOSE instruction must be used after a FROM instruction")
    })?;
    let mut cmd = std::process::Command::new("buildah");
    cmd.arg("config");
    for port in line.split_whitespace() {
        cmd.arg("--port").arg(port);
    }
    cmd.arg(container);
    let out = cmd
        .status()
        .with_context(|| format!("Failed to set exposed ports: {}", line))?;
    if !out.success() {
        anyhow::bail!("Failed to set exposed ports: {}", line);
    }
    Ok(())
}

/// Execute a volume instruction by declaring one or more mount points on the current container.
fn execute_volume(line: &str, state: &mut State) -> Result<()> {
    println!("\x1b[34mVOLUME {}\x1b[0m", line);
    let container = state.container.as_ref().ok_or_else(|| {
        anyhow::anyhow!("VOLUME instruction must be used after a FROM instruction")
    })?;
    let volumes: Vec<String> = if line.trim_start().starts_with('[') {
        serde_json::from_str(line.trim()).context("Failed to parse VOLUME JSON array")?
    } else {
        shlex::split(line).context("Failed to parse VOLUME")?
    };
    let mut cmd = std::process::Command::new("buildah");
    cmd.arg("config");
    for vol in &volumes {
        cmd.arg("--volume").arg(vol);
    }
    cmd.arg(container);
    let out = cmd
        .status()
        .with_context(|| format!("Failed to set volumes: {}", line))?;
    if !out.success() {
        anyhow::bail!("Failed to set volumes: {}", line);
    }
    Ok(())
}

/// Execute a stopsignal instruction by setting the stop signal on the current container.
fn execute_stopsignal(line: &str, state: &mut State) -> Result<()> {
    println!("\x1b[34mSTOPSIGNAL {}\x1b[0m", line);
    let out = std::process::Command::new("buildah")
        .args([
            "config",
            "--stop-signal",
            line.trim(),
            state.container.as_ref().ok_or_else(|| {
                anyhow::anyhow!("STOPSIGNAL instruction must be used after a FROM instruction")
            })?,
        ])
        .status()
        .with_context(|| format!("Failed to set stop signal: {}", line))?;
    if !out.success() {
        anyhow::bail!("Failed to set stop signal: {}", line);
    }
    Ok(())
}

/// Execute a healthcheck instruction by configuring the health check command and options.
/// Supports: HEALTHCHECK NONE  and  HEALTHCHECK [OPTIONS] CMD <test>
/// Options: --interval=, --timeout=, --start-period=, --retries=
fn execute_healthcheck(line: &str, state: &mut State) -> Result<()> {
    println!("\x1b[34mHEALTHCHECK {}\x1b[0m", line);
    let container = state.container.as_ref().ok_or_else(|| {
        anyhow::anyhow!("HEALTHCHECK instruction must be used after a FROM instruction")
    })?;

    #[derive(clap::Parser)]
    struct HealthCheckArgs {
        #[clap(long)]
        interval: Option<String>,
        #[clap(long)]
        timeout: Option<String>,
        #[clap(long)]
        start_period: Option<String>,
        #[clap(long)]
        retries: Option<u32>,
    }

    let trimmed = line.trim();
    if trimmed == "NONE" {
        let out = std::process::Command::new("buildah")
            .args(["config", "--healthcheck", "NONE", container])
            .status()
            .context("Failed to set HEALTHCHECK NONE")?;
        if !out.success() {
            anyhow::bail!("Failed to set HEALTHCHECK NONE");
        }
        return Ok(());
    }

    // Tokenize the whole line, then split on the first bare "CMD" token.
    // This naturally handles quoted option values and is consistent with how
    // COPY is parsed — shlex first, then clap for the options portion.
    let tokens = shlex::split(trimmed).context("Failed to parse HEALTHCHECK")?;
    let cmd_pos = tokens
        .iter()
        .position(|t| t == "CMD")
        .ok_or_else(|| anyhow::anyhow!("HEALTHCHECK instruction missing CMD keyword: {}", line))?;
    let (opt_tokens, cmd_tokens) = tokens.split_at(cmd_pos);
    let cmd_tokens = &cmd_tokens[1..]; // drop the "CMD" token itself

    let args = HealthCheckArgs::try_parse_from(
        std::iter::once("healthcheck").chain(opt_tokens.iter().map(|s| s.as_str())),
    )
    .context("Failed to parse HEALTHCHECK options")?;

    let mut cmd = std::process::Command::new("buildah");
    cmd.arg("config");
    if let Some(v) = &args.interval {
        cmd.args(["--healthcheck-interval", v]);
    }
    if let Some(v) = &args.timeout {
        cmd.args(["--healthcheck-timeout", v]);
    }
    if let Some(v) = &args.start_period {
        cmd.args(["--healthcheck-start-period", v]);
    }
    if let Some(v) = args.retries {
        cmd.args(["--healthcheck-retries", &v.to_string()]);
    }

    // Reassemble the test command. If it's a single token starting with '[' it's exec
    // form (already a JSON array string); otherwise rejoin as a shell string.
    let test_str = cmd_tokens.join(" ");
    let test = if test_str.trim_start().starts_with('[') {
        format!("CMD {}", test_str)
    } else {
        format!("CMD-SHELL {}", test_str)
    };
    cmd.args(["--healthcheck", &test, container]);
    let out = cmd
        .status()
        .with_context(|| format!("Failed to set HEALTHCHECK: {}", line))?;
    if !out.success() {
        anyhow::bail!("Failed to set HEALTHCHECK: {}", line);
    }
    Ok(())
}

enum Preprocessed {
    From(String),
    Checkpoint(String),
    Env(String),
    Run {
        line: String,
        args: Vec<(String, String)>,
    },
    Workdir(String),
    Copy(String),
    Label(String),
    Add(String),
    User(String),
    Entrypoint(String),
    Cmd(String),
    Expose(String),
    Volume(String),
    StopSignal(String),
    HealthCheck(String),
}

/// Executes a chunk of Dockerfile instructions, which is a sequence of instructions starting with a FROM or CHECKPOINT instruction
/// and ending before the next FROM or CHECKPOINT instruction.
fn execute_chunk(ops: &[(Operation, String)], state: &mut State) -> Result<()> {
    // If the first instruction is not a FROM or CHECKPOINT, then we are in the initial ARG instructions before the first FROM,
    //which only provide default values for build args and do not affect caching, so we can just execute them and return.
    if !matches!(
        ops.first(),
        Some((Operation::From | Operation::Checkpoint, _))
    ) {
        for (op, line) in ops {
            match op {
                Operation::Arg => {
                    let line = substitute(line, &state.global)?;
                    for part in shlex::split(&line).context("Invalid")? {
                        let kvp = part.parse::<Kvp>()?;
                        match state.global.args.entry(kvp.key) {
                            std::collections::hash_map::Entry::Occupied(_) => {
                                // ARG before the first FROM only provides default values for build args
                            }
                            std::collections::hash_map::Entry::Vacant(e) => {
                                e.insert(kvp.value);
                            }
                        }
                    }
                }
                _ => {
                    anyhow::bail!(
                        "Only ARG instructions are allowed before the first FROM or CHECKPOINT, found: {:?}",
                        op
                    );
                }
            }
        }
        return Ok(());
    }

    // Pull remote image and find its environment variables
    let mut ppops = Vec::with_capacity(ops.len());
    let mut hasher = blake3::Hasher::new();
    let ((first_op, first_line), ops) = ops.split_first().unwrap();
    match *first_op {
        Operation::From => {
            // If there is a current AS, save the image for that AS before processing the new FROM
            if let Some(cur_as) = state.cur_as.take() {
                // Print that we insert in yellow
                state
                    .as_images
                    .insert(cur_as, std::mem::take(&mut state.last_id));
            }

            let line = substitute(first_line, &state.global)?;
            println!("\x1b[34mFROM {}\x1b[0m", line);
            let from = if let Some((from, as_)) = line.split_once(" AS ") {
                state.cur_as = Some(as_.trim().to_string());
                from.trim()
            } else {
                line.as_ref()
            };

            if from == "scratch" {
                hasher.update("\0FROM\0scratch\0".as_bytes());
            } else {
                let image = match state.as_images.get(from) {
                    Some(image) => image.clone(),
                    None => {
                        let out = std::process::Command::new("buildah")
                            .args(["pull", from])
                            .stderr(Stdio::inherit())
                            .output()
                            .with_context(|| format!("Failed to pull image: {}", from))?;
                        out.status
                            .success()
                            .then_some(())
                            .ok_or_else(|| anyhow::anyhow!("Failed to pull image: {}", from))?;
                        let image = String::from_utf8(out.stdout)?.trim().to_string();
                        state.db.execute(
                            "INSERT OR REPLACE INTO remote_images (buildah_id, last_used_at) VALUES (?1, unixepoch())",
                            rusqlite::params![image],
                        )?;
                        image
                    }
                };
                let out = std::process::Command::new("buildah")
                    .args([
                        "inspect",
                        "--format",
                        "{{range .OCIv1.Config.Env}}{{.}}\n{{end}}",
                        &image,
                    ])
                    .stderr(Stdio::inherit())
                    .output()
                    .with_context(|| format!("Failed to inspect image: {}", from))?;
                out.status
                    .success()
                    .then_some(())
                    .ok_or_else(|| anyhow::anyhow!("Failed to inspect image: {}", from))?;
                for line in String::from_utf8(out.stdout)?.lines() {
                    if let Some((key, value)) = line.split_once('=') {
                        state.stage.env.insert(key.to_string(), value.to_string());
                    }
                }
                hasher.update("\0FROM\0".as_bytes());
                hasher.update(image.as_bytes());
            }
            ppops.push(Preprocessed::From(line));
        }
        Operation::Checkpoint => {
            ppops.push(Preprocessed::Checkpoint(first_line.clone()));
        }
        _ => unreachable!("First instruction in chunk should be FROM or CHECKPOINT"),
    }
    handle_oob_ret(&mut state.oob_ret_rx);

    // Preprocess instructions by substituting variables.
    for (op, line) in ops {
        match op {
            Operation::Arg => {
                let line = substitute(line, &state.stage)?;
                for part in shlex::split(&line).context("Invalid")? {
                    match part.parse::<Kvp>() {
                        Ok(kvp) => {
                            state.stage.args.insert(kvp.key, kvp.value);
                        }
                        Err(_) => {
                            if let Some(v) = state.global.args.get(&part) {
                                state.stage.args.insert(part, v.clone());
                            } else {
                                state.stage.args.insert(part, "".to_string());
                            }
                        }
                    }
                }
                // The arg instruction is now fully handled
            }
            Operation::Env => {
                let line = substitute(line, &state.stage)?;
                for part in shlex::split(&line).context("Invalid")? {
                    let kvp: Kvp = part.parse()?;
                    state.stage.env.insert(kvp.key, kvp.value);
                }
                ppops.push(Preprocessed::Env(line));
            }
            Operation::From => {
                unreachable!("FROM instruction should not be in the middle of a chunk");
            }
            Operation::Checkpoint => {
                unreachable!("CHECKPOINT instruction should not be in the middle of a chunk");
            }
            // RUN: variable substitution is handled by the shell, not the builder.
            // ARG values are automatically passed as environment variables to RUN by buildah.
            Operation::Run => {
                let mut args: Vec<_> = state
                    .stage
                    .args
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                args.sort();
                ppops.push(Preprocessed::Run {
                    line: line.to_string(),
                    args,
                });
            }
            // CMD and ENTRYPOINT are image metadata (runtime config), not build-time commands.
            // Build args are not available at container runtime, so they are not passed here.
            Operation::Entrypoint => {
                ppops.push(Preprocessed::Entrypoint(line.to_string()));
            }
            Operation::Cmd => {
                ppops.push(Preprocessed::Cmd(line.to_string()));
            }
            Operation::Workdir => {
                let line = substitute(line, &state.stage)?;
                ppops.push(Preprocessed::Workdir(line));
            }
            Operation::Copy => {
                let line = substitute(line, &state.stage)?;
                ppops.push(Preprocessed::Copy(line));
            }
            Operation::Add => {
                let line = substitute(line, &state.stage)?;
                ppops.push(Preprocessed::Add(line));
            }
            Operation::Label => {
                let line = substitute(line, &state.stage)?;
                ppops.push(Preprocessed::Label(line));
            }
            Operation::User => {
                let line = substitute(line, &state.stage)?;
                ppops.push(Preprocessed::User(line));
            }
            Operation::Expose => {
                let line = substitute(line, &state.stage)?;
                ppops.push(Preprocessed::Expose(line));
            }
            Operation::Volume => {
                let line = substitute(line, &state.stage)?;
                ppops.push(Preprocessed::Volume(line));
            }
            Operation::StopSignal => {
                let line = substitute(line, &state.stage)?;
                ppops.push(Preprocessed::StopSignal(line));
            }
            Operation::HealthCheck => {
                let line = substitute(line, &state.stage)?;
                ppops.push(Preprocessed::HealthCheck(line));
            }
        }
    }

    // We cannot currently hash COPY instructions that use --from,
    // as that would require us to read files from another image.
    // To work around this, we split the chunk into a head and main part,
    // where the head contains all instructions up to and including the last COPY instruction that cannot be hashed,
    // and the main contains the rest of the instructions
    let last_copy = ppops.iter().rposition(|ppop| {
        matches!(ppop, Preprocessed::Copy(line) if !copy_can_hash(line))
            || matches!(ppop, Preprocessed::Add(line) if !add_can_hash(line))
    });
    let (head, main) = if let Some(pos) = last_copy {
        ppops.split_at(pos + 1)
    } else {
        (&[] as &[Preprocessed], ppops.as_slice())
    };

    // Execute head while hashing, this ensures that we handle all instructions that
    // can affect the hash before we compute the hash and check the cache.
    for ppop in head {
        match ppop {
            Preprocessed::From(line) => {
                execute_from(line, state)?;
            }
            Preprocessed::Env(line) => {
                hasher.update("\0ENV\0".as_bytes());
                hasher.update(line.as_bytes());
                execute_env(line, state)?;
            }
            Preprocessed::Run { line, args } => {
                hash_run(line, args, &mut hasher);
                execute_run(line, state, args)?;
            }
            Preprocessed::Workdir(line) => {
                hasher.update("\0WORKDIR\0".as_bytes());
                hasher.update(line.as_bytes());
                execute_workdir(line, state)?;
            }
            Preprocessed::Copy(line) => {
                hasher.update("\0COPY\0".as_bytes());
                let content_hash = execute_copy(line, state)?;
                hasher.update(content_hash.trim().as_bytes());
            }
            Preprocessed::Label(line) => {
                hasher.update("\0LABEL\0".as_bytes());
                hasher.update(line.as_bytes());
                execute_label(line, state)?;
            }
            Preprocessed::Add(line) => {
                hasher.update("\0ADD\0".as_bytes());
                let content_hash = execute_add(line, state)?;
                hasher.update(content_hash.trim().as_bytes());
            }
            Preprocessed::User(line) => {
                hasher.update("\0USER\0".as_bytes());
                hasher.update(line.as_bytes());
                execute_user(line, state)?;
            }
            Preprocessed::Entrypoint(line) => {
                hasher.update("\0ENTRYPOINT\0".as_bytes());
                hasher.update(line.as_bytes());
                execute_entrypoint(line, state)?;
            }
            Preprocessed::Cmd(line) => {
                hasher.update("\0CMD\0".as_bytes());
                hasher.update(line.as_bytes());
                execute_cmd(line, state)?;
            }
            Preprocessed::Expose(line) => {
                hasher.update("\0EXPOSE\0".as_bytes());
                hasher.update(line.as_bytes());
                execute_expose(line, state)?;
            }
            Preprocessed::Volume(line) => {
                hasher.update("\0VOLUME\0".as_bytes());
                hasher.update(line.as_bytes());
                execute_volume(line, state)?;
            }
            Preprocessed::StopSignal(line) => {
                hasher.update("\0STOPSIGNAL\0".as_bytes());
                hasher.update(line.as_bytes());
                execute_stopsignal(line, state)?;
            }
            Preprocessed::HealthCheck(line) => {
                hasher.update("\0HEALTHCHECK\0".as_bytes());
                hasher.update(line.as_bytes());
                execute_healthcheck(line, state)?;
            }
            Preprocessed::Checkpoint(line) => {
                execute_checkpoint(line, state)?;
            }
        }
        handle_oob_ret(&mut state.oob_ret_rx);
    }

    // Hash Remaining
    for op in main {
        match op {
            // All ready handled
            Preprocessed::From(_) | Preprocessed::Checkpoint(_) => {}
            Preprocessed::Env(line) => {
                hasher.update("\0ENV\0".as_bytes());
                hasher.update(line.as_bytes());
            }
            Preprocessed::Run { line, args } => {
                hash_run(line, args, &mut hasher);
            }
            Preprocessed::Workdir(line) => {
                hasher.update("\0WORKDIR\0".as_bytes());
                hasher.update(line.as_bytes());
            }
            Preprocessed::Copy(line) => {
                hasher.update("\0COPY\0".as_bytes());
                hash_sources(line, state, &mut hasher)?;
            }
            Preprocessed::Label(line) => {
                hasher.update("\0LABEL\0".as_bytes());
                hasher.update(line.as_bytes());
            }
            Preprocessed::Add(line) => {
                hasher.update("\0ADD\0".as_bytes());
                hash_sources(line, state, &mut hasher)?;
            }
            Preprocessed::User(line) => {
                hasher.update("\0USER\0".as_bytes());
                hasher.update(line.as_bytes());
            }
            Preprocessed::Entrypoint(line) => {
                hasher.update("\0ENTRYPOINT\0".as_bytes());
                hasher.update(line.as_bytes());
            }
            Preprocessed::Cmd(line) => {
                hasher.update("\0CMD\0".as_bytes());
                hasher.update(line.as_bytes());
            }
            Preprocessed::Expose(line) => {
                hasher.update("\0EXPOSE\0".as_bytes());
                hasher.update(line.as_bytes());
            }
            Preprocessed::Volume(line) => {
                hasher.update("\0VOLUME\0".as_bytes());
                hasher.update(line.as_bytes());
            }
            Preprocessed::StopSignal(line) => {
                hasher.update("\0STOPSIGNAL\0".as_bytes());
                hasher.update(line.as_bytes());
            }
            Preprocessed::HealthCheck(line) => {
                hasher.update("\0HEALTHCHECK\0".as_bytes());
                hasher.update(line.as_bytes());
            }
        }
        handle_oob_ret(&mut state.oob_ret_rx);
    }

    // After hashing, we can check the cache and execute the remaining instructions if there is a cache miss, or skip them if there is a cache hit.
    let chunk_hash = hasher.finalize().to_hex().to_string();
    // When --no-cache is set we do not consult any cache (local or remote), so we
    // start from a forced cache miss and skip all of the lookup logic below.
    let mut image = if state.no_cache {
        None
    } else {
        state
            .db
            .query_one(
                "UPDATE checkpoint_cache SET last_used_at = unixepoch() WHERE checkpoint_hash = ?1 RETURNING buildah_id",
                rusqlite::params![chunk_hash],
                |row| row.get::<_, String>(0),
            )
            .optional()?
    };

    if let Some(img) = &image {
        if state.cache_from.is_some() {
            // Mark the cache image as used on the remote server by pulling it, this will update its last used timestamp on the server and prevent it from being evicted.
            state.obw_tx.send(OutOfBandWork::LruMarkCache {
                hash: chunk_hash.clone(),
            })?;
        }

        // Check if image is actually there
        let status = std::process::Command::new("buildah")
            .args(["images", "--format", "{{.ID}}", img])
            .stdout(Stdio::null())
            .status()
            .with_context(|| format!("Failed to check if image exists: {}", img))?;
        if !status.success() {
            // Print cache failure in red
            println!(
                "\x1b[31mCACHE FAILURE\x1b[0m Failed to check if image exists: {}",
                img
            );
            state.db.execute(
                "DELETE FROM checkpoint_cache WHERE checkpoint_hash = ?1",
                rusqlite::params![chunk_hash],
            )?;
            image = None;
        }
    }

    // If there is a cache miss and we have a cache from, we can try to pull the image from the remote registry
    // and check if it exists there. Skipped entirely when --no-cache is set.
    if !state.no_cache
        && image.is_none()
        && let Some(from_cache) = &state.cache_from
    {
        let from_name = format!("{}:{}", from_cache, chunk_hash);

        let out = std::process::Command::new("buildah")
            .args(["pull", from_name.as_ref()])
            .stderr(Stdio::null())
            .output()
            .with_context(|| format!("Failed to pull cache image: {}", from_name))?;
        if out.status.success() {
            let id = String::from_utf8(out.stdout)?.trim().to_string();
            image = Some(id);
            println!("\x1b[32mCACHE HIT\x1b[0m {}", from_name);

            state.db.execute(
                "INSERT OR REPLACE INTO checkpoint_cache (checkpoint_hash, buildah_id, created_at, last_used_at) VALUES (?1, ?2, unixepoch(), unixepoch())",
                rusqlite::params![chunk_hash, image.as_ref().unwrap()],
            )?;
        } else {
            // Buildah failes with status 125 no mater what happens,
            // so we cannot check if the image was not found or if there was a different error, we just assume it was not found and print the error message for debugging.
        }
    }

    // If there is a cache hit, we can skip executing the instructions and just set the current image to the cached image.
    if !state.no_cache
        && let Some(image) = image
    {
        if let Some(container) = &state.container {
            let r = std::process::Command::new("buildah")
                .args(["rm", container])
                .stdout(Stdio::null())
                .status()
                .context("Failed to remove container")?;
            if !r.success() {
                anyhow::bail!("Failed to remove container: {:?}", r);
            }
            state.container = None;
        }

        for op in main {
            match op {
                Preprocessed::From(_) => (),
                Preprocessed::Env(line) => println!("\x1b[32mENV {}\x1b[0m", line),
                Preprocessed::Run { line, .. } => println!("\x1b[32mRUN {}\x1b[0m", line),
                Preprocessed::Workdir(line) => println!("\x1b[32mWORKDIR {}\x1b[0m", line),
                Preprocessed::Copy(line) => println!("\x1b[32mCOPY {}\x1b[0m", line),
                Preprocessed::Add(line) => println!("\x1b[32mADD {}\x1b[0m", line),
                Preprocessed::Label(line) => println!("\x1b[32mLABEL {}\x1b[0m", line),
                Preprocessed::User(line) => println!("\x1b[32mUSER {}\x1b[0m", line),
                Preprocessed::Entrypoint(line) => println!("\x1b[32mENTRYPOINT {}\x1b[0m", line),
                Preprocessed::Cmd(line) => println!("\x1b[32mCMD {}\x1b[0m", line),
                Preprocessed::Expose(line) => println!("\x1b[32mEXPOSE {}\x1b[0m", line),
                Preprocessed::Volume(line) => println!("\x1b[32mVOLUME {}\x1b[0m", line),
                Preprocessed::StopSignal(line) => println!("\x1b[32mSTOPSIGNAL {}\x1b[0m", line),
                Preprocessed::HealthCheck(line) => println!("\x1b[32mHEALTHCHECK {}\x1b[0m", line),
                Preprocessed::Checkpoint(line) => println!("\x1b[32mCHECKPOINT {}\x1b[0m", line),
            };
        }
        handle_oob_ret(&mut state.oob_ret_rx);
        state.last_id = image;
        return Ok(());
    }

    // If there is a cache miss, we execute the remaining instructions as normal.
    for op in main {
        match op {
            Preprocessed::From(line) => {
                execute_from(line, state)?;
            }
            Preprocessed::Env(line) => {
                execute_env(line, state)?;
            }
            Preprocessed::Run { line, args } => {
                execute_run(line, state, args)?;
            }
            Preprocessed::Workdir(line) => {
                execute_workdir(line, state)?;
            }
            Preprocessed::Copy(line) => {
                execute_copy(line, state)?;
            }
            Preprocessed::Add(line) => {
                execute_add(line, state)?;
            }
            Preprocessed::Label(line) => {
                execute_label(line, state)?;
            }
            Preprocessed::User(line) => {
                execute_user(line, state)?;
            }
            Preprocessed::Entrypoint(line) => {
                execute_entrypoint(line, state)?;
            }
            Preprocessed::Cmd(line) => {
                execute_cmd(line, state)?;
            }
            Preprocessed::Expose(line) => {
                execute_expose(line, state)?;
            }
            Preprocessed::Volume(line) => {
                execute_volume(line, state)?;
            }
            Preprocessed::StopSignal(line) => {
                execute_stopsignal(line, state)?;
            }
            Preprocessed::HealthCheck(line) => {
                execute_healthcheck(line, state)?;
            }
            Preprocessed::Checkpoint(line) => {
                execute_checkpoint(line, state)?;
            }
        }
        handle_oob_ret(&mut state.oob_ret_rx);
    }

    let name = cache_image_name(&chunk_hash);
    // Commit the container and delete it
    let out = std::process::Command::new("buildah")
        .args(["commit", state.container.as_ref().unwrap(), name.as_ref()])
        .stderr(Stdio::inherit())
        .output()
        .context("Failed to commit container")?;
    if !out.status.success() {
        anyhow::bail!("Failed to commit container: {:?}", out.status);
    }
    let image_id = String::from_utf8(out.stdout)?.trim().to_string();

    if state.cache_to {
        state.obw_tx.send(OutOfBandWork::PushCache {
            buildah_id: image_id.clone(),
            hash: chunk_hash.clone(),
        })?;
    }

    state.db.execute(
        "INSERT OR REPLACE INTO checkpoint_cache
        (checkpoint_hash, buildah_id, created_at, last_used_at)
        VALUES (?1, ?2, unixepoch(), unixepoch())",
        rusqlite::params![chunk_hash, image_id],
    )?;

    state.last_id = image_id;

    let r = std::process::Command::new("buildah")
        .args(["rm", state.container.as_ref().unwrap()])
        .status()
        .context("Failed to remove container")?;
    if !r.success() {
        anyhow::bail!("Failed to remove container: {:?}", r);
    }
    state.container = None;

    Ok(())
}

/// Handle out-of-band returns by printing them to the console.
/// This is used by the out-of-band worker to send messages back to the main thread without blocking it.
pub fn handle_oob_ret(oob_ret_rx: &mut crossbeam::channel::Receiver<String>) {
    while let Ok(ret) = oob_ret_rx.try_recv() {
        println!("{}", ret);
    }
}

/// Out-of-band worker that handles cache pushes and LRU marking without blocking the main build process.
/// It listens for work on the `oob_rx` channel and sends results back on the `oob_ret_tx` channel.
pub fn oob_worker(
    oob_rx: crossbeam::channel::Receiver<OutOfBandWork>,
    oob_ret_tx: &mut crossbeam::channel::Sender<String>,
    cache_to: Option<String>,
    cache_from: Option<String>,
) -> Result<()> {
    for work in oob_rx {
        match work {
            OutOfBandWork::LruMarkCache { hash } => {
                // We don't actually care about the image, we just want to make it a used upstream
                // We could replace this with a direct HTTP HEAD request to the metadata.
                // But that would require us to implement and understand the auth logic
                let Some(cache_from) = &cache_from else {
                    continue;
                };
                let name = format!("{}:{}", cache_from, hash);
                // We do not care about the output of this command.
                let _ = std::process::Command::new("buildah")
                    .args(["pull", &name])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
                oob_ret_tx.send(format!("\x1b[35mUSED CACHE {}\x1b[0m", name))?;
            }
            OutOfBandWork::PushCache { buildah_id, hash } => {
                let Some(cache_to) = &cache_to else {
                    continue;
                };
                let name = format!("{}:{}", cache_to, hash);
                let (mut reader, writer) = std::io::pipe()?;
                let mut out = std::process::Command::new("buildah")
                    .args(["push", &buildah_id, &name])
                    .stdout(writer.try_clone()?)
                    .stderr(writer)
                    .spawn()
                    .with_context(|| format!("Failed to push cache image: {}", name))?;

                let mut output = String::new();
                reader.read_to_string(&mut output)?;
                let status = out
                    .wait()
                    .with_context(|| format!("Failed to wait for push command: {}", name))?;
                if status.success() {
                    oob_ret_tx.send(format!("\x1b[35mPUSHED CACHE {}\x1b[0m", name))?;
                } else {
                    oob_ret_tx.send(format!(
                        "\x1b[31mFailed to push cache image: {}\x1b[0m\nOutput:\n{}",
                        name, output
                    ))?;
                }
            }
        }
    }
    Ok(())
}

/// Main build function that orchestrates the entire build process, including parsing the Dockerfile, managing the build state,
/// executing instructions, and handling caching.
pub fn build_command(args: BuildArgs) -> Result<()> {
    let db = connect_db()?;

    let file = args.file.unwrap_or_else(|| args.context.join("Dockerfile"));

    let ignore = load_gitignore(&args.context, &file)?;
    let operations = parse_dockerfile(&file)?;

    let (obw_tx, obw_rx) = crossbeam::channel::unbounded();
    let (oob_ret_tx, oob_ret_rx) = crossbeam::channel::unbounded();

    let mut state = State {
        ignore,
        context_dir: args.context,
        db,
        container: Default::default(),
        global: Default::default(),
        stage: Default::default(),
        cur_as: Default::default(),
        as_images: Default::default(),
        last_id: Default::default(),
        no_cache: args.no_cache,
        obw_tx,
        oob_ret_rx,
        cache_to: args.cache_to.is_some(),
        cache_from: args.cache_from.clone(),
        format: args.format,
        network: args.network,
    };

    for (key, value) in std::env::vars() {
        state.global.env.insert(key, value);
    }

    // Seed build args: KEY=VALUE sets the arg directly; bare KEY inherits from the
    // caller's environment (Docker behaviour), falling back to an empty string.
    for arg in args.build_arg {
        let (key, value) = match arg.split_once('=') {
            Some((k, v)) => (k.to_string(), v.to_string()),
            None => {
                let v = std::env::var(&arg).unwrap_or_default();
                (arg, v)
            }
        };
        state.global.args.insert(key, value);
    }

    for _ in 0..8 {
        let obw_rx = obw_rx.clone();
        let mut oob_ret_tx = oob_ret_tx.clone();
        let cache_to = args.cache_to.clone();
        let cache_from = args.cache_from.clone();
        std::thread::spawn(move || {
            if let Err(e) = oob_worker(obw_rx, &mut oob_ret_tx, cache_to, cache_from) {
                let _ =
                    oob_ret_tx.send(format!("\x1b[31mOut of band worker error: {:?}\x1b[0m", e));
            }
        });
    }

    // We chunk operations by FROM and CHECKPOINT instructions, as they from the cache boundaries.
    for chunk in operations.chunk_by(|l, r| Operation::can_chunk(l.0, r.0)) {
        execute_chunk(chunk, &mut state)?;
    }

    // Tag the final image with the specified tags, if any.
    for tag in args.tag {
        let out = std::process::Command::new("buildah")
            .args(["tag", &state.last_id, &tag])
            .status()
            .with_context(|| format!("Failed to tag image: {}", tag))?;
        if !out.success() {
            anyhow::bail!("Failed to tag image: {}", tag);
        }
    }

    // Drop the out-of-band worker channels and print any remaining messages from the out-of-band workers.
    std::mem::drop(state.obw_tx);
    std::mem::drop(oob_ret_tx);
    for ret in state.oob_ret_rx {
        println!("{}", ret);
    }

    Ok(())
}
