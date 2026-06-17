//! Provides functionality to load and compile .dockerignore files into a matcher using the `ignore` crate.
//! This allows us to determine which files should be included or excluded from the build context when building.
use std::{
    fs::File,
    io::BufRead,
    io::BufReader,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder};

/// A compiled .dockerignore, combining the underlying `Gitignore` matcher with the
/// information needed to decide whether an excluded directory may still contain a
/// re-included (negated) descendant, so we can safely prune directories during the walk.
#[derive(Debug)]
pub struct DockerIgnore {
    matcher: Gitignore,
    /// The literal leading path of every negation ("!") rule, i.e. the part before the
    /// first glob segment. A negation can only re-include paths that start with this
    /// prefix, so a directory whose subtree does not intersect any prefix can be pruned.
    negation_prefixes: Vec<PathBuf>,
    /// True if any negation rule begins with a glob segment (e.g. `**/foo`, `*.rs`).
    /// Such a rule can match at any depth, so no directory can ever be pruned.
    unbounded_negation: bool,
}

impl DockerIgnore {
    /// An empty matcher that ignores nothing.
    pub fn empty() -> Self {
        DockerIgnore {
            matcher: Gitignore::empty(),
            negation_prefixes: Vec::new(),
            unbounded_negation: false,
        }
    }

    /// Returns whether the given path is matched, inheriting exclusions from parent
    /// directories so that a file inside an excluded directory is also excluded unless
    /// re-included by a negation rule.
    pub fn matched(&self, path: &Path, is_dir: bool) -> Match<&ignore::gitignore::Glob> {
        self.matcher.matched_path_or_any_parents(path, is_dir)
    }

    /// Returns true if `dir` (an excluded directory, relative to the context root) can be
    /// pruned from the walk because no negation rule could re-include anything beneath it.
    pub fn can_skip_dir(&self, dir: &Path) -> bool {
        if self.unbounded_negation {
            return false;
        }
        // A negation can affect `dir`'s subtree only if its literal prefix and `dir` are
        // in an ancestor/descendant (or equal) relationship.
        !self
            .negation_prefixes
            .iter()
            .any(|n| n.starts_with(dir) || dir.starts_with(n))
    }
}

/// The set of characters that introduce glob/wildcard matching in a pattern segment.
const GLOB_CHARS: [char; 6] = ['*', '?', '[', ']', '{', '}'];

/// Extracts the literal leading path of a pattern, i.e. the path made up of the segments
/// before the first segment that contains a glob character. Returns `None` if the very
/// first segment is a glob (so the pattern is unanchored and can match at any depth).
fn literal_prefix(pattern: &str) -> Option<PathBuf> {
    let mut prefix = PathBuf::new();
    for seg in pattern.trim_start_matches('/').split('/') {
        if seg.is_empty() {
            continue;
        }
        if seg.contains(GLOB_CHARS) {
            break;
        }
        prefix.push(seg);
    }
    if prefix.as_os_str().is_empty() {
        None
    } else {
        Some(prefix)
    }
}

/// Compiles a .dockerignore file into a [`DockerIgnore`], anchored to the build context root.
fn compile_dockerignore(context_root: &Path, dockerignore_path: &Path) -> Result<DockerIgnore> {
    // 1. Initialize the builder anchored to your build context root
    let mut builder = GitignoreBuilder::new(context_root);
    let mut negation_prefixes = Vec::new();
    let mut unbounded_negation = false;

    // 2. Parse and normalize Docker rules into Git-styled anchored rules
    let file = File::open(dockerignore_path).with_context(|| {
        format!(
            "Failed to open .dockerignore at {}",
            dockerignore_path.display()
        )
    })?;
    let reader = BufReader::new(file);

    for line in reader.lines() {
        let line = line.with_context(|| {
            format!(
                "Failed to read line in .dockerignore at {}",
                dockerignore_path.display()
            )
        })?;
        let trimmed = line.trim();

        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        let mut is_negation = false;
        let mut pattern = trimmed;

        if pattern.starts_with('!') {
            is_negation = true;
            pattern = pattern[1..].trim();
        }

        // Docker runs filepath.Clean on every pattern, while the `ignore` crate
        // follows Git semantics. Normalize the two differences that matter:
        //   * a leading "./" is stripped ("./foo" -> "foo")
        //   * a trailing "/" is stripped ("build/" -> "build"), so the rule
        //     matches both a directory and a plain file of that name, like Docker.
        let pattern = pattern.strip_prefix("./").unwrap_or(pattern);
        let pattern = pattern.strip_suffix('/').unwrap_or(pattern);
        if pattern.is_empty() || pattern == "." {
            continue;
        }

        // Record the reach of negation rules so we know which directories must still be
        // walked (because a descendant could be re-included) and which can be pruned.
        if is_negation {
            match literal_prefix(pattern) {
                Some(prefix) => negation_prefixes.push(prefix),
                None => unbounded_negation = true,
            }
        }

        // Docker matches are inherently relative to the context root.
        // We prepend '/' to unanchored patterns so the `ignore` crate treats them as rooted.
        let anchored_pattern = if pattern.starts_with('/') || pattern.starts_with("**") {
            pattern.to_string()
        } else {
            format!("/{}", pattern)
        };

        // Re-apply negation token if it was stripped
        let finalized_rule = if is_negation {
            format!("!{}", anchored_pattern)
        } else {
            anchored_pattern
        };

        // Feed the line into the NFA compiler
        builder.add_line(None, &finalized_rule).with_context(|| {
            format!(
                "Failed to add rule '{}' from .dockerignore at {}",
                finalized_rule,
                dockerignore_path.display()
            )
        })?;
    }

    let matcher = builder.build()?;
    Ok(DockerIgnore {
        matcher,
        negation_prefixes,
        unbounded_negation,
    })
}

/// Loads .dockerignore rules by searching from the Dockerfile's directory up to the context root.
pub fn load_gitignore(context_root: &Path, docker_file: &Path) -> Result<DockerIgnore> {
    let dockerignore_path = docker_file.with_file_name(".dockerignore");
    if dockerignore_path.is_file() {
        return compile_dockerignore(context_root, &dockerignore_path);
    }
    let mut p = context_root.to_path_buf();
    loop {
        p.push(".dockerignore");
        if p.is_file() {
            return compile_dockerignore(context_root, &p);
        }
        if !p.pop() || !p.pop() {
            break;
        }
    }
    Ok(DockerIgnore::empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_prefix_extraction() {
        assert_eq!(
            literal_prefix("a/b/c.txt"),
            Some(PathBuf::from("a/b/c.txt"))
        );
        assert_eq!(literal_prefix("a/b/*.txt"), Some(PathBuf::from("a/b")));
        assert_eq!(literal_prefix("/a/b"), Some(PathBuf::from("a/b")));
        // A leading glob segment means the rule can match anywhere.
        assert_eq!(literal_prefix("**/foo"), None);
        assert_eq!(literal_prefix("*.log"), None);
    }

    /// A directory can be pruned only when no negation can reach into its subtree.
    fn ignore_with(prefixes: &[&str], unbounded: bool) -> DockerIgnore {
        DockerIgnore {
            matcher: Gitignore::empty(),
            negation_prefixes: prefixes.iter().map(PathBuf::from).collect(),
            unbounded_negation: unbounded,
        }
    }

    #[test]
    fn skips_directories_unrelated_to_negations() {
        // The negations from a real monorepo .dockerignore, all literal paths.
        let di = ignore_with(
            &[
                "terrastream/3rdparty/mdal/tests/data/dhi/OresundHD.dfsu",
                "rustweb/build.rs",
                "tileserver/build.rs",
                "dbdata/deploy",
            ],
            false,
        );

        // Excluded directories elsewhere are still pruned despite the negations existing.
        assert!(di.can_skip_dir(Path::new("python-web/node_modules")));
        assert!(di.can_skip_dir(Path::new("terrastream/3rdparty/mdal/build")));
        assert!(di.can_skip_dir(Path::new("rustweb/target")));

        // Ancestors of a negated path must be walked into.
        assert!(!di.can_skip_dir(Path::new("terrastream/3rdparty/mdal/tests")));
        assert!(!di.can_skip_dir(Path::new("terrastream/3rdparty/mdal/tests/data/dhi")));
        assert!(!di.can_skip_dir(Path::new("dbdata")));
    }

    #[test]
    fn unbounded_negation_disables_all_pruning() {
        let di = ignore_with(&["a/b"], true);
        assert!(!di.can_skip_dir(Path::new("anything/at/all")));
    }
}
