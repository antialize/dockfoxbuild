//! A tool for building OCI images using Buildah, with a focus on caching and reproducibility.
use anyhow::Result;
use clap::Parser;

mod build;
mod copy;
mod db;
mod dockerignore;
mod duration;
mod parse;
mod prune;
mod pull;
mod push;
mod size;
mod state;
mod substitute;

#[derive(clap::Subcommand)]
enum Commands {
    /// Build an image from a Dockerfile.
    Build(build::BuildArgs),
    /// Prune the cache.
    Prune(prune::PruneArgs),
    /// Pull an image from a registry.
    Pull(pull::PullArgs),
    /// Push an image to a registry.
    Push(push::PushArgs),
}

/// A tool for building OCI images using Buildah, with a focus on caching and reproducibility.
#[derive(clap::Parser)]
struct Args {
    #[command(subcommand)]
    command: Commands,
}

fn main() -> Result<()> {
    let args = Args::try_parse()?;
    match args.command {
        Commands::Build(args) => build::build_command(args)?,
        Commands::Prune(args) => prune::prune(args)?,
        Commands::Pull(args) => pull::pull(args)?,
        Commands::Push(args) => push::push(args)?,
    }
    Ok(())
}
