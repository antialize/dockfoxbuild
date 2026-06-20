//! Forwards `dockfoxbuild push` to `buildah push`, providing a consistent CLI interface.
use anyhow::{Context, Result};
use std::process::Stdio;

/// Arguments for the push command.
#[derive(clap::Parser)]
pub struct PushArgs {
    /// Image to push (name, ID, or tag).
    image: String,

    /// Destination to push to. Defaults to the image name when omitted.
    destination: Option<String>,

    /// Remove the local image after a successful push.
    #[clap(long)]
    rm: bool,

    /// Manifest type to use when pushing, e.g. "oci" or "v2s2".
    #[clap(long)]
    format: Option<String>,

    /// Disable TLS verification for the destination registry.
    #[clap(long)]
    tls_verify: Option<bool>,

    /// Path to an authentication file.
    #[clap(long)]
    authfile: Option<String>,

    /// Extra arguments passed directly to buildah push.
    #[clap(last = true)]
    extra: Vec<String>,
}

pub fn push(args: PushArgs) -> Result<()> {
    let mut cmd = std::process::Command::new("buildah");
    cmd.arg("push");

    if args.rm {
        cmd.arg("--rm");
    }
    if let Some(fmt) = &args.format {
        cmd.args(["--format", fmt]);
    }
    if let Some(verify) = args.tls_verify {
        cmd.args(["--tls-verify", if verify { "true" } else { "false" }]);
    }
    if let Some(authfile) = &args.authfile {
        cmd.args(["--authfile", authfile]);
    }
    for arg in &args.extra {
        cmd.arg(arg);
    }

    cmd.arg(&args.image);
    if let Some(dest) = &args.destination {
        cmd.arg(dest);
    }

    let status = cmd
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("Failed to run buildah push")?;

    if !status.success() {
        anyhow::bail!("buildah push failed with status: {}", status);
    }
    Ok(())
}
