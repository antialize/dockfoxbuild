//! Forwards `dockfoxbuild pull` to `buildah pull`, providing a consistent CLI interface.
use anyhow::{Context, Result};
use std::process::Stdio;

/// Arguments for the pull command.
#[derive(clap::Parser)]
pub struct PullArgs {
    /// Image to pull (name or reference).
    image: String,

    /// Disable TLS verification for the source registry.
    #[clap(long)]
    tls_verify: Option<bool>,

    /// Path to an authentication file.
    #[clap(long)]
    authfile: Option<String>,

    /// Pull all tagged images in the repository.
    #[clap(long)]
    all_tags: bool,

    /// Extra arguments passed directly to buildah pull.
    #[clap(last = true)]
    extra: Vec<String>,
}

pub fn pull(args: PullArgs) -> Result<()> {
    let mut cmd = std::process::Command::new("buildah");
    cmd.arg("pull");

    if let Some(verify) = args.tls_verify {
        cmd.args(["--tls-verify", if verify { "true" } else { "false" }]);
    }
    if let Some(authfile) = &args.authfile {
        cmd.args(["--authfile", authfile]);
    }
    if args.all_tags {
        cmd.arg("--all-tags");
    }
    for arg in &args.extra {
        cmd.arg(arg);
    }

    cmd.arg(&args.image);

    let status = cmd
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("Failed to run buildah pull")?;

    if !status.success() {
        anyhow::bail!("buildah pull failed with status: {}", status);
    }
    Ok(())
}
