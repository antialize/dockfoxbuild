//! This module implements the logic for handling COPY instructions in the Dockerfile.
use std::{io::Read, os::unix::ffi::OsStrExt, process::Stdio};

use anyhow::{Context, Result};
use clap::Parser;
use ignore::{WalkBuilder, WalkState};

use crate::state::State;

/// Arguments shared by COPY and ADD hashing: we only care about --from (to skip
/// --from instructions) and need to tolerate --checksum so ADD lines parse cleanly.
/// The --checksum value itself is not used during hashing because we hash the local
/// file contents; the checksum is verified by buildah at execution time.
#[derive(clap::Parser)]
struct HashSourceArgs {
    #[clap(long)]
    from: Option<String>,
    #[clap(long)]
    checksum: Option<String>,
    rest: Vec<String>,
}

/// Arguments for the COPY instruction, including support for --from and multiple sources.
#[derive(clap::Parser)]
struct CopyArgs {
    #[clap(long)]
    from: Option<String>,
    rest: Vec<String>,
}

/// Determines if a COPY instruction can be hashed based on its arguments.
/// Currently, we can only hash COPY instructions that do not use --from,
/// as hashing those would require us to read files from another image,
/// which is outside the scope of our current implementation.
pub fn copy_can_hash(line: &str) -> bool {
    !line.contains("--from")
}

/// Determines if an ADD instruction can be hashed based on its arguments.
/// We cannot hash ADD instructions that use --from or reference URL sources,
/// as those require reading from another image or making network requests.
pub fn add_can_hash(line: &str) -> bool {
    if line.contains("--from") {
        return false;
    }
    let parts = shlex::split(line).unwrap_or_default();
    // All non-flag arguments except the last are sources; the last is the destination.
    let non_flags: Vec<&str> = parts
        .iter()
        .filter(|p| !p.starts_with('-'))
        .map(String::as_str)
        .collect();
    match non_flags.split_last() {
        Some((_, sources)) => !sources
            .iter()
            .any(|s| s.starts_with("http://") || s.starts_with("https://")),
        None => true,
    }
}

/// Hashes the contents of the files being copied by a COPY instruction.
/// This function walks through the source files specified in the COPY instruction,
/// computes a hash for each file (including its path and content), and combines them
/// into a single hash that represents the entire COPY instruction.
/// This allows us to determine if the COPY instruction has changed
///
/// We use the ignore crate to handle .dockerignore rules, ensuring that we only hash files
/// that would actually be copied into the image.
pub fn hash_sources(line: &str, state: &mut State, hasher: &mut blake3::Hasher) -> Result<()> {
    let parts = shlex::split(line).context("Invalid")?;

    let args = HashSourceArgs::try_parse_from(
        std::iter::once("copy").chain(parts.iter().map(|s| s.as_str())),
    )
    .context("Failed to parse COPY/ADD arguments")?;

    let (first_source, rest) = args
        .rest
        .split_first()
        .context("No source files specified")?;
    let (_dest, additional_sources) = rest.split_last().context("No destination specified")?;

    let (tx, rx) = std::sync::mpsc::channel::<[u8; 32]>();
    let ctx = &state.context_dir;

    let mut builder = WalkBuilder::new(ctx.join(first_source));
    builder.hidden(false);
    for src in additional_sources {
        builder.add(ctx.join(src));
    }

    let walker = builder.build_parallel();

    walker.run(|| {
        let tx = tx.clone();
        let matcher = &state.ignore;

        Box::new(move |entry| {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    println!("Failed to read entry: {}", e);
                    return WalkState::Continue;
                }
            };
            let path = entry.path();
            let Ok(relative_path) = path.strip_prefix(ctx) else {
                println!("Failed to strip prefix: {}", path.display());
                return WalkState::Continue;
            };
            let is_dir = path.is_dir();
            if matcher.matched(relative_path, is_dir).is_ignore() {
                // Only prune an excluded directory when no negation rule could re-include
                // something beneath it; otherwise we must descend to find the exceptions.
                if is_dir && matcher.can_skip_dir(relative_path) {
                    return WalkState::Skip;
                }
                return WalkState::Continue;
            }

            if path.is_symlink() {
                let target = match std::fs::read_link(path) {
                    Ok(t) => t,
                    Err(e) => {
                        println!("Failed to read symlink: {}", e);
                        return WalkState::Continue;
                    }
                };
                let mut hasher = blake3::Hasher::new();
                hasher.update(relative_path.as_os_str().as_bytes());
                hasher.update(&[1]);
                hasher.update(target.as_os_str().as_bytes());
                tx.send(hasher.finalize().into()).unwrap();
                return WalkState::Continue;
            }

            if path.is_file() {
                let mut f = match std::fs::File::open(path) {
                    Ok(f) => f,
                    Err(e) => {
                        println!("Failed to open file: {}", e);
                        return WalkState::Continue;
                    }
                };
                let mut hasher = blake3::Hasher::new();
                hasher.update(relative_path.as_os_str().as_bytes());
                hasher.update(&[0]);
                let mut buf = [0; 1024 * 128];
                loop {
                    match f.read(&mut buf) {
                        Ok(0) => {
                            break;
                        }
                        Ok(r) => {
                            hasher.update(&buf[..r]);
                        }
                        Err(e) => {
                            println!("Failed to read file: {}", e);
                            return WalkState::Continue;
                        }
                    }
                }
                tx.send(hasher.finalize().into()).unwrap();
            }
            WalkState::Continue
        })
    });

    std::mem::drop(tx);
    let mut content = Vec::new();
    for hash in rx {
        content.push(hash);
    }
    content.sort();

    hasher.update(&content.len().to_le_bytes());
    for c in content {
        hasher.update(&c);
    }
    Ok(())
}

/// Executes a COPY instruction by invoking buildah copy command with the appropriate arguments.
pub fn execute_copy(line: &str, state: &mut State) -> Result<String> {
    println!("\x1b[34mCOPY {}\x1b[0m", line);

    let parts = shlex::split(line).context("Invalid")?;

    let args =
        CopyArgs::try_parse_from(std::iter::once("copy").chain(parts.iter().map(|s| s.as_str())))
            .context("Failed to parse COPY arguments")?;

    let mut cmd = std::process::Command::new("buildah");
    cmd.arg("copy");
    if let Some(from) = args.from {
        let from = state
            .as_images
            .get(&from)
            .map(|v| v.as_str())
            .unwrap_or(from.as_str());
        cmd.arg("--from").arg(from);
    } else {
        cmd.arg("--contextdir").arg(&state.context_dir);
    }
    cmd.arg(state.container.as_ref().expect("Container").trim());
    cmd.args(args.rest);
    let out = cmd
        .stderr(Stdio::inherit())
        .output()
        .with_context(|| format!("Failed to copy files: {}", line))?;
    if !out.status.success() {
        println!("\x1b[31mFAILED\x1b[0m {}", out.status);
        anyhow::bail!("Failed to copy files: {}", line);
    }
    let stdout = String::from_utf8(out.stdout)?;
    Ok(stdout)
}

/// Executes an ADD instruction by invoking buildah add with the appropriate arguments.
/// Unlike COPY, ADD supports URL sources and automatic tar extraction of local archives.
pub fn execute_add(line: &str, state: &mut State) -> Result<String> {
    println!("\x1b[34mADD {}\x1b[0m", line);

    let parts = shlex::split(line).context("Invalid")?;
    #[derive(clap::Parser)]
    struct AddArgs {
        #[clap(long)]
        from: Option<String>,
        #[clap(long)]
        checksum: Option<String>,
        rest: Vec<String>,
    }
    let args =
        AddArgs::try_parse_from(std::iter::once("add").chain(parts.iter().map(|s| s.as_str())))
            .context("Failed to parse ADD arguments")?;

    let mut cmd = std::process::Command::new("buildah");
    cmd.arg("add");
    if let Some(ref from) = args.from {
        let from = state
            .as_images
            .get(from.as_str())
            .map(|v| v.as_str())
            .unwrap_or(from.as_str());
        cmd.arg("--from").arg(from);
    } else {
        // Only use --contextdir for local (non-URL) sources.
        let has_url = args
            .rest
            .iter()
            .take(args.rest.len().saturating_sub(1))
            .any(|s| s.starts_with("http://") || s.starts_with("https://"));
        if !has_url {
            cmd.arg("--contextdir").arg(&state.context_dir);
        }
    }
    if let Some(ref checksum) = args.checksum {
        cmd.arg("--checksum").arg(checksum);
    }
    cmd.arg(state.container.as_ref().expect("Container").trim());
    cmd.args(&args.rest);
    let out = cmd
        .stderr(Stdio::inherit())
        .output()
        .with_context(|| format!("Failed to add files: {}", line))?;
    if !out.status.success() {
        println!("\x1b[31mFAILED\x1b[0m {}", out.status);
        anyhow::bail!("Failed to add files: {}", line);
    }
    let stdout = String::from_utf8(out.stdout)?;
    Ok(stdout)
}
