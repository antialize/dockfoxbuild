//! This module implements the logic for handling COPY instructions in the Dockerfile.
use std::{io::Read, os::unix::ffi::OsStrExt, path::Path, path::PathBuf, process::Stdio};

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

/// Characters that make a source pattern a glob, matching the wildcards recognized by
/// buildah/Go's `filepath.Glob` (`*`, `?`, and `[...]` character classes). Brace
/// alternation is intentionally excluded because Go does not expand it either.
const GLOB_CHARS: [char; 3] = ['*', '?', '['];

/// Expands a single COPY/ADD source into the concrete paths (rooted at the context
/// directory) that it refers to.
///
/// buildah expands glob patterns in COPY/ADD sources against the build context, so we
/// must do the same before hashing; otherwise a pattern like `backend/*.cc` would be
/// treated as a literal (non-existent) path and contribute nothing to the hash, leaving
/// the cache stale when the matched files change. Sources without glob characters are
/// returned unchanged so behavior is identical to before for plain paths.
fn expand_source(ctx: &Path, source: &str) -> Vec<PathBuf> {
    if !source.contains(GLOB_CHARS) {
        return vec![ctx.join(source)];
    }
    // Escape the context directory so any special characters in it are treated literally,
    // then append the (unescaped) glob pattern from the source.
    let pattern = format!(
        "{}/{}",
        glob::Pattern::escape(&ctx.to_string_lossy()),
        source
    );
    match glob::glob(&pattern) {
        Ok(paths) => paths.filter_map(std::result::Result::ok).collect(),
        Err(e) => {
            println!("Failed to expand glob '{}': {}", source, e);
            Vec::new()
        }
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

    let (_dest, sources) = args
        .rest
        .split_last()
        .context("No source files specified")?;

    let (tx, rx) = std::sync::mpsc::channel::<[u8; 32]>();
    let ctx = &state.context_dir;

    // Expand any glob patterns in the sources into concrete paths, mirroring buildah.
    let expanded: Vec<PathBuf> = sources
        .iter()
        .flat_map(|src| expand_source(ctx, src))
        .collect();

    let mut expanded = expanded.iter();
    let Some(first) = expanded.next() else {
        // No files matched the sources (e.g. globs with no matches); hash an empty set so
        // the result stays consistent instead of failing.
        hasher.update(&0usize.to_le_bytes());
        return Ok(());
    };

    let mut builder = WalkBuilder::new(first);
    builder.hidden(false);
    for src in expanded {
        builder.add(src);
    }

    let walker = builder.build_parallel();
    let debug_hash = state.debug_hash;
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
                let hash = hasher.finalize();
                if debug_hash {
                    println!(
                        "\x1b[33mHASH symlink:\x1b[0m {} -> {} -> {}",
                        relative_path.display(),
                        target.display(),
                        hash.to_hex()
                    );
                }
                tx.send(hash.into()).unwrap();
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
                let hash = hasher.finalize();
                if debug_hash {
                    println!(
                        "\x1b[33mHASH file:\x1b[0m {} -> {}",
                        relative_path.display(),
                        hash.to_hex()
                    );
                }
                tx.send(hash.into()).unwrap();
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
    cmd.arg(state.container.as_ref().expect("Container").name());
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
    cmd.arg(state.container.as_ref().expect("Container").name());
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A scratch directory under the OS temp dir, canonicalized (as `state.context_dir`
    /// is in production, see build.rs) so it has no "." component: the `glob` crate
    /// silently drops a leading "./" from matched paths, which would otherwise make
    /// these tests fail for reasons unrelated to what they check.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("dockfoxbuild_test_{name}_{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir.canonicalize().unwrap())
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn touch(&self, rel: &str) {
            let p = self.0.join(rel);
            if let Some(parent) = p.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(p, "").unwrap();
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Sorts for order-independent comparison; `expand_source` does not guarantee order.
    fn sorted(mut v: Vec<PathBuf>) -> Vec<PathBuf> {
        v.sort();
        v
    }

    #[test]
    fn plain_path_without_glob_chars_is_returned_unchanged() {
        let ctx = TempDir::new("plain");
        // No glob characters, so the path is joined literally even though it doesn't exist.
        let result = expand_source(ctx.path(), "does/not/exist.txt");
        assert_eq!(result, vec![ctx.path().join("does/not/exist.txt")]);
    }

    #[test]
    fn star_glob_matches_files_including_dotfiles() {
        let ctx = TempDir::new("star");
        ctx.touch("a.txt");
        ctx.touch("b.txt");
        ctx.touch(".hidden.txt");
        ctx.touch("c.log");

        // Go's filepath.Glob (used by buildah) does not treat a leading dot specially,
        // so "*.txt" must match dotfiles too.
        let result = sorted(expand_source(ctx.path(), "*.txt"));
        assert_eq!(
            result,
            sorted(vec![
                ctx.path().join("a.txt"),
                ctx.path().join("b.txt"),
                ctx.path().join(".hidden.txt"),
            ])
        );
    }

    #[test]
    fn star_glob_does_not_descend_into_subdirectories() {
        let ctx = TempDir::new("star_nodescend");
        ctx.touch("top.txt");
        ctx.touch("sub/nested.txt");

        // A single "*" segment matches one path component; "sub" itself matches but
        // its contents are not expanded.
        let result = sorted(expand_source(ctx.path(), "*"));
        assert_eq!(
            result,
            sorted(vec![ctx.path().join("top.txt"), ctx.path().join("sub")])
        );
    }

    #[test]
    fn glob_with_subdirectory_prefix_matches_within_that_directory() {
        let ctx = TempDir::new("subdir_prefix");
        ctx.touch("backend/Cargo.toml");
        ctx.touch("backend/Cargo.lock");
        ctx.touch("backend/src/main.rs");

        let result = sorted(expand_source(ctx.path(), "backend/Cargo.*"));
        assert_eq!(
            result,
            sorted(vec![
                ctx.path().join("backend/Cargo.toml"),
                ctx.path().join("backend/Cargo.lock"),
            ])
        );
    }

    #[test]
    fn question_mark_matches_exactly_one_character() {
        let ctx = TempDir::new("question_mark");
        ctx.touch("a.txt");
        ctx.touch("ab.txt");

        // "a?txt" needs exactly one character between "a" and "txt", so only "a.txt"
        // qualifies; "ab.txt" has two characters ("b" and ".") and must not match.
        let result = expand_source(ctx.path(), "a?txt");
        assert_eq!(result, vec![ctx.path().join("a.txt")]);
    }

    #[test]
    fn bracket_char_class_matches_any_listed_character() {
        let ctx = TempDir::new("bracket_class");
        ctx.touch("file1.txt");
        ctx.touch("file2.txt");
        ctx.touch("file3.txt");

        let result = sorted(expand_source(ctx.path(), "file[12].txt"));
        assert_eq!(
            result,
            sorted(vec![
                ctx.path().join("file1.txt"),
                ctx.path().join("file2.txt"),
            ])
        );
    }

    #[test]
    fn glob_with_no_matches_returns_empty() {
        let ctx = TempDir::new("no_match");
        ctx.touch("a.txt");

        assert_eq!(
            expand_source(ctx.path(), "nomatch*.foo"),
            Vec::<PathBuf>::new()
        );
    }

    #[test]
    fn brace_alternation_is_not_expanded_as_a_glob() {
        let ctx = TempDir::new("brace");
        ctx.touch("a.txt");
        ctx.touch("b.txt");

        // `{a,b}.txt` contains no `*`, `?` or `[`, so unlike shell globs it is treated as
        // a literal path (matching Go's filepath.Glob, which buildah relies on) rather
        // than expanded to "a.txt"/"b.txt".
        let result = expand_source(ctx.path(), "{a,b}.txt");
        assert_eq!(result, vec![ctx.path().join("{a,b}.txt")]);
    }

    #[test]
    fn special_characters_in_context_dir_are_escaped() {
        let ctx = TempDir::new("special_[chars]");
        ctx.touch("a.txt");

        // The context directory's own path may legitimately contain glob metacharacters
        // (e.g. "[chars]" in a repo checkout path); expand_source must escape those so
        // they aren't misinterpreted as part of the pattern.
        let result = expand_source(ctx.path(), "*.txt");
        assert_eq!(result, vec![ctx.path().join("a.txt")]);
    }
}
